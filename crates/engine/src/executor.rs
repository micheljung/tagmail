//! Durable write ops (plan §3.4 Writes): the API records an op in
//! `pending_op` (IMAP-stable references only), the overlay shows it at
//! once, and each account's executor runs due ops on the account worker
//! between sync steps. Failures stay visible as failed ops until retried or
//! discarded; never silently dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_core::ops_dto::{OpDto, OpKindDto, OpStatusDto};
use tagmail_api_core::{OpMailbox, OpRequest, Submitted, WriteError, WriteResult};
use tagmail_core::ops::{
    self, ARCHIVE_NAME, ArchiveMode, Candidate, Destination, LabelArg, MailboxView, MessageSearch,
    Op, OpKind, OpState, Role,
};
use tagmail_core::{
    AccountId, DEFAULT_KEYWORD_LIMIT, EmailId, Flags, Keyword, KeywordSet, LabelKey, MailboxRef,
    SpecialUse, encode_label,
};
use tagmail_imap::{
    FetchSpec, Flag, ImapSession, ModSeq, SearchCriteria, SelectOptions, SelectedMailbox, StoreOp,
    Uid, UidSet,
};
use tagmail_store::rusqlite::{self, Connection, OptionalExtension, params};
use tagmail_store::{CacheDb, ChangeOp, ConfigDb, Entity};
use tokio::sync::{Notify, watch};

use crate::cache::{self, MailboxRow, SyncState};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::jobs::{JobFuture, Priority};
use crate::labels;
use crate::message;
use crate::msgid_search;
use crate::supervisor::{ConnectorFactory, Engine, Session};
use crate::sync;

/// Attempts before an op that keeps failing for transient reasons is
/// marked failed.
pub const MAX_ATTEMPTS: u32 = 5;
/// `op_seen` retention, longer than the 90-day replica intent TTL.
pub const OP_SEEN_SECS: i64 = 120 * 24 * 60 * 60;
/// Ops per worker job.
const BATCH: usize = 50;
/// Retry delay after a transient failure, doubled per attempt.
const RETRY_BASE: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// How often the executor checks a cache that is still in initial sync.
const NOT_READY_POLL: Duration = Duration::from_secs(1);
/// Upper bound on an idle executor's sleep.
const IDLE_WAIT: Duration = Duration::from_secs(3600);
/// Mailboxes Trash and Junk are found by when the server has no
/// special-use attribute for them.
const TRASH_NAMES: &[&str] = &["Trash", "Deleted Items", "Deleted Messages"];
const JUNK_NAMES: &[&str] = &["Junk", "Spam", "Junk E-mail"];

/// Names `u` is found by without a special-use attribute; the first is
/// the one created when none exists.
fn fallback_names(u: SpecialUse) -> &'static [&'static str] {
    match u {
        SpecialUse::Trash => TRASH_NAMES,
        SpecialUse::Junk => JUNK_NAMES,
        _ => &[],
    }
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Submission order of ops; strictly increasing within the process and
/// close to wall-clock milliseconds across restarts.
fn next_seq() -> i64 {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let mut last = LAST.load(Ordering::SeqCst);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

fn backoff(attempts: u32) -> Duration {
    RETRY_BASE
        .saturating_mul(1 << attempts.min(16))
        .min(RETRY_MAX)
}

/// A `pending_op` row.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    pub op: Op,
    pub account: AccountId,
    pub state: OpState,
    pub attempts: u32,
    pub next_at: Option<i64>,
    pub error: Option<String>,
    pub created_at: i64,
    pub seq: i64,
}

impl Stored {
    fn dto(&self, thread_ids: Vec<i64>, mailbox_id: Option<i64>) -> OpDto {
        OpDto {
            op_id: self.op.id.clone(),
            seq: self.seq,
            account_id: self.account.0,
            kind: OpKindDto::from(self.op.kind),
            status: self.state.into(),
            error: self.error.clone(),
            attempts: self.attempts,
            thread_ids,
            label: self.op.label.as_ref().map(|l| l.key.to_string()),
            mailbox_id,
            created_at: self.created_at,
        }
    }
}

/// The DTOs of `ops` with their current thread and destination ids.
pub(crate) async fn dtos(cache: &CacheDb, ops: Vec<Stored>) -> Result<Vec<OpDto>> {
    Ok(cache
        .read(move |c| {
            let mut threads = c.prepare_cached(
                "SELECT thread_id FROM message \
                 WHERE account_id = ?1 AND email_id = ?2 AND tombstoned_at IS NULL",
            )?;
            let mut merged = c.prepare_cached("SELECT merged_into FROM thread WHERE id = ?1")?;
            let mut mailbox =
                c.prepare_cached("SELECT id FROM mailbox WHERE account_id = ?1 AND name = ?2")?;
            let mut out = Vec::with_capacity(ops.len());
            for s in &ops {
                let mut ids = BTreeSet::new();
                for e in &s.op.email_ids {
                    let found: Option<Option<i64>> = threads
                        .query_row(params![s.account.0, e.as_str()], |r| r.get(0))
                        .optional()?;
                    let Some(Some(mut thread)) = found else {
                        continue;
                    };
                    while let Some(Some(into)) = merged
                        .query_row([thread], |r| r.get::<_, Option<i64>>(0))
                        .optional()?
                    {
                        thread = into;
                    }
                    ids.insert(thread);
                }
                let mailbox_id = match &s.op.mailbox {
                    Some(m) => mailbox
                        .query_row(params![s.account.0, m.name], |r| r.get(0))
                        .optional()?,
                    None => None,
                };
                out.push(s.dto(ids.into_iter().collect(), mailbox_id));
            }
            Ok(out)
        })
        .await?)
}

const OP_COLUMNS: &str = "op_id, account_id, kind, email_ids, mailbox_name, mailbox_special_use, \
     mailbox_uidvalidity, payload, state, attempts, next_at, error, created_at";

fn corrupt(what: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("pending_op: {what}").into(),
    )
}

