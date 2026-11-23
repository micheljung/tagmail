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

async fn has_keyword<S: ImapSession>(s: &mut S, uid: Uid) -> Result<bool> {
    let read_only = SelectOptions {
        read_only: true,
        qresync: None,
    };
    s.select("INBOX", read_only).await?;
    let mut uids = UidSet::new();
    uids.insert(uid);
    let metas = s.fetch(&uids, &FetchSpec::flags()).await?;
    Ok(metas
        .iter()
        .filter_map(|m| m.flags.as_ref())
        .flatten()
        .any(|f| matches!(f, Flag::Keyword(k) if k.eq_ignore_ascii_case(PROBE_KEYWORD))))
}

async fn unmark<S: ImapSession>(s: &mut S, uid: Uid) -> Result<()> {
    s.select("INBOX", SelectOptions::default()).await?;
    let mut uids = UidSet::new();
    uids.insert(uid);
    s.store_flags(&uids, StoreOp::Remove, &probe_flag(), None)
        .await?;
    Ok(())
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
    let kept = if pool.max_connections() >= 2 {
        let mut reader = pool.acquire().await?;
        has_keyword(&mut *reader, uid).await
    } else {
        worker.discard();
        worker = pool.acquire().await?;
        has_keyword(&mut *worker, uid).await
    };
    let Some(kept) = inconclusive(kept)? else {
        return Ok((worker, None));
    };
    inconclusive(unmark(&mut *worker, uid).await)?;
    let state = if kept {
        ProbeState::Ok
    } else {
        ProbeState::Dropped
    };
    Ok((worker, Some(state)))
}
