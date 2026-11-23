//! Keyword probe on an account's first connection: sets a hidden keyword
//! on the newest INBOX message, reads it back on another connection and
//! clears it again. A server that answers OK but drops the keyword cannot
//! keep labels. One keyword cannot reveal a per-account keyword limit
//! (Stalwart keeps about 98); the executor reports that when a label op
//! is dropped.

use tagmail_core::label::PROBE_KEYWORD;
use tagmail_imap::{
    Connector, FetchSpec, Flag, ImapSession, Pool, Pooled, SearchCriteria, SelectOptions, StoreOp,
    Uid, UidSet,
};

use crate::error::Result;

/// `account.keyword_probe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeState {
    Ok,
    Dropped,
    Unsupported,
    /// INBOX was empty; probed again at the next start.
    Skipped,
}

impl ProbeState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProbeState::Ok => "ok",
            ProbeState::Dropped => "dropped",
            ProbeState::Unsupported => "unsupported",
            ProbeState::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<ProbeState> {
        [
            ProbeState::Ok,
            ProbeState::Dropped,
            ProbeState::Unsupported,
            ProbeState::Skipped,
        ]
        .into_iter()
        .find(|p| p.as_str() == s)
    }

    /// Whether the account still has to be probed.
    pub(crate) fn needed(state: Option<ProbeState>) -> bool {
        matches!(state, None | Some(ProbeState::Skipped))
    }
}

fn probe_flag() -> [Flag; 1] {
    [Flag::Keyword(PROBE_KEYWORD.to_owned())]
}

/// Sets the probe keyword on the newest INBOX message; the state when
/// there is nothing to probe.
async fn mark<S: ImapSession>(s: &mut S) -> Result<Result<Uid, ProbeState>> {
    let sel = s.select("INBOX", SelectOptions::default()).await?;
    if !sel.keyword_capable {
        return Ok(Err(ProbeState::Unsupported));
    }
    if sel.exists == 0 {
        return Ok(Err(ProbeState::Skipped));
    }
    let Some(newest) = s.uid_search(SearchCriteria::All).await?.max() else {
        return Ok(Err(ProbeState::Skipped));
    };
    let mut uids = UidSet::new();
    uids.insert(newest);
    s.store_flags(&uids, StoreOp::Add, &probe_flag(), None)
        .await?;
    Ok(Ok(newest))
}

/// Whether the message kept the keyword; `None` when it is gone.
async fn has_keyword<S: ImapSession>(s: &mut S, uid: Uid) -> Result<Option<bool>> {
    let read_only = SelectOptions {
        read_only: true,
        qresync: None,
    };
    s.select("INBOX", read_only).await?;
    let mut uids = UidSet::new();
    uids.insert(uid);
    let metas = s.fetch(&uids, &FetchSpec::flags()).await?;
    let Some(flags) = metas
        .iter()
        .find(|m| m.uid == Some(uid))
        .and_then(|m| m.flags.as_ref())
    else {
        return Ok(None);
    };
    Ok(Some(flags.iter().any(
        |f| matches!(f, Flag::Keyword(k) if k.eq_ignore_ascii_case(PROBE_KEYWORD)),
    )))
}

async fn unmark<S: ImapSession>(s: &mut S, uid: Uid) -> Result<()> {
    s.select("INBOX", SelectOptions::default()).await?;
    let mut uids = UidSet::new();
    uids.insert(uid);
    s.store_flags(&uids, StoreOp::Remove, &probe_flag(), None)
        .await?;
    Ok(())
}

/// Clears the probe keyword on a fresh connection when dropped armed, so a
/// probe that failed or was cancelled after marking does not leave it.
struct Unmark<C: Connector> {
    pool: Pool<C>,
    uid: Option<Uid>,
}

impl<C: Connector> Drop for Unmark<C> {
    fn drop(&mut self) {
        let Some(uid) = self.uid.take() else {
            return;
        };
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let cleared = async { unmark(&mut *pool.acquire().await?, uid).await }.await;
            if let Err(e) = cleared {
                tracing::warn!(error = %e, "cannot clear the probe keyword");
            }
        });
    }
}