fn op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    let id: String = r.get(0)?;
    let kind: String = r.get(2)?;
    let kind: OpKind = kind.parse().map_err(corrupt)?;
    let email_ids: String = r.get(3)?;
    let email_ids: Vec<String> = serde_json::from_str(&email_ids).map_err(corrupt)?;
    let email_ids = email_ids
        .iter()
        .map(|e| EmailId::parse(e))
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(corrupt)?;
    let mailbox = match (r.get::<_, Option<String>>(4)?, r.get::<_, Option<u32>>(6)?) {
        (Some(name), Some(uidvalidity)) => Some(MailboxRef {
            name,
            special_use: r
                .get::<_, Option<String>>(5)?
                .and_then(|s| s.parse::<SpecialUse>().ok()),
            uidvalidity,
        }),
        _ => None,
    };
    let payload: Value = r
        .get::<_, Option<String>>(7)?
        .map(|p| serde_json::from_str(&p))
        .transpose()
        .map_err(corrupt)?
        .unwrap_or(Value::Null);
    let label = match (payload["keyword"].as_str(), payload["key"].as_str()) {
        (Some(k), Some(key)) => Some(LabelArg {
            keyword: Keyword::new(k).map_err(corrupt)?,
            key: LabelKey::from_stored(key.to_owned()),
        }),
        _ => None,
    };
    let state: String = r.get(8)?;
    Ok(Stored {
        op: Op {
            id,
            kind,
            email_ids,
            label,
            mailbox,
        },
        account: AccountId(r.get(1)?),
        state: state.parse().map_err(corrupt)?,
        attempts: r.get(9)?,
        next_at: r.get(10)?,
        error: r.get(11)?,
        created_at: r.get(12)?,
        seq: payload["seq"].as_i64().unwrap_or(0),
    })
}

fn load(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Stored>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {OP_COLUMNS} FROM pending_op WHERE {filter}"
    ))?;
    let mut out = stmt
        .query_map(params, op_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    out.sort_by_key(|s| (s.seq, s.created_at));
    Ok(out)
}

pub(crate) fn load_pending(conn: &Connection) -> rusqlite::Result<Vec<Stored>> {
    load(conn, "state = 'pending'", [])
}

fn insert(conn: &Connection, s: &Stored) -> rusqlite::Result<()> {
    let email_ids: Vec<&str> = s.op.email_ids.iter().map(EmailId::as_str).collect();
    let mut payload = json!({ "seq": s.seq });
    if let Some(l) = &s.op.label {
        payload["keyword"] = json!(l.keyword.as_str());
        payload["key"] = json!(l.key.as_str());
    }
    conn.execute(
        "INSERT INTO pending_op (op_id, account_id, kind, email_ids, mailbox_name, \
         mailbox_special_use, mailbox_uidvalidity, payload, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
        params![
            s.op.id,
            s.account.0,
            s.op.kind.as_str(),
            serde_json::to_string(&email_ids).map_err(corrupt)?,
            s.op.mailbox.as_ref().map(|m| m.name.clone()),
            s.op.mailbox
                .as_ref()
                .and_then(|m| m.special_use)
                .map(SpecialUse::as_imap),
            s.op.mailbox.as_ref().map(|m| m.uidvalidity),
            payload.to_string(),
        ],
    )?;
    Ok(())
}

