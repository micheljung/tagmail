//! The compose runner (plan §3.5): one task that saves edited drafts to the
//! IMAP Drafts mailbox (replacing the previous version), expunges replaced
//! or deleted draft copies, sends the outbox through SMTP and puts a copy
//! in Sent, and removes orphaned uploads.
//!
//! Delivery is at-least-once: an item is marked `sending` before SMTP
//! DATA. If the server stops between the SMTP server accepting the
//! message and the outbox commit, the item is found in `sending` on
//! restart; Sent is searched for its Message-ID first and the message is
//! sent again only if it is not there. When the SMTP server does not save
//! Sent itself, a crash in that window sends the message twice.
//!
//! `account.server_saves_sent` decides the Sent copy: `1` (preset for
//! Gmail, or learned) means the server files it; unknown means Sent is
//! watched for the Message-ID for [`crate::EngineConfig::sent_probe`]
//! after the first send and the answer is remembered; `0` means it is
//! APPENDed unless Sent already has it (so a retried copy is never a
//! duplicate).

use std::sync::Arc;
use std::time::Duration;

use tagmail_api_core::compose::ComposeResult;
use tagmail_core::{AccountId, SpecialUse};
use tagmail_imap::{Flag, ImapSession, SearchCriteria, SelectOptions, UidSet};
use tagmail_store::rusqlite::{self, OptionalExtension, params};
use tokio::sync::watch;
use tokio::time::Instant;

use tagmail_api_core::compose_dto::OutboxStateDto;
use tagmail_api_core::dto::AddressDto;
use tagmail_store::{ChangeOp, Entity, NewChange, changes};

use crate::compose::{
    SavedAt, UPLOAD_TMP, delete_draft_row, draft_build, draft_row, now, outgoing, purge_later,
    read, remove_upload_files, state_of, upload_path, write,
};
use crate::error::{Error, Result};
use crate::jobs::Priority;
use crate::outgoing::Purpose;
use crate::smtp;
use crate::supervisor::{ConnectorFactory, Engine};

/// Longest sleep of the runner when nothing is due.
const IDLE_WAIT: Duration = Duration::from_secs(60);
/// How often orphaned uploads are looked for.
const CLEANUP_EVERY: Duration = Duration::from_secs(60 * 60);
/// Pause between searches of Sent while probing `server_saves_sent`.
const PROBE_STEP: Duration = Duration::from_secs(1);

/// The mailbox with `special_use`, else the one named `fallback`
/// (case-insensitive), else a new one named `fallback`.
pub(crate) async fn special_mailbox<S: ImapSession>(
    s: &mut S,
    special_use: SpecialUse,
    fallback: &str,
) -> Result<String> {
    let list = s.list().await?;
    let found = list
        .iter()
        .find(|m| m.selectable && m.special_use == Some(special_use))
        .or_else(|| {
            list.iter()
                .find(|m| m.selectable && m.name.eq_ignore_ascii_case(fallback))
        });
    if let Some(m) = found {
        return Ok(m.name.clone());
    }
    s.create_mailbox(fallback).await?;
    Ok(fallback.to_owned())
}

/// UIDs in `mailbox` whose Message-ID is `msgid`.
async fn find_message<S: ImapSession>(s: &mut S, mailbox: &str, msgid: &str) -> Result<UidSet> {
    let read_only = SelectOptions {
        read_only: true,
        qresync: None,
    };
    s.select(mailbox, read_only).await?;
    Ok(s.uid_search(SearchCriteria::Header {
        name: "Message-ID".to_owned(),
        value: format!("<{msgid}>"),
    })
    .await?)
}

/// APPENDs a draft version to Drafts; the previous version is expunged
/// separately (`draft_purge`).
async fn save_draft<S: ImapSession>(s: &mut S, mime: &[u8], msgid: &str) -> Result<SavedAt> {
    let mailbox = special_mailbox(s, SpecialUse::Drafts, "Drafts").await?;
    let appended = s.append(&mailbox, &[Flag::Draft, Flag::Seen], mime).await?;
    let uid = match appended.uid {
        Some(uid) => uid.0,
        // Without UIDPLUS: the newest copy with this Message-ID.
        None => find_message(s, &mailbox, msgid)
            .await?
            .iter()
            .map(|u| u.0)
            .max()
            .ok_or_else(|| Error::Unavailable("the saved draft is not in Drafts".to_owned()))?,
    };
    let uidvalidity = match appended.uid_validity {
        Some(v) => v.0,
        None => {
            s.status(&mailbox)
                .await?
                .uid_validity
                .ok_or_else(|| Error::Unavailable("no UIDVALIDITY for Drafts".to_owned()))?
                .0
        }
    };
    Ok(SavedAt {
        mailbox,
        uidvalidity,
        uid,
    })
}

