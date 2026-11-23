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

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use tagmail_api_core::compose::ComposeResult;
use tagmail_api_core::compose_dto::OutboxStateDto;
use tagmail_api_core::dto::AddressDto;
use tagmail_core::{AccountId, SpecialUse};
use tagmail_imap::{Flag, ImapSession, SelectOptions, StoreOp, UidSet};
use tagmail_store::rusqlite::{self, OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, NewChange, changes};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::compose::{
    OUTBOX_TMP, SavedAt, UPLOAD_TMP, delete_draft_row, draft_build, draft_row, now, outgoing,
    purge_later, read, remove_upload_files, state_of, write, write_message,
};
use crate::error::{Error, Result};
use crate::jobs::Priority;
use crate::msgid_search;
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
    let sel = s.select(mailbox, read_only).await?;
    msgid_search::find(s, &sel, msgid).await
}

/// APPENDs a draft version to Drafts; the previous version is expunged
/// separately (`draft_purge`).
async fn save_draft<S: ImapSession>(s: &mut S, mime: &Path, msgid: &str) -> Result<SavedAt> {
    let mailbox = special_mailbox(s, SpecialUse::Drafts, "Drafts").await?;
    let appended = append_file(s, &mailbox, &[Flag::Draft, Flag::Seen], mime).await?;
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
async fn copy_to_sent<S: ImapSession>(s: &mut S, mime: &Path, msgid: &str) -> Result<()> {
    if in_sent(s, msgid).await? {
        return Ok(());
    }
    let sent = special_mailbox(s, SpecialUse::Sent, "Sent").await?;
    append_file(s, &sent, &[Flag::Seen], mime).await?;
    Ok(())
}

/// APPENDs a message file, streamed.
async fn append_file<S: ImapSession>(
    s: &mut S,
    mailbox: &str,
    flags: &[Flag],
    path: &Path,
) -> Result<tagmail_imap::AppendResult> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(tagmail_store::Error::from)?;
    let size = file
        .metadata()
        .await
        .map_err(tagmail_store::Error::from)?
        .len();
    Ok(s.append_stream(mailbox, flags, size, &mut file).await?)
}