/// `account.archive_mode` of every account.
async fn archive_modes(config: &ConfigDb) -> Result<HashMap<AccountId, Option<ArchiveMode>>> {
    Ok(config
        .read(|c| {
            let mut stmt = c.prepare("SELECT id, archive_mode FROM account")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        AccountId(r.get(0)?),
                        r.get::<_, Option<String>>(1)?
                            .and_then(|m| m.parse::<ArchiveMode>().ok()),
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?)
}

fn op_event(w: &mut cache::W<'_>, dto: &OpDto) {
    w.emit(
        Entity::Op,
        dto.seq,
        ChangeOp::Upsert,
        Some(serde_json::to_value(dto).unwrap_or(Value::Null)),
    );
}

/// Message rows of the account for `email_ids`.
async fn message_rows(ctx: &Ctx, email_ids: &BTreeSet<EmailId>) -> Result<Vec<i64>> {
    let account = ctx.account.0;
    let ids: Vec<String> = email_ids.iter().map(|e| e.as_str().to_owned()).collect();
    ctx.read(move |c| cache::messages_by_email_id(c, account, &ids))
        .await
}

/// Re-projects the overlay over `email_ids` and publishes `events`.
async fn reproject(ctx: &Ctx, email_ids: &BTreeSet<EmailId>, events: Vec<OpDto>) -> Result<()> {
    let messages = message_rows(ctx, email_ids).await?;
    ctx.write(move |w| {
        cache::reproject(w, &messages)?;
        for e in &events {
            op_event(w, e);
        }
        Ok(())
    })
    .await
}

impl<F: ConnectorFactory> Engine<F> {
    pub(crate) fn ctx(&self, account: AccountId) -> Ctx {
        let stores = &self.inner.stores;
        Ctx::new(
            stores.cache.clone(),
            stores.blobs.clone(),
            stores.feed.clone(),
            account,
        )
        .with_overlays(self.inner.overlays.clone())
        .with_blob_writes(self.blob_writes(account))
    }

    pub(crate) fn wake(&self, account: AccountId) -> std::sync::Arc<Notify> {
        self.inner
            .wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(account)
            .or_default()
            .clone()
    }

    /// Rebuilds the overlay from `pending_op` and re-projects it, also over
    /// rows a previous run left projected: at startup, before any account
    /// syncs. Ops interrupted while running go back to pending; expired
    /// `op_seen` rows are dropped.
    pub async fn restore_overlays(&self) -> Result<()> {
        let config = &self.inner.stores.config;
        let cutoff = now_secs() - OP_SEEN_SECS;
        let open = config
            .write(move |tx| {
                tx.execute(
                    "UPDATE pending_op SET state = 'pending' WHERE state = 'running'",
                    [],
                )?;
                tx.execute("DELETE FROM op_seen WHERE seen_at < ?1", [cutoff])?;
                tx.execute(
                    "DELETE FROM pending_op WHERE state = 'done' AND created_at < ?1",
                    [cutoff],
                )?;
                Ok(load_pending(tx)?)
            })
            .await?;
        let modes = archive_modes(config).await?;
        let mut accounts: BTreeSet<AccountId> = modes.keys().copied().collect();
        accounts.extend(open.iter().map(|s| s.account));
        for account in accounts {
            let ops: Vec<Op> = open
                .iter()
                .filter(|s| s.account == account)
                .map(|s| s.op.clone())
                .collect();
            let mode = modes.get(&account).copied().flatten();
            self.inner.overlays.update(account, |a| {
                a.archive_mode = mode;
                a.ops = ops::Overlay::new(ops.clone());
            });
            let ctx = self.ctx(account);
            let ids: BTreeSet<EmailId> = ops.iter().flat_map(|o| o.email_ids.clone()).collect();
            let mut messages = message_rows(&ctx, &ids).await?;
            let a = account.0;
            messages.extend(ctx.read(move |c| cache::overlaid_messages(c, a)).await?);
            messages.sort_unstable();
            messages.dedup();
            if !messages.is_empty() {
                ctx.write(move |w| cache::reproject(w, &messages)).await?;
            }
        }
        Ok(())
    }

    async fn stored_op(&self, op_id: String) -> Result<Option<Stored>> {
        Ok(self
            .inner
            .stores
            .config
            .read(move |c| Ok(load(c, "op_id = ?1", [op_id])?.into_iter().next()))
            .await?)
    }

    pub(crate) async fn op_dto(&self, op_id: String) -> WriteResult<OpDto> {
        let Some(s) = self.stored_op(op_id).await.map_err(WriteError::internal)? else {
            return Err(WriteError::NotFound("no such op".to_owned()));
        };
        self.dto(s).await
    }

    async fn dto(&self, s: Stored) -> WriteResult<OpDto> {
        dtos(&self.inner.stores.cache, vec![s])
            .await
            .map_err(WriteError::internal)?
            .pop()
            .ok_or_else(|| WriteError::NotFound("no such op".to_owned()))
    }

    pub(crate) async fn queue_op(&self, req: OpRequest) -> WriteResult<Submitted> {
        let id = req.op_id.clone();
        let seen: bool = self
            .inner
            .stores
            .config
            .read(move |c| {
                Ok(
                    c.query_row("SELECT 1 FROM op_seen WHERE op_id = ?1", [id], |_| Ok(()))
                        .optional()?
                        .is_some(),
                )
            })
            .await
            .map_err(WriteError::internal)?;
        if seen {
            return self.existing(req.op_id).await;
        }
        let (account, email_ids) = self.targets(&req).await?;
        let label = match &req.label {
            Some(l) => Some(self.label_arg(account, req.kind, l).await?),
            None => None,
        };
        let mailbox = match req.mailbox {
            Some(OpMailbox::Id(id)) => Some(self.mailbox_ref(account, id.0).await?),
            // IMAP never assigns uidvalidity 0, so an unknown one matches nothing.
            Some(OpMailbox::Ref(r)) => Some(MailboxRef {
                name: r.name,
                special_use: r.special_use.map(Into::into),
                uidvalidity: r.uidvalidity.unwrap_or(0),
            }),
            None => None,
        };
        let stored = Stored {
            op: Op {
                id: req.op_id.clone(),
                kind: req.kind,
                email_ids,
                label,
                mailbox,
            },
            account,
            state: OpState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
            created_at: now_secs(),
            seq: next_seq(),
        };
        let row = stored.clone();
        let created = self
            .inner
            .stores
            .config
            .write(move |tx| {
                let fresh = tx.execute(
                    "INSERT OR IGNORE INTO op_seen (op_id, account_id) VALUES (?1, ?2)",
                    params![row.op.id, row.account.0],
                )? == 1;
                if fresh {
                    insert(tx, &row)?;
                }
                Ok(fresh)
            })
            .await
            .map_err(WriteError::internal)?;
        if !created {
            return self.existing(req.op_id).await;
        }
        self.inner.overlays.insert(account, stored.op.clone());
        let ctx = self.ctx(account);
        let dto = self.dto(stored.clone()).await?;
        reproject(&ctx, &stored.op.email_ids, vec![dto.clone()])
            .await
            .map_err(WriteError::internal)?;
        self.wake(account).notify_one();
        Ok(Submitted::Created(dto))
    }

    /// A re-submitted op id: its status, unless it was discarded.
    async fn existing(&self, op_id: String) -> WriteResult<Submitted> {
        match self.op_dto(op_id).await {
            Ok(dto) => Ok(Submitted::Existing(dto)),
            Err(WriteError::NotFound(_)) => Err(WriteError::Conflict(
                "this op id was used by an op that has been discarded".to_owned(),
            )),
            Err(e) => Err(e),
        }
    }

    /// The single account and the email ids an op request names.
    async fn targets(&self, req: &OpRequest) -> WriteResult<(AccountId, BTreeSet<EmailId>)> {
        let threads: Vec<i64> = req.thread_ids.iter().map(|t| t.0).collect();
        let emails = req.email_ids.clone();
        let rows: Vec<(i64, String)> = self
            .inner
            .stores
            .cache
            .read(move |c| {
                let mut out = Vec::new();
                let mut stmt = c.prepare_cached(
                    "SELECT account_id, email_id FROM message \
                     WHERE thread_id = ?1 AND tombstoned_at IS NULL",
                )?;
                for t in threads {
                    let mut thread = t;
                    for _ in 0..64 {
                        let next: Option<Option<i64>> = c
                            .query_row(
                                "SELECT merged_into FROM thread WHERE id = ?1",
                                [thread],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match next {
                            Some(Some(into)) => thread = into,
                            _ => break,
                        }
                    }
                    for row in stmt.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))? {
                        out.push(row?);
                    }
                }
                let mut by_email = c.prepare_cached(
                    "SELECT account_id, email_id FROM message \
                     WHERE email_id = ?1 AND tombstoned_at IS NULL",
                )?;
                for e in emails {
                    for row in by_email.query_map([e], |r| Ok((r.get(0)?, r.get(1)?)))? {
                        out.push(row?);
                    }
                }
                Ok(out)
            })
            .await
            .map_err(WriteError::internal)?;
        let accounts: BTreeSet<i64> = rows.iter().map(|(a, _)| *a).collect();
        let account = match accounts.len() {
            0 => {
                return Err(WriteError::NotFound(
                    "no such threads or messages".to_owned(),
                ));
            }
            1 => AccountId(*accounts.first().unwrap_or(&0)),
            _ => {
                return Err(WriteError::invalid(
                    "threadIds",
                    "the targets belong to more than one account; send one op per account",
                ));
            }
        };
        let ids = rows
            .into_iter()
            .filter_map(|(_, e)| EmailId::parse(&e).ok())
            .collect();
        Ok((account, ids))
    }

    /// The keyword a label op stores: the account's existing spelling, else
    /// the label's registered or known display name encoded (a unified-view
    /// label creates the keyword in this account).
    async fn label_arg(
        &self,
        account: AccountId,
        kind: OpKind,
        label: &str,
    ) -> WriteResult<LabelArg> {
        let key = LabelKey::of(label);
        let k = key.as_str().to_owned();
        let a = account.0;
        let cached: Vec<(i64, String)> = self
            .inner
            .stores
            .cache
            .read(move |c| {
                let mut stmt = c.prepare_cached(
                    "SELECT account_id, keyword FROM label \
                     WHERE label_key = ?1 AND kind = 'keyword' ORDER BY account_id != ?2",
                )?;
                let rows = stmt
                    .query_map(params![k, a], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await
            .map_err(WriteError::internal)?;
        if let Some((owner, keyword)) = cached.first()
            && *owner == account.0
            && let Ok(keyword) = Keyword::new(keyword.clone())
        {
            return Ok(LabelArg { keyword, key });
        }
        let name = match labels::display_name(&self.inner.stores.config, &key)
            .await
            .map_err(WriteError::internal)?
        {
            Some(name) => Some(name),
            None => cached.first().and_then(|(_, kw)| {
                Keyword::new(kw.clone()).ok().and_then(|kw| {
                    match tagmail_core::classify_keyword(&kw) {
                        tagmail_core::KeywordClass::Label { name, .. } => Some(name),
                        tagmail_core::KeywordClass::Hidden => None,
                    }
                })
            }),
        };
        let name = match (name, kind) {
            (Some(n), _) => n,
            (None, OpKind::LabelRemove) => label.to_owned(),
            (None, _) => {
                return Err(WriteError::invalid(
                    "label",
                    "no such label; create it with POST /api/labels",
                ));
            }
        };
        let keyword = encode_label(&name, DEFAULT_KEYWORD_LIMIT)
            .map_err(|e| WriteError::invalid("label", e.to_string()))?;
        Ok(LabelArg { keyword, key })
    }

    async fn mailbox_ref(&self, account: AccountId, mailbox: i64) -> WriteResult<MailboxRef> {
        let row: Option<(i64, String, Option<String>, u32)> = self
            .inner
            .stores
            .cache
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT account_id, name, special_use, uidvalidity FROM mailbox WHERE id = ?1",
                    [mailbox],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?)
            })
            .await
            .map_err(WriteError::internal)?;
        match row {
            Some((a, name, special_use, uidvalidity)) if a == account.0 => {
                if uidvalidity == 0 {
                    return Err(WriteError::invalid(
                        "mailboxId",
                        "the mailbox has not been synced yet",
                    ));
                }
                Ok(MailboxRef {
                    name,
                    special_use: special_use.and_then(|s| s.parse().ok()),
                    uidvalidity,
                })
            }
            Some(_) => Err(WriteError::invalid(
                "mailboxId",
                "the mailbox belongs to another account",
            )),
            None => Err(WriteError::invalid("mailboxId", "no such mailbox")),
        }
    }

    pub(crate) async fn list_ops(&self, status: Option<OpStatusDto>) -> WriteResult<Vec<OpDto>> {
        let filter = match status {
            Some(OpStatusDto::Failed) => "state = 'failed'",
            Some(OpStatusDto::Pending) => "state IN ('pending', 'running')",
            _ => "state != 'done'",
        };
        let rows = self
            .inner
            .stores
            .config
            .read(move |c| Ok(load(c, filter, [])?))
            .await
            .map_err(WriteError::internal)?;
        dtos(&self.inner.stores.cache, rows)
            .await
            .map_err(WriteError::internal)
    }

    pub(crate) async fn retry_failed_op(&self, op_id: String) -> WriteResult<OpDto> {
        let Some(s) = self.stored_op(op_id).await.map_err(WriteError::internal)? else {
            return Err(WriteError::NotFound("no such open op".to_owned()));
        };
        if s.state != OpState::Failed {
            return Err(WriteError::Conflict(
                "only failed ops can be retried".to_owned(),
            ));
        }
        let id = s.op.id.clone();
        self.inner
            .stores
            .config
            .write(move |tx| {
                tx.execute(
                    "UPDATE pending_op SET state = 'pending', attempts = 0, next_at = NULL, \
                     error = NULL WHERE op_id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .await
            .map_err(WriteError::internal)?;
        let fresh = Stored {
            state: OpState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
            ..s
        };
        self.inner.overlays.insert(fresh.account, fresh.op.clone());
        let dto = self.dto(fresh.clone()).await?;
        reproject(
            &self.ctx(fresh.account),
            &fresh.op.email_ids,
            vec![dto.clone()],
        )
        .await
        .map_err(WriteError::internal)?;
        self.wake(fresh.account).notify_one();
        Ok(dto)
    }

    pub(crate) async fn discard_op_id(&self, op_id: String) -> WriteResult<()> {
        let Some(s) = self.stored_op(op_id).await.map_err(WriteError::internal)? else {
            return Err(WriteError::NotFound("no such open op".to_owned()));
        };
        let id = s.op.id.clone();
        let deleted = self
            .inner
            .stores
            .config
            .write(move |tx| {
                Ok(tx.execute(
                    "DELETE FROM pending_op WHERE op_id = ?1 AND state != 'running'",
                    [id],
                )?)
            })
            .await
            .map_err(WriteError::internal)?;
        if deleted == 0 {
            return Err(WriteError::Conflict(
                "the op is being applied right now".to_owned(),
            ));
        }
        self.inner.overlays.remove(s.account, &s.op.id);
        let ctx = self.ctx(s.account);
        reproject(&ctx, &s.op.email_ids, Vec::new())
            .await
            .map_err(WriteError::internal)?;
        let op_id = s.op.id.clone();
        ctx.write(move |w| {
            w.emit(
                Entity::Op,
                0,
                ChangeOp::Delete,
                Some(json!({ "opId": op_id })),
            );
            Ok(())
        })
        .await
        .map_err(WriteError::internal)
    }
}

/// Whether every mailbox of the account has been listed through its
/// initial windows, so the cache knows every location of every message.
async fn cache_ready(ctx: &Ctx) -> Result<bool> {
    let account = ctx.account.0;
    ctx.read(move |c| {
        let (total, initial): (i64, i64) = c.query_row(
            "SELECT count(*), coalesce(sum(sync_state = 'initial'), 0) FROM mailbox \
             WHERE account_id = ?1",
            [account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(total > 0 && initial == 0)
    })
    .await
}

/// Runs one account's due ops and label jobs until `stop`.
pub(crate) async fn run<F: ConnectorFactory>(
    engine: Engine<F>,
    account: AccountId,
    mut stop: watch::Receiver<bool>,
) {
    let wake = engine.wake(account);
    let ctx = engine.ctx(account);
    loop {
        let wait = match step(&engine, &ctx, account).await {
            Ok(wait) => wait,
            Err(e) => {
                tracing::warn!(account = account.0, error = %e, "op executor failed");
                Some(RETRY_BASE)
            }
        };
        if let Some(wait) = wait {
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
                _ = stop.wait_for(|s| *s) => return,
            }
        } else if *stop.borrow() {
            return;
        }
    }
}

/// Runs one batch of due ops or one label-job step. `Some(wait)` when
/// there was nothing to do now.
async fn step<F: ConnectorFactory>(
    engine: &Engine<F>,
    ctx: &Ctx,
    account: AccountId,
) -> Result<Option<Duration>> {
    let now = now_secs();
    let a = account.0;
    let open = engine
        .inner
        .stores
        .config
        .read(move |c| Ok(load(c, "account_id = ?1 AND state = 'pending'", [a])?))
        .await?;
    let has_jobs = labels::has_jobs(&engine.inner.stores.config, account).await?;
    if open.is_empty() && !has_jobs {
        return Ok(Some(IDLE_WAIT));
    }
    if !cache_ready(ctx).await? {
        return Ok(Some(NOT_READY_POLL));
    }
    let due: Vec<Stored> = open
        .iter()
        .filter(|s| s.next_at.is_none_or(|t| t <= now))
        .take(BATCH)
        .cloned()
        .collect();
    if !due.is_empty() {
        run_ops(engine, ctx, account, due).await?;
        return Ok(None);
    }
    if has_jobs && labels::run_step(engine, ctx, account).await? {
        return Ok(None);
    }
    let next = open
        .iter()
        .filter_map(|s| s.next_at)
        .min()
        .map_or(IDLE_WAIT, |t| {
            Duration::from_secs(u64::try_from(t - now).unwrap_or(0).max(1))
        });
    Ok(Some(if has_jobs { next.min(RETRY_BASE) } else { next }))
}

/// How one op ended in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    Failed(String),
}

#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub outcomes: Vec<(String, Outcome)>,
    /// The session broke after `outcomes`; the remaining ops are retried.
    pub aborted: Option<String>,
    /// A label the server answered OK to but did not keep.
    pub dropped_label: Option<String>,
}

/// Marks `due` running and returns the ops that were still pending: one
/// settled, discarded or retried since `due` was read is left alone, so its
/// state is never overwritten by this run.
pub(crate) async fn claim(config: &ConfigDb, due: Vec<Stored>) -> Result<Vec<Stored>> {
    Ok(config
        .write(move |tx| {
            let mut claimed = Vec::with_capacity(due.len());
            for s in due {
                let changed = tx.execute(
                    "UPDATE pending_op SET state = 'running' WHERE op_id = ?1 AND state = 'pending'",
                    [&s.op.id],
                )?;
                if changed == 1 {
                    claimed.push(s);
                }
            }
            Ok(claimed)
        })
        .await?)
}

async fn run_ops<F: ConnectorFactory>(
    engine: &Engine<F>,
    ctx: &Ctx,
    account: AccountId,
    due: Vec<Stored>,
) -> Result<()> {
    let due = claim(&engine.inner.stores.config, due).await?;
    if due.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = due.iter().map(|s| s.op.id.clone()).collect();
    let ops: Vec<Op> = due.iter().map(|s| s.op.clone()).collect();
    let mode = engine.inner.overlays.get(account).archive_mode;
    let result = engine
        .run_job(account, Priority::Write, move |s, ctx| {
            execute_job::<F>(s, ctx, ops, mode)
        })
        .await;
    let (batch, transient) = match result {
        Ok(batch) => {
            let why = batch.aborted.clone();
            (batch, why)
        }
        Err(Error::Unavailable(why)) => {
            let now = now_secs();
            let wait = i64::try_from(RETRY_BASE.as_secs()).unwrap_or(1);
            engine
                .inner
                .stores
                .config
                .write(move |tx| {
                    for id in ids {
                        tx.execute(
                            "UPDATE pending_op SET state = 'pending', next_at = ?2, error = ?3 \
                             WHERE op_id = ?1 AND state = 'running'",
                            params![id, now + wait, why],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            return Ok(());
        }
        Err(e) => (Batch::default(), Some(e.to_string())),
    };
    let added: Vec<LabelArg> = due
        .iter()
        .filter(|s| s.op.kind == OpKind::LabelAdd)
        .filter(|s| {
            batch
                .outcomes
                .iter()
                .any(|(id, o)| *id == s.op.id && *o == Outcome::Done)
        })
        .filter_map(|s| s.op.label.clone())
        .collect();
    engine.record_label_health(account, batch.dropped_label.clone(), !added.is_empty());
    finish(engine, ctx, account, due, batch, transient).await?;
    for label in added {
        engine.ensure_registered(account, &label).await?;
    }
    Ok(())
}

/// Records each op's outcome; ops without one get a transient failure.
async fn finish<F: ConnectorFactory>(
    engine: &Engine<F>,
    ctx: &Ctx,
    account: AccountId,
    due: Vec<Stored>,
    batch: Batch,
    transient: Option<String>,
) -> Result<()> {
    let outcomes: HashMap<String, Outcome> = batch.outcomes.into_iter().collect();
    let now = now_secs();
    let mut closed = Vec::new();
    let mut events = Vec::new();
    let mut updates = Vec::new();
    for s in due {
        let (state, error, attempts) = match outcomes.get(&s.op.id) {
            Some(Outcome::Done) => (OpState::Done, None, s.attempts + 1),
            Some(Outcome::Failed(why)) => (OpState::Failed, Some(why.clone()), s.attempts + 1),
            None => {
                let attempts = s.attempts + 1;
                let why = transient
                    .clone()
                    .unwrap_or_else(|| "interrupted".to_owned());
                if attempts >= MAX_ATTEMPTS {
                    (OpState::Failed, Some(why), attempts)
                } else {
                    (OpState::Pending, Some(why), attempts)
                }
            }
        };
        let next_at = (state == OpState::Pending)
            .then(|| now + i64::try_from(backoff(attempts).as_secs()).unwrap_or(1));
        updates.push((s.op.id.clone(), state, error.clone(), attempts, next_at));
        if state != OpState::Pending {
            closed.push(s.op.clone());
        }
        events.push(Stored {
            state,
            error,
            attempts,
            next_at,
            ..s
        });
    }
    let events = dtos(&engine.inner.stores.cache, events).await?;
    engine
        .inner
        .stores
        .config
        .write(move |tx| {
            for (id, state, error, attempts, next_at) in updates {
                if state == OpState::Done {
                    tx.execute(
                        "UPDATE pending_op SET state = 'done', email_ids = '[]', error = NULL, \
                         attempts = ?2, next_at = NULL WHERE op_id = ?1",
                        params![id, attempts],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE pending_op SET state = ?2, error = ?3, attempts = ?4, \
                         next_at = ?5 WHERE op_id = ?1",
                        params![id, state.as_str(), error, attempts, next_at],
                    )?;
                }
            }
            Ok(())
        })
        .await?;
    let mut touched = BTreeSet::new();
    for op in &closed {
        engine.inner.overlays.remove(account, &op.id);
        touched.extend(op.email_ids.iter().cloned());
    }
    reproject(ctx, &touched, events).await
}

fn execute_job<'a, F: ConnectorFactory>(
    s: &'a mut Session<F>,
    ctx: &'a Ctx,
    ops: Vec<Op>,
    mode: Option<ArchiveMode>,
) -> JobFuture<'a, Batch> {
    Box::pin(execute_batch(s, ctx, ops, mode))
}

/// Why one op could not be applied.
enum OpError {
    /// Visible to the user; the op fails.
    Failed(String),
    /// The server answered OK to adding the label but did not keep its
    /// keyword, as Stalwart does past its per-account keyword limit.
    KeywordNotPersisted(String),
    /// The session is unusable; the batch stops.
    Fatal(Error),
}

impl From<Error> for OpError {
    fn from(e: Error) -> OpError {
        if e.is_fatal() {
            OpError::Fatal(e)
        } else {
            OpError::Failed(e.to_string())
        }
    }
}

impl From<tagmail_imap::Error> for OpError {
    fn from(e: tagmail_imap::Error) -> OpError {
        OpError::from(Error::from(e))
    }
}

impl From<tagmail_store::Error> for OpError {
    fn from(e: tagmail_store::Error) -> OpError {
        OpError::Fatal(Error::from(e))
    }
}

/// The server's mailboxes and the account's mailbox rows.
pub(crate) struct Mailboxes {
    pub candidates: Vec<Candidate>,
    pub rows: Vec<MailboxRow>,
    pub mode: ArchiveMode,
}

impl Mailboxes {
    pub(crate) async fn load<S: ImapSession>(
        s: &mut S,
        ctx: &Ctx,
        configured: Option<ArchiveMode>,
    ) -> Result<Mailboxes> {
        let rows = sync::list(s, ctx).await?;
        let mut candidates = Vec::with_capacity(rows.len());
        for r in &rows {
            let uidvalidity = if r.uidvalidity != 0 {
                Some(r.uidvalidity)
            } else {
                s.status(&r.name).await?.uid_validity.map(|v| v.0)
            };
            candidates.push(Candidate {
                name: r.name.clone(),
                special_use: r.special_use,
                uidvalidity,
                delimiter: r.delimiter,
            });
        }
        let has_all = rows.iter().any(|r| r.special_use == Some(SpecialUse::All));
        Ok(Mailboxes {
            candidates,
            rows,
            mode: ArchiveMode::detect(configured, has_all),
        })
    }

    fn archive_name(&self) -> Option<String> {
        ops::archive_mailbox(
            self.mode,
            self.candidates
                .iter()
                .map(|c| (c.name.as_str(), c.special_use, c.delimiter)),
        )
        .map(str::to_owned)
    }

    pub(crate) fn view<'a>(
        &self,
        name: &'a str,
        special_use: Option<SpecialUse>,
    ) -> MailboxView<'a> {
        MailboxView {
            name,
            role: Role::of(name, special_use),
            archive_mode: self.mode,
            is_archive: self.archive_name().as_deref() == Some(name),
        }
    }

    fn by_special_use(&self, u: SpecialUse) -> Option<String> {
        self.candidates
            .iter()
            .find(|c| c.special_use == Some(u))
            .map(|c| c.name.as_str())
            .or_else(|| {
                ops::folder_named(
                    fallback_names(u),
                    self.candidates
                        .iter()
                        .filter(|c| c.special_use.is_none())
                        .map(|c| (c.name.as_str(), c.delimiter)),
                )
            })
            .map(str::to_owned)
    }

    /// Creates the folder `leaf` in the account's personal namespace and
    /// reloads the mailboxes.
    async fn create<S: ImapSession>(
        &mut self,
        s: &mut S,
        ctx: &Ctx,
        leaf: &str,
    ) -> std::result::Result<(), OpError> {
        let namespace = s.personal_namespace().await?;
        let name = ops::new_folder_name(
            leaf,
            namespace.as_deref(),
            self.candidates
                .iter()
                .map(|c| (c.name.as_str(), c.delimiter)),
        );
        s.create_mailbox(&name)
            .await
            .map_err(|e| match OpError::from(e) {
                OpError::Failed(why) => OpError::Failed(format!("creating {name} failed: {why}")),
                fatal => fatal,
            })?;
        *self = Mailboxes::load(s, ctx, Some(self.mode)).await?;
        Ok(())
    }

    fn inbox(&self) -> Option<String> {
        self.candidates
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case("INBOX"))
            .map(|c| c.name.clone())
    }
}

pub(crate) async fn execute_batch<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    ops: Vec<Op>,
    configured: Option<ArchiveMode>,
) -> Result<Batch> {
    let mut mbs = Mailboxes::load(s, ctx, configured).await?;
    let mut batch = Batch::default();
    let mut sources = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for op in ops {
        let outcome = execute_op(s, ctx, &op, &mut mbs, &mut sources, &mut destinations).await;
        let why = match outcome {
            Ok(()) => {
                batch.outcomes.push((op.id, Outcome::Done));
                continue;
            }
            Err(OpError::Fatal(e)) => {
                batch.aborted = Some(e.to_string());
                return Ok(batch);
            }
            Err(OpError::Failed(why)) => why,
            Err(OpError::KeywordNotPersisted(label)) => {
                let why = format!(
                    "the server did not keep the label \u{201c}{label}\u{201d}; \
                     it may have reached its limit of labels"
                );
                batch.dropped_label = Some(label);
                why
            }
        };
        tracing::info!(op = op.id, error = why, "op failed");
        batch.outcomes.push((op.id, Outcome::Failed(why)));
    }
    let rows = sync::ordered(ctx).await?;
    for name in destinations.iter().chain(sources.difference(&destinations)) {
        let Some(row) = rows
            .iter()
            .find(|r| &r.name == name && r.state == SyncState::Live)
        else {
            continue;
        };
        if let Err(e) = sync::sync_mailbox(s, ctx, row, &|| false).await {
            if e.is_fatal() {
                batch.aborted = Some(e.to_string());
                return Ok(batch);
            }
            tracing::warn!(mailbox = name, error = %e, "post-op sync failed");
        }
    }
    Ok(batch)
}

/// One cached location of a target message: the server's state.
#[derive(Debug, Clone)]
struct Loc {
    message: i64,
    email_id: String,
    msgid: Option<String>,
    mailbox: i64,
    uid: u32,
    flags: Flags,
    keywords: KeywordSet,
    modseq: Option<u64>,
}

async fn locations(ctx: &Ctx, op: &Op) -> Result<Vec<Loc>> {
    let account = ctx.account.0;
    let ids: Vec<String> = op.email_ids.iter().map(|e| e.as_str().to_owned()).collect();
    ctx.read(move |c| {
        let mut stmt = c.prepare_cached(
            "SELECT m.id, m.email_id, m.msgid_hdr, l.mailbox_id, l.uid, \
             coalesce(l.srv_flags, l.flags), coalesce(l.srv_keywords, l.keywords), l.modseq \
             FROM message m JOIN location l ON l.message_id = m.id \
             WHERE m.account_id = ?1 AND m.email_id = ?2",
        )?;
        let mut out = Vec::new();
        for id in ids {
            let rows = stmt.query_map(params![account, id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, u8>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            })?;
            for row in rows {
                let (message, email_id, msgid, mailbox, uid, flags, keywords, modseq) = row?;
                out.push(Loc {
                    message,
                    email_id,
                    msgid,
                    mailbox,
                    uid,
                    flags: Flags::from_bits(flags),
                    keywords: KeywordSet::from_storage(&keywords)?,
                    modseq: modseq.and_then(|m| u64::try_from(m).ok()),
                });
            }
        }
        Ok(out)
    })
    .await
}

/// Finds the target messages in the selected mailbox by searching the
/// server, for when the cached UIDs belong to another UIDVALIDITY.
async fn search_locations<S: ImapSession>(
    s: &mut S,
    sel: &SelectedMailbox,
    cached: &[Loc],
    mailbox: i64,
) -> std::result::Result<Vec<Loc>, OpError> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut finder = msgid_search::Finder::default();
    for l in cached {
        if !seen.insert(l.email_id.clone()) {
            continue;
        }
        let Ok(email_id) = EmailId::parse(&l.email_id) else {
            continue;
        };
        let (uids, verify) = match ops::message_search(&email_id, l.msgid.as_deref()) {
            MessageSearch::EmailId(id) => (s.uid_search(SearchCriteria::EmailId(id)).await?, false),
            MessageSearch::MessageId(m) => (finder.find(s, sel, &m).await?, true),
            MessageSearch::Unsearchable => continue,
        };
        if uids.is_empty() {
            continue;
        }
        let spec = if verify {
            FetchSpec::sync_window(false)
        } else {
            FetchSpec::flags()
        };
        for meta in s.fetch(&uids, &spec).await? {
            let Some(f) = message::fetched(&meta) else {
                continue;
            };
            if verify && f.message.as_ref().map(|m| m.email_id.as_str()) != Some(email_id.as_str())
            {
                continue;
            }
            out.push(Loc {
                mailbox,
                uid: f.location.uid,
                flags: f.location.flags,
                keywords: f.location.keywords,
                modseq: f.location.modseq,
                ..l.clone()
            });
        }
    }
    Ok(out)
}