/// A non-fatal failure ends the probe without a result; the next start
/// tries again.
fn inconclusive<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_fatal() => Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "keyword probe failed");
            Ok(None)
        }
    }
}

/// Probes with the account's worker, reading back on a second pooled
/// connection, or on a reconnected worker when the account has only one.
/// Returns the worker to go on with and the result, if any.
pub(crate) async fn run<C: Connector>(
    pool: &Pool<C>,
    mut worker: Pooled<C>,
) -> Result<(Pooled<C>, Option<ProbeState>)> {
    let uid = match inconclusive(mark(&mut *worker).await)? {
        Some(Ok(uid)) => uid,
        Some(Err(state)) => return Ok((worker, Some(state))),
        None => return Ok((worker, None)),
    };
    let mut armed = Unmark {
        pool: pool.clone(),
        uid: Some(uid),
    };
    let kept = if pool.max_connections() >= 2 {
        let mut reader = pool.acquire().await?;
        has_keyword(&mut *reader, uid).await
    } else {
        worker.discard();
        worker = pool.acquire().await?;
        has_keyword(&mut *worker, uid).await
    };
    let kept = inconclusive(kept)?.flatten();
    let cleared = unmark(&mut *worker, uid).await;
    if cleared.is_ok() {
        armed.uid = None;
    }
    inconclusive(cleared)?;
    let state = kept.map(|kept| {
        if kept {
            ProbeState::Ok
        } else {
            ProbeState::Dropped
        }
    });
    Ok((worker, state))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tagmail_imap::PoolConfig;
    use tokio::time::Instant;

    use super::*;
    use crate::fake::{FakeConnector, FakeMsg, FetchFailure, Shared, lock, server};

    const CAPS: &[&str] = &["IMAP4rev1", "CONDSTORE"];

    fn seeded() -> Shared {
        let srv = server(CAPS);
        lock(&srv).append("INBOX", FakeMsg::new("p@x", "Hi", "a@x", "b@x", 1));
        srv
    }

    fn pool(srv: &Shared, max_connections: usize) -> Pool<FakeConnector> {
        Pool::new(
            FakeConnector(srv.clone()),
            PoolConfig {
                max_connections,
                ..PoolConfig::default()
            },
        )
    }

    async fn probe(srv: &Shared, max_connections: usize) -> Result<Option<ProbeState>> {
        let pool = pool(srv, max_connections);
        let worker = pool.acquire().await?;
        run(&pool, worker).await.map(|(_, state)| state)
    }

    async fn cleared(srv: &Shared) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(srv).flags_of("INBOX", 1).is_empty() {
            assert!(Instant::now() < deadline, "probe keyword left behind");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_message_gone_before_the_read_back_is_inconclusive() {
        for max_connections in [2, 1] {
            let srv = seeded();
            lock(&srv).expunge_after_keyword_store = true;
            assert_eq!(probe(&srv, max_connections).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn a_failed_read_back_still_clears_the_keyword() {
        for max_connections in [2, 1] {
            let srv = seeded();
            lock(&srv).fail_flag_fetches = Some(FetchFailure::No);
            assert_eq!(probe(&srv, max_connections).await.unwrap(), None);
            cleared(&srv).await;

            let srv = seeded();
            lock(&srv).fail_flag_fetches = Some(FetchFailure::Closed);
            assert!(probe(&srv, max_connections).await.is_err());
            cleared(&srv).await;
        }
    }

    #[tokio::test]
    async fn kept_and_dropped_keywords_are_told_apart() {
        let srv = seeded();
        assert_eq!(probe(&srv, 2).await.unwrap(), Some(ProbeState::Ok));
        cleared(&srv).await;
        lock(&srv).drop_keywords = true;
        assert_eq!(probe(&srv, 1).await.unwrap(), Some(ProbeState::Dropped));
    }
}