/// Expunges a saved draft copy; done when it is already gone.
async fn purge<S: ImapSession>(s: &mut S, at: &SavedAt) -> Result<()> {
    let selected = match s.select(&at.mailbox, SelectOptions::default()).await {
        Ok(selected) => selected,
        Err(tagmail_imap::Error::No { .. }) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if selected.uid_validity.0 != at.uidvalidity {
        return Ok(());
    }
    let mut uids = UidSet::new();
    uids.insert_range(at.uid, at.uid);
    Ok(s.expunge_uids(&uids).await?)
}

/// Whether Sent has the message.
async fn in_sent<S: ImapSession>(s: &mut S, msgid: &str) -> Result<bool> {
    let sent = special_mailbox(s, SpecialUse::Sent, "Sent").await?;
    Ok(!find_message(s, &sent, msgid).await?.is_empty())
}

/// Copies a sent message to Sent unless it is already there.
async fn copy_to_sent<S: ImapSession>(s: &mut S, mime: &[u8], msgid: &str) -> Result<()> {
    if in_sent(s, msgid).await? {
        return Ok(());
    }
    let sent = special_mailbox(s, SpecialUse::Sent, "Sent").await?;
    s.append(&sent, &[Flag::Seen], mime).await?;
    Ok(())
}

struct OutboxRow {
    id: i64,
    op_id: String,
    account: AccountId,
    state: String,
    msgid: String,
    sender: String,
    recipients: Vec<String>,
}

fn sql(e: rusqlite::Error) -> tagmail_api_core::compose::ComposeError {
    tagmail_api_core::compose::ComposeError::Internal(Box::new(tagmail_store::Error::from(e)))
}

/// Seconds to wait after `attempts` failures: `min` doubled per failure,
/// at most `max`.
fn backoff(min: Duration, max: Duration, attempts: u32) -> i64 {
    let wait = min.saturating_mul(1 << attempts.min(16)).min(max);
    i64::try_from(wait.as_secs().max(1)).unwrap_or(i64::MAX)
}

impl<F: ConnectorFactory> Engine<F> {
    fn backoff(&self, attempts: u32) -> i64 {
        let config = &self.inner.config;
        backoff(config.outbox_retry_min, config.outbox_retry_max, attempts)
    }

    /// Runs one runner pass; returns how long until something is due.
    async fn step(&self, last_cleanup: &mut Option<Instant>) -> ComposeResult<Duration> {
        if last_cleanup.is_none_or(|at| at.elapsed() >= CLEANUP_EVERY) {
            *last_cleanup = Some(Instant::now());
            self.cleanup().await?;
        }
        self.save_drafts().await?;
        self.purge_drafts().await?;
        self.send_outbox().await?;
        self.prune_sent().await?;
        self.next_due().await
    }

    async fn next_due(&self) -> ComposeResult<Duration> {
        let debounce = i64::try_from(self.inner.config.draft_debounce.as_secs()).unwrap_or(0);
        let next: Option<i64> = read(&self.inner.stores, move |c| {
            c.query_row(
                "SELECT min(t) FROM ( \
                   SELECT coalesce(next_save_at, updated_at + ?1) AS t FROM draft \
                     WHERE queued = 0 AND saved_version < version \
                   UNION ALL SELECT coalesce(next_at, 0) FROM draft_purge \
                   UNION ALL SELECT coalesce(next_at, 0) FROM outbox \
                     WHERE state IN ('queued', 'sending') \
                       OR (state = 'sent' AND copied_at IS NULL))",
                [debounce],
                |r| r.get(0),
            )
            .map_err(sql)
        })
        .await?;
        Ok(match next {
            None => IDLE_WAIT,
            Some(at) => {
                Duration::from_secs(u64::try_from(at - now()).unwrap_or(0).max(1)).min(IDLE_WAIT)
            }
        })
    }

    async fn save_drafts(&self) -> ComposeResult<()> {
        let debounce = i64::try_from(self.inner.config.draft_debounce.as_secs()).unwrap_or(0);
        let now = now();
        let due: Vec<i64> = read(&self.inner.stores, move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id FROM draft WHERE queued = 0 AND saved_version < version \
                     AND updated_at + ?1 <= ?2 AND coalesce(next_save_at, 0) <= ?2 ORDER BY id",
                )
                .map_err(sql)?;
            stmt.query_map([debounce, now], |r| r.get(0))
                .map_err(sql)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql)
        })
        .await?;
        for id in due {
            self.save_draft(id).await?;
        }
        Ok(())
    }

    async fn save_draft(&self, id: i64) -> ComposeResult<()> {
        let Some(build) = write(&self.inner.stores, move |tx| draft_build(tx, id)).await? else {
            return Ok(());
        };
        let (account, version, msgid) = (
            AccountId(build.row.account_id),
            build.row.version,
            build.row.msgid_hdr.clone(),
        );
        let dir = self.inner.stores.uploads.clone();
        let mime = tokio::task::spawn_blocking(move || {
            outgoing(&dir, build.from.clone(), &build.row, &build.files, now())
                .map(|m| m.build(Purpose::Draft))
        })
        .await
        .map_err(|e| tagmail_api_core::compose::ComposeError::Internal(Box::new(e)))?;
        let saved = match mime {
            Ok(mime) => {
                self.run_job(account, Priority::Write, move |s, _| {
                    Box::pin(async move { save_draft(s, &mime, &msgid).await })
                })
                .await
            }
            Err(e) => Err(Error::Store(e.into())),
        };
        let (min, max) = (
            self.inner.config.outbox_retry_min,
            self.inner.config.outbox_retry_max,
        );
        write(&self.inner.stores, move |tx| {
            let row = draft_row(tx, id)?;
            match (saved, row) {
                (Ok(at), None) => purge_later(tx, account.0, &at),
                (Ok(at), Some(row)) if row.account_id != account.0 => {
                    purge_later(tx, account.0, &at)
                }
                (Ok(at), Some(row)) => {
                    if let Some(old) = row.saved.as_ref().filter(|old| **old != at) {
                        purge_later(tx, row.account_id, old)?;
                    }
                    tx.execute(
                        "UPDATE draft SET saved_mailbox = ?2, saved_uidvalidity = ?3, \
                         saved_uid = ?4, saved_version = ?5, save_error = NULL, \
                         save_attempts = 0, next_save_at = NULL WHERE id = ?1",
                        params![id, at.mailbox, at.uidvalidity, at.uid, version],
                    )
                    .map_err(sql)?;
                    Ok(())
                }
                (Err(_), None) => Ok(()),
                (Err(e), Some(_)) => {
                    tracing::warn!(draft = id, error = %e, "saving a draft to IMAP failed");
                    let attempts: u32 = tx
                        .query_row("SELECT save_attempts FROM draft WHERE id = ?1", [id], |r| {
                            r.get(0)
                        })
                        .map_err(sql)?;
                    tx.execute(
                        "UPDATE draft SET save_error = ?2, save_attempts = save_attempts + 1, \
                         next_save_at = ?3 WHERE id = ?1",
                        params![id, e.to_string(), now() + backoff(min, max, attempts)],
                    )
                    .map_err(sql)?;
                    Ok(())
                }
            }
        })
        .await
    }

    async fn purge_drafts(&self) -> ComposeResult<()> {
        let now = now();
        let due: Vec<(i64, i64, SavedAt, u32)> = read(&self.inner.stores, move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, account_id, mailbox, uidvalidity, uid, attempts FROM draft_purge \
                     WHERE coalesce(next_at, 0) <= ?1 ORDER BY id",
                )
                .map_err(sql)?;
            stmt.query_map([now], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    SavedAt {
                        mailbox: r.get(2)?,
                        uidvalidity: r.get(3)?,
                        uid: r.get(4)?,
                    },
                    r.get(5)?,
                ))
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql)
        })
        .await?;
        for (id, account, at, attempts) in due {
            let result = self
                .run_job(AccountId(account), Priority::Background, move |s, _| {
                    Box::pin(async move { purge(s, &at).await })
                })
                .await;
            let wait = self.backoff(attempts);
            write(&self.inner.stores, move |tx| {
                match result {
                    Ok(()) => tx.execute("DELETE FROM draft_purge WHERE id = ?1", [id]),
                    Err(e) => {
                        tracing::debug!(purge = id, error = %e, "expunging a draft copy failed");
                        tx.execute(
                            "UPDATE draft_purge SET attempts = attempts + 1, next_at = ?2 \
                             WHERE id = ?1",
                            params![id, now + wait],
                        )
                    }
                }
                .map_err(sql)?;
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    async fn send_outbox(&self) -> ComposeResult<()> {
        let now = now();
        let due: Vec<OutboxRow> = read(&self.inner.stores, move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT op_id, account_id, state, msgid_hdr, sender, recipients, id \
                     FROM outbox WHERE (state IN ('queued', 'sending') \
                       OR (state = 'sent' AND copied_at IS NULL)) \
                     AND coalesce(next_at, 0) <= ?1 ORDER BY id",
                )
                .map_err(sql)?;
            stmt.query_map([now], |r| {
                let recipients: String = r.get(5)?;
                Ok(OutboxRow {
                    op_id: r.get(0)?,
                    account: AccountId(r.get(1)?),
                    state: r.get(2)?,
                    msgid: r.get(3)?,
                    sender: r.get(4)?,
                    recipients: serde_json::from_str::<Vec<AddressDto>>(&recipients)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| a.email)
                        .collect(),
                    id: r.get(6)?,
                })
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql)
        })
        .await?;
        for row in due {
            match row.state.as_str() {
                "sending" => self.recover(&row).await?,
                "queued" => self.submit(&row).await?,
                _ => self.copy(&row).await?,
            }
        }
        Ok(())
    }

    /// An item left in `sending` by a crash: resend only if Sent lacks it.
    async fn recover(&self, row: &OutboxRow) -> ComposeResult<()> {
        let msgid = row.msgid.clone();
        let found = self
            .run_job(row.account, Priority::Write, move |s, _| {
                Box::pin(async move { in_sent(s, &msgid).await })
            })
            .await;
        match found {
            Ok(true) => {
                tracing::info!(op = %row.op_id, "found in Sent after a restart; not resending");
                self.finish(row).await
            }
            Ok(false) => self.submit(row).await,
            Err(e) => self.defer(row, &format!("checking Sent: {e}")).await,
        }
    }

    /// Leaves the item in its state and tries again later.
    async fn defer(&self, row: &OutboxRow, error: &str) -> ComposeResult<()> {
        let op_id = row.op_id.clone();
        let error = error.to_owned();
        let attempts: u32 = read(&self.inner.stores, {
            let op_id = op_id.clone();
            move |c| {
                c.query_row(
                    "SELECT attempts FROM outbox WHERE op_id = ?1",
                    [op_id],
                    |r| r.get(0),
                )
                .map_err(sql)
            }
        })
        .await?;
        let next = now() + self.backoff(attempts);
        let message = error.clone();
        write(&self.inner.stores, move |tx| {
            tx.execute(
                "UPDATE outbox SET error = ?2, next_at = ?3 WHERE op_id = ?1",
                params![op_id, error, next],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await?;
        self.emit_outbox(row.id, Some((state_of(&row.state), Some(message))))
            .await;
        Ok(())
    }

    async fn submit(&self, row: &OutboxRow) -> ComposeResult<()> {
        let op_id = row.op_id.clone();
        let (attempts, mime): (u32, Vec<u8>) = write(&self.inner.stores, move |tx| {
            tx.execute(
                "UPDATE outbox SET state = 'sending', attempts = attempts + 1, next_at = NULL \
                 WHERE op_id = ?1",
                [&op_id],
            )
            .map_err(sql)?;
            tx.query_row(
                "SELECT attempts, mime_blob FROM outbox WHERE op_id = ?1",
                [&op_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql)
        })
        .await?;
        self.emit_outbox(row.id, Some((OutboxStateDto::Sending, None)))
            .await;
        let sent = match smtp::load_server(&self.inner.stores, row.account).await {
            Ok(server) => {
                self.inner
                    .smtp
                    .send(
                        server,
                        row.sender.clone(),
                        row.recipients.clone(),
                        Arc::new(mime),
                    )
                    .await
            }
            Err(e) => Err(smtp::SmtpError {
                permanent: true,
                message: e.to_string(),
            }),
        };
        let op_id = row.op_id.clone();
        let max = self.inner.config.max_send_attempts;
        let next = now() + self.backoff(attempts.saturating_sub(1));
        let failed = sent.as_ref().err().map(|e| e.permanent || attempts >= max);
        let outcome = sent.clone();
        write(&self.inner.stores, move |tx| {
            match (&outcome, failed) {
                (Ok(()), _) => tx.execute(
                    "UPDATE outbox SET state = 'sent', sent_at = ?2, error = NULL, \
                     next_at = NULL WHERE op_id = ?1",
                    params![op_id, now()],
                ),
                (Err(e), Some(true)) => tx.execute(
                    "UPDATE outbox SET state = 'failed', error = ?2 WHERE op_id = ?1",
                    params![op_id, e.message],
                ),
                (Err(e), _) => tx.execute(
                    "UPDATE outbox SET state = 'queued', error = ?2, next_at = ?3 \
                     WHERE op_id = ?1",
                    params![op_id, e.message, next],
                ),
            }
            .map_err(sql)?;
            Ok(())
        })
        .await?;
        let event = match (&sent, failed) {
            (Ok(()), _) => (OutboxStateDto::Sent, None),
            (Err(e), Some(true)) => (OutboxStateDto::Failed, Some(e.message.clone())),
            (Err(e), _) => (OutboxStateDto::Queued, Some(e.message.clone())),
        };
        self.emit_outbox(row.id, Some(event)).await;
        match sent {
            Ok(()) => self.copy(row).await,
            Err(e) => {
                tracing::warn!(op = %row.op_id, error = %e.message, "SMTP submission failed");
                Ok(())
            }
        }
    }

    /// Makes sure Sent has the message, then completes the item.
    async fn copy(&self, row: &OutboxRow) -> ComposeResult<()> {
        let account = row.account;
        let saves: Option<bool> = read(&self.inner.stores, move |c| {
            c.query_row(
                "SELECT server_saves_sent FROM account WHERE id = ?1",
                [account.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)
            .map(Option::flatten)
        })
        .await?;
        if saves == Some(true) {
            return self.finish(row).await;
        }
        if saves.is_none() {
            let deadline = Instant::now() + self.inner.config.sent_probe;
            loop {
                let msgid = row.msgid.clone();
                let found = self
                    .run_job(account, Priority::Write, move |s, _| {
                        Box::pin(async move { in_sent(s, &msgid).await })
                    })
                    .await;
                match found {
                    Ok(true) => {
                        self.remember_saves_sent(account, true).await?;
                        return self.finish(row).await;
                    }
                    Ok(false) if Instant::now() < deadline => {
                        tokio::time::sleep(PROBE_STEP).await;
                    }
                    Ok(false) => break,
                    Err(e) => return self.defer(row, &format!("Sent copy: {e}")).await,
                }
            }
        }
        let op_id = row.op_id.clone();
        let mime: Vec<u8> = read(&self.inner.stores, move |c| {
            c.query_row(
                "SELECT mime_blob FROM outbox WHERE op_id = ?1",
                [op_id],
                |r| r.get(0),
            )
            .map_err(sql)
        })
        .await?;
        let msgid = row.msgid.clone();
        let copied = self
            .run_job(account, Priority::Write, move |s, _| {
                Box::pin(async move { copy_to_sent(s, &mime, &msgid).await })
            })
            .await;
        match copied {
            Ok(()) => {
                if saves.is_none() {
                    self.remember_saves_sent(account, false).await?;
                }
                self.finish(row).await
            }
            Err(e) => self.defer(row, &format!("Sent copy: {e}")).await,
        }
    }

    async fn remember_saves_sent(&self, account: AccountId, saves: bool) -> ComposeResult<()> {
        tracing::info!(
            account = account.0,
            saves,
            "learned whether the server saves Sent"
        );
        write(&self.inner.stores, move |tx| {
            tx.execute(
                "UPDATE account SET server_saves_sent = ?2 WHERE id = ?1",
                params![account.0, saves],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Completes the item and removes its draft; the Sent copy threads with
    /// the conversation on the next poll, which is requested now. The item
    /// stays listed for [`crate::EngineConfig::sent_keep`].
    async fn finish(&self, row: &OutboxRow) -> ComposeResult<()> {
        let op_id = row.op_id.clone();
        let files = write(&self.inner.stores, move |tx| {
            let draft: Option<i64> = tx
                .query_row(
                    "SELECT draft_id FROM outbox WHERE op_id = ?1",
                    [&op_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?
                .flatten();
            tx.execute(
                "UPDATE outbox SET state = 'sent', copied_at = ?2, mime_blob = X'', \
                 error = NULL, next_at = NULL WHERE op_id = ?1",
                params![op_id, now()],
            )
            .map_err(sql)?;
            match draft.map(|d| draft_row(tx, d)).transpose()?.flatten() {
                Some(draft) => delete_draft_row(tx, &draft),
                None => Ok(Vec::new()),
            }
        })
        .await?;
        remove_upload_files(&self.inner.stores.uploads, &files);
        tracing::info!(op = %row.op_id, "sent");
        self.emit_outbox(row.id, Some((OutboxStateDto::Sent, None)))
            .await;
        self.request_poll(row.account);
        Ok(())
    }

    /// Forgets completed items after `sent_keep`.
    async fn prune_sent(&self) -> ComposeResult<()> {
        let keep = i64::try_from(self.inner.config.sent_keep.as_secs()).unwrap_or(i64::MAX);
        let cutoff = now().saturating_sub(keep);
        let pruned: Vec<i64> = write(&self.inner.stores, move |tx| {
            let mut stmt = tx
                .prepare("DELETE FROM outbox WHERE copied_at <= ?1 RETURNING id")
                .map_err(sql)?;
            stmt.query_map([cutoff], |r| r.get(0))
                .map_err(sql)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql)
        })
        .await?;
        for id in pruned {
            self.emit_outbox(id, None).await;
        }
        Ok(())
    }

    /// Publishes an outbox change (`None`: removed) to the change feed.
    pub(crate) async fn emit_outbox(
        &self,
        id: i64,
        state: Option<(OutboxStateDto, Option<String>)>,
    ) {
        let (op, payload) = match state {
            Some((status, error)) => (
                ChangeOp::Upsert,
                Some(serde_json::json!({ "status": status, "error": error }).to_string()),
            ),
            None => (ChangeOp::Delete, None),
        };
        let change = NewChange {
            entity: Entity::Outbox,
            entity_id: id,
            op,
            payload,
        };
        let cache = &self.inner.stores.cache;
        let epoch = cache.epoch();
        let head = cache
            .write(move |tx| {
                changes::append(tx, &change)?;
                changes::head(tx, epoch)
            })
            .await;
        match head {
            Ok(head) => self.inner.stores.feed.publish(head),
            Err(e) => tracing::warn!(outbox = id, error = %e, "cannot publish an outbox change"),
        }
    }

    /// Drops uploads that never joined a draft (or lost it) after the grace
    /// period, files without a row, and stale unfinished uploads.
    async fn cleanup(&self) -> ComposeResult<()> {
        let grace = i64::try_from(self.inner.config.upload_grace.as_secs()).unwrap_or(i64::MAX);
        let cutoff = now().saturating_sub(grace);
        let (removed, known): (Vec<i64>, Vec<i64>) = write(&self.inner.stores, move |tx| {
            let removed: Vec<i64> = {
                let mut stmt = tx
                    .prepare(
                        "DELETE FROM upload WHERE draft_id IS NULL AND created_at < ?1 \
                         RETURNING id",
                    )
                    .map_err(sql)?;
                stmt.query_map([cutoff], |r| r.get(0))
                    .map_err(sql)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(sql)?
            };
            let mut stmt = tx.prepare("SELECT id FROM upload").map_err(sql)?;
            let known = stmt
                .query_map([], |r| r.get(0))
                .map_err(sql)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql)?;
            Ok((removed, known))
        })
        .await?;
        let dir = self.inner.stores.uploads.clone();
        let upload_grace = self.inner.config.upload_grace;
        tokio::task::spawn_blocking(move || {
            remove_upload_files(&dir, &removed);
            let stale = |path: &std::path::Path| {
                std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age >= upload_grace)
            };
            for sub in [dir.clone(), dir.join(UPLOAD_TMP)] {
                let Ok(entries) = std::fs::read_dir(&sub) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let orphan = match entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                        Some(id) if sub == dir => {
                            !known.contains(&id) && path == upload_path(&dir, id)
                        }
                        _ => true,
                    };
                    if orphan && stale(&path) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        })
        .await
        .map_err(|e| tagmail_api_core::compose::ComposeError::Internal(Box::new(e)))?;
        Ok(())
    }
}

/// The runner task; ends when `stop` turns true.
pub(crate) async fn run<F: ConnectorFactory>(engine: Engine<F>, mut stop: watch::Receiver<bool>) {
    let mut last_cleanup = None;
    loop {
        let wait = match engine.step(&mut last_cleanup).await {
            Ok(wait) => wait,
            Err(e) => {
                tracing::error!(error = %e, "compose runner pass failed");
                engine.inner.config.outbox_retry_min
            }
        };
        tokio::select! {
            () = engine.inner.outbox_wake.notified() => {}
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}