fn uid_set(locs: &[Loc]) -> UidSet {
    locs.iter().map(|l| Uid(l.uid)).collect()
}

fn store_args(op: &Op, locs: &[Loc]) -> Option<(StoreOp, Vec<Flag>, Vec<Loc>)> {
    let all = locs.to_vec();
    match op.kind {
        OpKind::MarkRead => Some((StoreOp::Add, vec![Flag::Seen], all)),
        OpKind::MarkUnread => Some((StoreOp::Remove, vec![Flag::Seen], all)),
        OpKind::Star => Some((StoreOp::Add, vec![Flag::Flagged], all)),
        OpKind::Unstar => Some((StoreOp::Remove, vec![Flag::Flagged], all)),
        OpKind::LabelAdd => {
            let l = op.label.as_ref()?;
            let missing: Vec<Loc> = locs
                .iter()
                .filter(|loc| ops::label_spellings(&loc.keywords, &l.key).is_empty())
                .cloned()
                .collect();
            Some((
                StoreOp::Add,
                vec![Flag::Keyword(l.keyword.as_str().to_owned())],
                missing,
            ))
        }
        OpKind::LabelRemove => {
            let l = op.label.as_ref()?;
            let mut spellings: BTreeSet<Keyword> = BTreeSet::new();
            let mut having = Vec::new();
            for loc in locs {
                let found = ops::label_spellings(&loc.keywords, &l.key);
                if !found.is_empty() {
                    spellings.extend(found);
                    having.push(loc.clone());
                }
            }
            let flags = spellings
                .into_iter()
                .map(|k| Flag::Keyword(k.as_str().to_owned()))
                .collect();
            Some((StoreOp::Remove, flags, having))
        }
        _ => None,
    }
}