/// Adds `flag` to every location of the original message.
async fn flag_original<S: ImapSession>(
    s: &mut S,
    locations: &[(String, u32)],
    flag: &Flag,
) -> Result<()> {
    let mut mailboxes: Vec<&str> = locations.iter().map(|(m, _)| m.as_str()).collect();
    mailboxes.dedup();
    for mailbox in mailboxes {
        let uids: UidSet = locations
            .iter()
            .filter(|(m, _)| m == mailbox)
            .map(|(_, u)| tagmail_imap::Uid(*u))
            .collect();
        s.select(mailbox, SelectOptions::default()).await?;
        s.store_flags(&uids, StoreOp::Add, std::slice::from_ref(flag), None)
            .await?;
    }
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
                     AND coalesce(next_save_at, updated_at + ?1) <= ?2 ORDER BY id",
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
        let message = outgoing(
            &self.inner.stores.uploads,
            build.from.clone(),
            &build.row,
            &build.files,
            now(),
        );
        let outbox = self.inner.stores.outbox.clone();
        let built =
            tokio::task::spawn_blocking(move || write_message(&outbox, &message, Purpose::Draft))
                .await
                .map_err(|e| tagmail_api_core::compose::ComposeError::Internal(Box::new(e)))?;
        let saved = match built {
            Ok((path, _)) => {
                let file = path.clone();
                let saved = self
                    .run_job(account, Priority::Write, move |s, _| {
                        Box::pin(async move { save_draft(s, &file, &msgid).await })
                    })
                    .await;
                let _ = tokio::fs::remove_file(path).await;
                saved
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
        let (attempts, file): (u32, Option<String>) = write(&self.inner.stores, move |tx| {
            tx.execute(
                "UPDATE outbox SET state = 'sending', attempts = attempts + 1, next_at = NULL \
                 WHERE op_id = ?1",
                [&op_id],
            )
            .map_err(sql)?;
            tx.query_row(
                "SELECT attempts, mime_file FROM outbox WHERE op_id = ?1",
                [&op_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql)
        })
        .await?;
        self.emit_outbox(row.id, Some((OutboxStateDto::Sending, None)))
            .await;
        let server = smtp::load_server(&self.inner.stores, row.account).await;
        let sent = match (server, file) {
            (Ok(server), Some(file)) => {
                self.inner
                    .smtp
                    .send(
                        server,
                        row.sender.clone(),
                        row.recipients.clone(),
                        self.inner.stores.outbox.join(file),
                    )
                    .await
            }
            (Err(e), _) => Err(smtp::SmtpError {
                permanent: true,
                message: e.to_string(),
            }),
            (_, None) => Err(smtp::SmtpError {
                permanent: true,
                message: "the queued message file is missing".to_owned(),
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
        let file: Option<String> = read(&self.inner.stores, move |c| {
            c.query_row(
                "SELECT mime_file FROM outbox WHERE op_id = ?1",
                [op_id],
                |r| r.get(0),
            )
            .map_err(sql)
        })
        .await?;
        let Some(file) = file else {
            return self
                .defer(row, "Sent copy: the message file is missing")
                .await;
        };
        let (msgid, path) = (row.msgid.clone(), self.inner.stores.outbox.join(file));
        let copied = self
            .run_job(account, Priority::Write, move |s, _| {
                Box::pin(async move { copy_to_sent(s, &path, &msgid).await })
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
        type Finished = (Vec<i64>, Option<String>, Option<(String, String)>);
        let (files, message, flag): Finished = write(&self.inner.stores, move |tx| {
            let (draft, message, flag_msgid, flag_kind): (
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = tx
                .query_row(
                    "SELECT draft_id, mime_file, flag_msgid, flag_kind FROM outbox \
                     WHERE op_id = ?1",
                    [&op_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map_err(sql)?;
            tx.execute(
                "UPDATE outbox SET state = 'sent', copied_at = ?2, mime_file = NULL, \
                 error = NULL, next_at = NULL WHERE op_id = ?1",
                params![op_id, now()],
            )
            .map_err(sql)?;
            let files = match draft.map(|d| draft_row(tx, d)).transpose()?.flatten() {
                Some(draft) => delete_draft_row(tx, &draft)?,
                None => Vec::new(),
            };
            Ok((files, message, flag_msgid.zip(flag_kind)))
        })
        .await?;
        remove_upload_files(&self.inner.stores.uploads, &files);
        if let Some(message) = message {
            let _ = tokio::fs::remove_file(self.inner.stores.outbox.join(message)).await;
        }
        if let Some((msgid, kind)) = flag {
            self.flag_original(row.account, msgid, &kind).await;
        }
        tracing::info!(op = %row.op_id, "sent");
        self.emit_outbox(row.id, Some((OutboxStateDto::Sent, None)))
            .await;
        self.request_poll(row.account);
        Ok(())
    }

    /// Marks the replied-to (`\Answered`) or forwarded (`$Forwarded`)
    /// message at all its locations. Best effort: a failure is logged and
    /// the flag stays unset.
    async fn flag_original(&self, account: AccountId, msgid: String, kind: &str) {
        let flag = match kind {
            "answered" => Flag::Answered,
            _ => Flag::Keyword("$Forwarded".to_owned()),
        };
        let locations = self
            .inner
            .stores
            .cache
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT mb.name, l.uid FROM message m \
                     JOIN location l ON l.message_id = m.id \
                     JOIN mailbox mb ON mb.id = l.mailbox_id \
                     WHERE m.account_id = ?1 AND m.msgid_hdr = ?2 AND m.tombstoned_at IS NULL \
                     ORDER BY mb.name, l.uid",
                )?;
                let rows = stmt
                    .query_map(params![account.0, msgid], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<(String, u32)>>>()?;
                Ok(rows)
            })
            .await;
        let result = match locations {
            Ok(locations) if locations.is_empty() => return,
            Ok(locations) => {
                self.run_job(account, Priority::Write, move |s, _| {
                    Box::pin(async move { flag_original(s, &locations, &flag).await })
                })
                .await
            }
            Err(e) => Err(e.into()),
        };
        if let Err(e) = result {
            tracing::warn!(account = account.0, error = %e, "cannot flag the original message");
        }
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
        type Known = (Vec<i64>, HashSet<String>, HashSet<String>);
        let (removed, uploads, messages): Known = write(&self.inner.stores, move |tx| {
            let names = |sql_text: &str, args: &[i64]| -> ComposeResult<Vec<String>> {
                let mut stmt = tx.prepare(sql_text).map_err(sql)?;
                stmt.query_map(rusqlite::params_from_iter(args), |r| r.get(0))
                    .map_err(sql)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(sql)
            };
            let removed = names(
                "DELETE FROM upload WHERE draft_id IS NULL AND created_at < ?1 \
                 RETURNING CAST(id AS TEXT)",
                &[cutoff],
            )?;
            let uploads = names("SELECT CAST(id AS TEXT) FROM upload", &[])?;
            let messages = names(
                "SELECT mime_file FROM outbox WHERE mime_file IS NOT NULL",
                &[],
            )?;
            Ok((
                removed.iter().filter_map(|id| id.parse().ok()).collect(),
                uploads.into_iter().collect(),
                messages.into_iter().collect(),
            ))
        })
        .await?;
        let upload_dir = self.inner.stores.uploads.clone();
        let outbox_dir = self.inner.stores.outbox.clone();
        let grace = self.inner.config.upload_grace;
        tokio::task::spawn_blocking(move || {
            remove_upload_files(&upload_dir, &removed);
            sweep(&upload_dir, &uploads, grace);
            sweep(&upload_dir.join(UPLOAD_TMP), &HashSet::new(), grace);
            sweep(&outbox_dir, &messages, grace);
            sweep(&outbox_dir.join(OUTBOX_TMP), &HashSet::new(), grace);
        })
        .await
        .map_err(|e| tagmail_api_core::compose::ComposeError::Internal(Box::new(e)))?;
        Ok(())
    }
}

/// Removes files in `dir` older than `grace` whose names are not in
/// `known`. Blocking.
fn sweep(dir: &Path, known: &HashSet<String>, grace: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= grace);
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_file() && stale && !known.contains(&name) {
            let _ = std::fs::remove_file(&path);
        }
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