/// STOREs the op's flags on `locs` of the selected mailbox, then reads
/// back an added label, which some servers acknowledge without keeping.
async fn store<S: ImapSession>(
    s: &mut S,
    op: &Op,
    locs: &[Loc],
) -> std::result::Result<(), OpError> {
    let Some((store_op, flags, targets)) = store_args(op, locs) else {
        return Ok(());
    };
    if targets.is_empty() || flags.is_empty() {
        return Ok(());
    }
    store_guarded(s, op, store_op, &flags, &targets).await?;
    match &op.label {
        Some(l) if op.kind == OpKind::LabelAdd => verify_kept(s, l, &targets).await,
        _ => Ok(()),
    }
}

async fn verify_kept<S: ImapSession>(
    s: &mut S,
    label: &LabelArg,
    targets: &[Loc],
) -> std::result::Result<(), OpError> {
    let metas = s.fetch(&uid_set(targets), &FetchSpec::flags()).await?;
    let dropped = metas
        .iter()
        .filter_map(message::fetched)
        .any(|f| ops::label_spellings(&f.location.keywords, &label.key).is_empty());
    if !dropped {
        return Ok(());
    }
    let name = match tagmail_core::classify_keyword(&label.keyword) {
        tagmail_core::KeywordClass::Label { name, .. } => name,
        tagmail_core::KeywordClass::Hidden => label.keyword.as_str().to_owned(),
    };
    Err(OpError::KeywordNotPersisted(name))
}

/// Guarded by `UNCHANGEDSINCE` under CONDSTORE: messages changed
/// concurrently are re-read and stored once more against their new modseq.
async fn store_guarded<S: ImapSession>(
    s: &mut S,
    op: &Op,
    store_op: StoreOp,
    flags: &[Flag],
    targets: &[Loc],
) -> std::result::Result<(), OpError> {
    let since = max_modseq(targets).filter(|_| s.capabilities().condstore());
    let result = s
        .store_flags(&uid_set(targets), store_op, flags, since)
        .await?;
    if result.modified.is_empty() {
        return Ok(());
    }
    let metas = s.fetch(&result.modified, &FetchSpec::flags()).await?;
    let mut fresh = Vec::new();
    for meta in metas {
        let Some(f) = message::fetched(&meta) else {
            continue;
        };
        if let Some(l) = targets.iter().find(|l| l.uid == f.location.uid) {
            fresh.push(Loc {
                flags: f.location.flags,
                keywords: f.location.keywords,
                modseq: f.location.modseq,
                ..l.clone()
            });
        }
    }
    let Some((store_op, flags, targets)) = store_args(op, &fresh) else {
        return Ok(());
    };
    if targets.is_empty() || flags.is_empty() {
        return Ok(());
    }
    let again = s
        .store_flags(&uid_set(&targets), store_op, &flags, max_modseq(&targets))
        .await?;
    if again.modified.is_empty() {
        Ok(())
    } else {
        Err(OpError::Failed(
            "the messages changed on the server at the same time; retry".to_owned(),
        ))
    }
}

/// The newest modseq among `locs` when all are known.
fn max_modseq(locs: &[Loc]) -> Option<ModSeq> {
    locs.iter()
        .map(|l| l.modseq)
        .collect::<Option<Vec<u64>>>()
        .and_then(|m| m.into_iter().max())
        .map(ModSeq)
}

/// The server mailbox a moving op puts messages into, created when the
/// account lacks its `Archive`, Trash or Junk folder.
async fn destination<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    op: &Op,
    mbs: &mut Mailboxes,
) -> std::result::Result<Option<String>, OpError> {
    let Some(dest) = op.destination() else {
        return Ok(None);
    };
    let found = match &dest {
        Destination::Inbox => mbs.inbox(),
        Destination::Archive => match mbs.archive_name() {
            Some(name) => Some(name),
            None if mbs.mode == ArchiveMode::MoveArchive => {
                mbs.create(s, ctx, ARCHIVE_NAME).await?;
                mbs.archive_name()
            }
            None => {
                return Err(OpError::Failed(
                    "the account has no \\All mailbox to archive into".to_owned(),
                ));
            }
        },
        Destination::Special(u) => match (mbs.by_special_use(*u), fallback_names(*u).first()) {
            (Some(name), _) => Some(name),
            (None, Some(leaf)) => {
                mbs.create(s, ctx, leaf).await?;
                mbs.by_special_use(*u)
            }
            (None, None) => None,
        },
        Destination::Mailbox(r) => ops::resolve_mailbox(r, &mbs.candidates).map(|c| c.name.clone()),
    };
    match found {
        Some(name) => Ok(Some(name)),
        None => Err(OpError::Failed(match dest {
            Destination::Special(u) => format!("the account has no {u} mailbox"),
            _ => "mailbox not found".to_owned(),
        })),
    }
}

async fn execute_op<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    op: &Op,
    mbs: &mut Mailboxes,
    sources: &mut BTreeSet<String>,
    destinations: &mut BTreeSet<String>,
) -> std::result::Result<(), OpError> {
    let locs = locations(ctx, op).await?;
    if locs.is_empty() {
        return Err(OpError::Failed(
            "the messages no longer exist on the server".to_owned(),
        ));
    }
    let dest = destination(s, ctx, op, mbs).await?;
    let mut by_mailbox: BTreeMap<i64, Vec<Loc>> = BTreeMap::new();
    for l in locs {
        by_mailbox.entry(l.mailbox).or_default().push(l);
    }
    let in_inbox: BTreeSet<i64> = by_mailbox
        .iter()
        .filter(|(m, _)| mbs.rows.iter().any(|r| r.id == **m && r.is_inbox()))
        .flat_map(|(_, ls)| ls.iter().map(|l| l.message))
        .collect();
    let mut labelled_somewhere = false;
    for (mailbox, cached) in by_mailbox {
        let Some(row) = mbs.rows.iter().find(|r| r.id == mailbox).cloned() else {
            continue;
        };
        let r = MailboxRef {
            name: row.name.clone(),
            special_use: row.special_use,
            uidvalidity: row.uidvalidity,
        };
        let Some(server) = ops::resolve_mailbox(&r, &mbs.candidates).cloned() else {
            continue;
        };
        let view = mbs.view(&server.name, server.special_use);
        let removes = op.removes_from(&view);
        let copies = op.copies_from(&view);
        if op.kind.is_move() && !removes && !copies {
            continue;
        }
        if op.kind.needs_label() && !row.keyword_capable {
            continue;
        }
        labelled_somewhere = true;
        let cached: Vec<Loc> = if copies {
            cached
                .into_iter()
                .filter(|l| !in_inbox.contains(&l.message))
                .collect()
        } else {
            cached
        };
        if cached.is_empty() {
            continue;
        }
        let sel = s.select(&server.name, SelectOptions::default()).await?;
        let trusted = sel.uid_validity.0 == row.uidvalidity && server.name == row.name;
        let targets = if trusted {
            cached
        } else {
            search_locations(s, &sel, &cached, mailbox).await?
        };
        if targets.is_empty() {
            continue;
        }
        let uids = uid_set(&targets);
        if copies {
            let to = dest.clone().unwrap_or_else(|| "INBOX".to_owned());
            s.copy_messages(&uids, &to).await?;
        } else if removes {
            if op.kind == OpKind::Delete {
                s.expunge_uids(&uids).await?;
            } else if let Some(to) = &dest {
                s.move_messages(&uids, to).await?;
            }
            if trusted {
                ctx.write(move |w| cache::remove_uids(w, mailbox, &uids).map(drop))
                    .await?;
            }
        } else {
            store(s, op, &targets).await?;
            if trusted {
                record_store(ctx, op, mailbox, &targets).await?;
            }
        }
        sources.insert(server.name.clone());
    }
    if op.kind.needs_label() && !labelled_somewhere {
        return Err(OpError::Failed(
            "the server does not allow labels on these messages".to_owned(),
        ));
    }
    if let Some(d) = dest {
        destinations.insert(d);
    }
    Ok(())
}

/// The server now holds the op's flags: record them as server state, so
/// dropping the overlay does not flicker back before the next sync.
async fn record_store(ctx: &Ctx, op: &Op, mailbox: i64, targets: &[Loc]) -> Result<()> {
    let states: Vec<crate::message::LocationState> = targets
        .iter()
        .map(|l| {
            let (mut flags, mut keywords) = (l.flags, l.keywords.clone());
            op.apply(&mut flags, &mut keywords);
            crate::message::LocationState {
                uid: l.uid,
                flags,
                keywords,
                modseq: None,
            }
        })
        .collect();
    ctx.write(move |w| cache::update_flags(w, mailbox, &states).map(drop))
        .await
}
