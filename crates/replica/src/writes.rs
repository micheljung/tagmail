//! Desktop writes (§5, §7): an op becomes a durable intent, is projected
//! over the replica at once and flushed to the server under the client's
//! op id, so a lost acknowledgement replays without a second op.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use tagmail_api_core::dto::{ChangeDto, ChangeOpDto, EntityDto, LabelDto, SpecialUseDto};
use tagmail_api_core::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelPatchDto, MailboxRefDto, NewLabelDto, OpDto, OpStatusDto,
};
use tagmail_api_core::store::BoxFuture;
use tagmail_api_core::writes::{
    OpMailbox, OpRequest, Submitted, WriteError, WriteResult, WriteService,
};
use tagmail_core::ops::{LabelArg, Op, Overlay};
use tagmail_core::{
    DEFAULT_KEYWORD_LIMIT, EmailId, Keyword, LabelKey, MailboxRef, SpecialUse, encode_label,
};
use tagmail_store::changes::{self, ChangeOp, Entity, NewChange};

use crate::client::ClientError;
use crate::error::{Error, Result};
use crate::intents::{Intent, IntentState, SENT_RECHECK_SECS};
use crate::overlay;
use crate::sync::{Replica, now};

const RETRY_BASE_SECS: i64 = 5;
const RETRY_MAX_SECS: i64 = 600;
pub const DISCARDED_ON_SERVER: &str = "discarded on the server";

/// The keyword a label intent projects: the account's spelling, else the
/// label encoded as a new keyword.
fn label_arg(
    conn: &Connection,
    account: i64,
    label: &str,
) -> tagmail_store::Result<Option<LabelArg>> {
    let key = LabelKey::of(label);
    let stored: Option<String> = conn
        .query_row(
            "SELECT keyword FROM label WHERE account_id = ?1 AND label_key = ?2 AND kind = 'keyword'",
            params![account, key.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let keyword = match stored.and_then(|k| Keyword::new(k).ok()) {
        Some(k) => k,
        None => match encode_label(label, DEFAULT_KEYWORD_LIMIT) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        },
    };
    Ok(Some(LabelArg { keyword, key }))
}

fn op_of(conn: &Connection, i: &Intent) -> tagmail_store::Result<Op> {
    let label = match &i.label {
        Some(l) => label_arg(conn, i.account_id, l)?,
        None => None,
    };
    Ok(Op {
        id: i.op_id.clone(),
        kind: i.kind,
        email_ids: i.email_ids.clone(),
        label,
        mailbox: i.mailbox.as_ref().map(|m| MailboxRef {
            name: m.name.clone(),
            special_use: m.special_use.map(Into::into),
            uidvalidity: m.uidvalidity.unwrap_or(0),
        }),
    })
}

fn special_use_dto(u: SpecialUse) -> SpecialUseDto {
    match u {
        SpecialUse::All => SpecialUseDto::All,
        SpecialUse::Archive => SpecialUseDto::Archive,
        SpecialUse::Drafts => SpecialUseDto::Drafts,
        SpecialUse::Flagged => SpecialUseDto::Flagged,
        SpecialUse::Important => SpecialUseDto::Important,
        SpecialUse::Junk => SpecialUseDto::Junk,
        SpecialUse::Sent => SpecialUseDto::Sent,
        SpecialUse::Trash => SpecialUseDto::Trash,
    }
}

/// The op as the SPA lists it, with its messages' current threads.
fn op_dto(conn: &Connection, i: &Intent) -> tagmail_store::Result<OpDto> {
    let messages = overlay::messages_of(conn, i.account_id, &i.email_ids)?;
    let mut threads = BTreeSet::new();
    let mut stmt = conn.prepare_cached("SELECT thread_id FROM message WHERE id = ?1")?;
    for m in messages {
        if let Some(t) = stmt.query_row([m], |r| r.get::<_, Option<i64>>(0))? {
            threads.insert(t);
        }
    }
    let mailbox_id = match &i.mailbox {
        Some(m) => conn
            .query_row(
                "SELECT id FROM mailbox WHERE account_id = ?1 AND name = ?2",
                params![i.account_id, m.name],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    Ok(OpDto {
        op_id: i.op_id.clone(),
        seq: i.seq,
        account_id: i.account_id,
        kind: i.kind.into(),
        status: match i.state {
            IntentState::Pending | IntentState::Sent => OpStatusDto::Pending,
            IntentState::Done => OpStatusDto::Done,
            IntentState::Failed => OpStatusDto::Failed,
        },
        error: i.error.clone(),
        attempts: i.attempts,
        thread_ids: if i.state == IntentState::Done {
            Vec::new()
        } else {
            threads.into_iter().collect()
        },
        label: i
            .label
            .as_ref()
            .map(|l| LabelKey::of(l).as_str().to_owned()),
        mailbox_id,
        created_at: i.created_at,
    })
}

fn op_change(conn: &Connection, i: &Intent) -> tagmail_store::Result<NewChange> {
    Ok(NewChange {
        entity: Entity::Op,
        entity_id: i.seq,
        op: ChangeOp::Upsert,
        payload: Some(
            serde_json::to_string(&op_dto(conn, i)?)
                .map_err(|e| tagmail_store::Error::Corrupt(e.to_string()))?,
        ),
    })
}

fn backoff(attempts: u32) -> i64 {
    (RETRY_BASE_SECS << attempts.min(16)).min(RETRY_MAX_SECS)
}

fn internal(e: Error) -> WriteError {
    WriteError::internal(e)
}

impl Replica {
    /// Re-projects the open intents over the messages of `touched`
    /// (`None`: of every open intent) and logs `extra` with the changes.
    pub(crate) async fn refresh_overlay(
        &self,
        touched: Option<Vec<Intent>>,
        extra: Vec<Intent>,
    ) -> Result<()> {
        let Some(store) = self.store() else {
            return Ok(());
        };
        let open = self
            .durable()
            .intents(&[IntentState::Pending, IntentState::Sent])?;
        let touched = touched.unwrap_or_else(|| open.clone());
        if touched.is_empty() && extra.is_empty() {
            return Ok(());
        }
        let epoch = store.cache().epoch();
        let head = store
            .cache()
            .write(move |tx| {
                let mut scope: BTreeMap<i64, BTreeSet<EmailId>> = BTreeMap::new();
                for i in &touched {
                    scope
                        .entry(i.account_id)
                        .or_default()
                        .extend(i.email_ids.iter().cloned());
                }
                let mut log = Vec::new();
                for (account, emails) in scope {
                    let ops = open
                        .iter()
                        .filter(|i| i.account_id == account)
                        .map(|i| op_of(tx, i))
                        .collect::<tagmail_store::Result<Vec<_>>>()?;
                    let messages = overlay::messages_of(tx, account, &emails)?;
                    log.extend(overlay::reproject(
                        tx,
                        account,
                        &Overlay::new(ops),
                        &messages,
                    )?);
                }
                for i in &extra {
                    log.push(op_change(tx, i)?);
                }
                changes::append_all(tx, &log)?;
                changes::head(tx, epoch)
            })
            .await?;
        self.feed().publish(head);
        Ok(())
    }

    pub async fn submit_op(&self, req: OpRequest) -> WriteResult<Submitted> {
        let durable = self.durable();
        if let Some(existing) = durable.intent(&req.op_id).map_err(internal)? {
            return Ok(Submitted::Existing(self.op_dto(existing).await?));
        }
        let store = self
            .store()
            .ok_or_else(|| WriteError::Conflict("the replica is not ready yet".into()))?;
        let threads: Vec<i64> = req.thread_ids.iter().map(|t| t.0).collect();
        let emails = req.email_ids.clone();
        let mailbox = req.mailbox.clone();
        let label = req.label.clone();
        let (account, email_ids, mailbox) = store
            .cache()
            .read(move |c| Ok(targets(c, &threads, &emails, mailbox, label.as_deref())))
            .await
            .map_err(WriteError::internal)??;
        let intent = Intent {
            op_id: req.op_id.clone(),
            seq: 0,
            kind: req.kind,
            account_id: account,
            email_ids,
            label: req.label,
            mailbox,
            created_at: now(),
            state: IntentState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
        };
        if !durable.add_intent(&intent).map_err(internal)? {
            let existing = durable.intent(&req.op_id).map_err(internal)?;
            let existing = existing.ok_or_else(|| WriteError::NotFound(req.op_id.clone()))?;
            return Ok(Submitted::Existing(self.op_dto(existing).await?));
        }
        let stored = durable
            .intent(&req.op_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::NotFound(req.op_id.clone()))?;
        self.refresh_overlay(Some(vec![stored.clone()]), vec![stored.clone()])
            .await
            .map_err(internal)?;
        self.kick_flush();
        Ok(Submitted::Created(self.op_dto(stored).await?))
    }

    async fn op_dto(&self, i: Intent) -> WriteResult<OpDto> {
        let Some(store) = self.store() else {
            return Ok(OpDto {
                op_id: i.op_id.clone(),
                seq: i.seq,
                account_id: i.account_id,
                kind: i.kind.into(),
                status: OpStatusDto::Pending,
                error: i.error.clone(),
                attempts: i.attempts,
                thread_ids: Vec::new(),
                label: i.label.clone(),
                mailbox_id: None,
                created_at: i.created_at,
            });
        };
        store
            .cache()
            .read(move |c| op_dto(c, &i))
            .await
            .map_err(WriteError::internal)
    }

    pub async fn list_ops(&self, status: Option<OpStatusDto>) -> WriteResult<Vec<OpDto>> {
        let states: &[IntentState] = match status {
            Some(OpStatusDto::Failed) => &[IntentState::Failed],
            Some(OpStatusDto::Pending) => &[IntentState::Pending, IntentState::Sent],
            _ => &[IntentState::Pending, IntentState::Sent, IntentState::Failed],
        };
        let intents = self.durable().intents(states).map_err(internal)?;
        let mut out = Vec::with_capacity(intents.len());
        for i in intents {
            out.push(self.op_dto(i).await?);
        }
        Ok(out)
    }

    pub async fn get_op(&self, op_id: String) -> WriteResult<OpDto> {
        match self.durable().intent(&op_id).map_err(internal)? {
            Some(i) => self.op_dto(i).await,
            None => Err(WriteError::NotFound(format!("no op {op_id}"))),
        }
    }

    pub async fn retry_op(&self, op_id: String) -> WriteResult<OpDto> {
        if !self.durable().retry_intent(&op_id).map_err(internal)? {
            return Err(WriteError::Conflict(
                "only failed ops can be retried".into(),
            ));
        }
        let i = self
            .durable()
            .intent(&op_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::NotFound(op_id.clone()))?;
        self.refresh_overlay(Some(vec![i.clone()]), vec![i.clone()])
            .await
            .map_err(internal)?;
        self.kick_flush();
        self.op_dto(i).await
    }

    pub async fn discard_op(&self, op_id: String) -> WriteResult<()> {
        let i = self
            .durable()
            .intent(&op_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::NotFound(format!("no op {op_id}")))?;
        if !matches!(i.state, IntentState::Pending | IntentState::Failed) {
            return Err(WriteError::Conflict(
                "the op already reached the server".into(),
            ));
        }
        self.durable().remove_intent(&op_id).map_err(internal)?;
        self.refresh_overlay(Some(vec![i.clone()]), Vec::new())
            .await
            .map_err(internal)?;
        if let Some(store) = self.store() {
            let epoch = store.cache().epoch();
            let change = NewChange {
                entity: Entity::Op,
                entity_id: i.seq,
                op: ChangeOp::Delete,
                payload: Some(json!({ "opId": op_id }).to_string()),
            };
            let head = store
                .cache()
                .write(move |tx| {
                    changes::append(tx, &change)?;
                    changes::head(tx, epoch)
                })
                .await
                .map_err(WriteError::internal)?;
            self.feed().publish(head);
        }
        Ok(())
    }

    /// Sends due pending intents. Stops quietly when the server is
    /// unreachable; they stay pending.
    pub async fn flush(&self) -> Result<()> {
        let _guard = self.flush_lock().lock().await;
        let durable = self.durable();
        let now = now();
        let mut changed = durable.expire_intents(now)?;
        let mut stop = self.recheck_sent(now, &mut changed).await?;
        let due: Vec<Intent> = durable
            .intents(&[IntentState::Pending])?
            .into_iter()
            .filter(|i| stop.is_none() && i.next_at.is_none_or(|t| t <= now))
            .collect();
        for i in due {
            let body = json!({
                "opId": i.op_id,
                "kind": tagmail_api_core::ops_dto::OpKindDto::from(i.kind),
                "emailIds": i.email_ids.iter().map(EmailId::as_str).collect::<Vec<_>>(),
                "label": i.label,
                "mailbox": i.mailbox,
            });
            match self.client().post_json::<_, OpDto>("api/ops", &body).await {
                Ok(op) => {
                    let (state, error) = match op.status {
                        OpStatusDto::Pending => (IntentState::Sent, None),
                        OpStatusDto::Done => (IntentState::Done, None),
                        OpStatusDto::Failed => (IntentState::Failed, op.error),
                    };
                    durable.advance_intent(&i.op_id, state, error.as_deref())?;
                }
                Err(ClientError::Api { status, body })
                    if (400..500).contains(&status) && status != 429 =>
                {
                    durable.advance_intent(&i.op_id, IntentState::Failed, Some(&body.message))?;
                }
                Err(
                    e @ (ClientError::Offline(_) | ClientError::Unauthorized | ClientError::Gone),
                ) => {
                    stop = Some(e);
                    break;
                }
                Err(e) => {
                    durable.defer_intent(&i.op_id, now + backoff(i.attempts), &e.to_string())?;
                }
            }
            changed.push(durable.intent(&i.op_id)?.unwrap_or(i));
        }
        if !changed.is_empty() {
            self.refresh_overlay(Some(changed.clone()), changed).await?;
        }
        let stop = match stop {
            None => match self.flush_compose().await {
                Err(Error::Client(e)) => Some(e),
                other => return other,
            },
            stop => stop,
        };
        match stop {
            Some(ClientError::Offline(_)) | None => Ok(()),
            Some(e) => Err(e.into()),
        }
    }

    /// Asks the server for the verdict of `sent` intents that are due, so
    /// one whose op change was never pulled still settles.
    async fn recheck_sent(
        &self,
        now: i64,
        changed: &mut Vec<Intent>,
    ) -> Result<Option<ClientError>> {
        let durable = self.durable();
        let due: Vec<Intent> = durable
            .intents(&[IntentState::Sent])?
            .into_iter()
            .filter(|i| i.next_at.is_none_or(|t| t <= now))
            .collect();
        for i in due {
            let path = format!(
                "api/ops/{}",
                percent_encoding::utf8_percent_encode(&i.op_id, percent_encoding::NON_ALPHANUMERIC)
            );
            let verdict = match self.client().get_json::<OpDto>(&path).await {
                Ok(op) => match op.status {
                    OpStatusDto::Pending => None,
                    OpStatusDto::Done => Some((IntentState::Done, None)),
                    OpStatusDto::Failed => Some((IntentState::Failed, op.error)),
                },
                Err(ClientError::Api { status: 404, .. } | ClientError::Status { status: 404 }) => {
                    Some((IntentState::Done, None))
                }
                Err(
                    e @ (ClientError::Offline(_) | ClientError::Unauthorized | ClientError::Gone),
                ) => return Ok(Some(e)),
                Err(e) => {
                    tracing::debug!(op = %i.op_id, error = %e, "op verdict unknown");
                    None
                }
            };
            match verdict {
                Some((state, error)) => {
                    if durable.advance_intent(&i.op_id, state, error.as_deref())? {
                        changed.extend(durable.intent(&i.op_id)?);
                    }
                }
                None => durable.recheck_at(&i.op_id, now + SENT_RECHECK_SECS)?,
            }
        }
        Ok(None)
    }

    /// Server `op` changes settle the intents with the same op id.
    pub(crate) async fn apply_server_ops(&self, writes: &[ChangeDto]) -> Result<()> {
        let durable = self.durable();
        let mut changed = Vec::new();
        for c in writes.iter().filter(|c| c.entity == EntityDto::Op) {
            let Some(payload) = &c.payload else {
                continue;
            };
            let (op_id, verdict) = match c.op {
                ChangeOpDto::Upsert => match serde_json::from_value::<OpDto>(payload.clone()) {
                    Ok(op) => {
                        let verdict = match op.status {
                            OpStatusDto::Pending => (IntentState::Sent, None),
                            OpStatusDto::Done => (IntentState::Done, None),
                            OpStatusDto::Failed => (IntentState::Failed, op.error),
                        };
                        (op.op_id, verdict)
                    }
                    Err(_) => continue,
                },
                ChangeOpDto::Delete => match payload.get("opId").and_then(|v| v.as_str()) {
                    Some(id) => (
                        id.to_owned(),
                        (IntentState::Failed, Some(DISCARDED_ON_SERVER.to_owned())),
                    ),
                    None => continue,
                },
                _ => continue,
            };
            let Some(i) = durable.intent(&op_id)? else {
                continue;
            };
            if !durable.advance_intent(&op_id, verdict.0, verdict.1.as_deref())? {
                continue;
            }
            changed.push(durable.intent(&op_id)?.unwrap_or(i));
        }
        if !changed.is_empty() {
            self.refresh_overlay(Some(changed.clone()), changed).await?;
        }
        Ok(())
    }
}

type Targets = (i64, BTreeSet<EmailId>, Option<MailboxRefDto>);

/// The account and email ids of an op's threads and messages, and its
/// destination as an IMAP-stable reference.
fn targets(
    c: &Connection,
    threads: &[i64],
    emails: &[String],
    mailbox: Option<OpMailbox>,
    label: Option<&str>,
) -> WriteResult<Targets> {
    let run = || -> tagmail_store::Result<WriteResult<Targets>> {
        let mut rows: Vec<(i64, String)> = Vec::new();
        let mut by_thread = c.prepare_cached(
            "SELECT account_id, email_id FROM message WHERE thread_id = ?1 AND tombstoned_at IS NULL",
        )?;
        for &t in threads {
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
            for row in by_thread.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))? {
                rows.push(row?);
            }
        }
        let mut by_email = c.prepare_cached(
            "SELECT account_id, email_id FROM message WHERE email_id = ?1 AND tombstoned_at IS NULL",
        )?;
        for e in emails {
            for row in by_email.query_map([e], |r| Ok((r.get(0)?, r.get(1)?)))? {
                rows.push(row?);
            }
        }
        let accounts: BTreeSet<i64> = rows.iter().map(|(a, _)| *a).collect();
        let account = match accounts.len() {
            0 => {
                return Ok(Err(WriteError::NotFound(
                    "no such threads or messages".into(),
                )));
            }
            1 => *accounts.first().unwrap_or(&0),
            _ => {
                return Ok(Err(WriteError::invalid(
                    "threadIds",
                    "the targets belong to more than one account; send one op per account",
                )));
            }
        };
        if let Some(l) = label {
            let known: bool = c
                .query_row(
                    "SELECT 1 FROM label WHERE label_key = ?1 AND kind = 'keyword'",
                    [LabelKey::of(l).as_str()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !known && label_arg(c, account, l)?.is_none() {
                return Ok(Err(WriteError::invalid("label", "not a valid label name")));
            }
        }
        let mailbox = match mailbox {
            None => None,
            Some(OpMailbox::Ref(r)) => Some(r),
            Some(OpMailbox::Id(id)) => {
                let row: Option<(i64, String, Option<String>, u32)> = c
                    .query_row(
                        "SELECT account_id, name, special_use, uidvalidity FROM mailbox WHERE id = ?1",
                        [id.0],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                match row {
                    Some((a, name, special_use, uidvalidity)) if a == account => {
                        Some(MailboxRefDto {
                            name,
                            special_use: special_use
                                .and_then(|s| s.parse::<SpecialUse>().ok())
                                .map(special_use_dto),
                            uidvalidity: Some(uidvalidity),
                        })
                    }
                    Some(_) => {
                        return Ok(Err(WriteError::invalid(
                            "mailboxId",
                            "the mailbox belongs to another account",
                        )));
                    }
                    None => return Ok(Err(WriteError::invalid("mailboxId", "no such mailbox"))),
                }
            }
        };
        let ids = rows
            .into_iter()
            .filter_map(|(_, e)| EmailId::parse(&e).ok())
            .collect();
        Ok(Ok((account, ids, mailbox)))
    };
    run().map_err(WriteError::internal)?
}

/// [`WriteService`] of the desktop over the replica's intents. Label
/// management needs the server and is passed through before it gets here.
pub struct DesktopWrites(pub Arc<Replica>);

fn server_only<T: Send + 'static>() -> BoxFuture<WriteResult<T>> {
    Box::pin(async {
        Err(WriteError::Conflict(
            "labels are managed on the server".into(),
        ))
    })
}

impl WriteService for DesktopWrites {
    fn submit_op(&self, req: OpRequest) -> BoxFuture<WriteResult<Submitted>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.submit_op(req).await })
    }

    fn ops(&self, status: Option<OpStatusDto>) -> BoxFuture<WriteResult<Vec<OpDto>>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.list_ops(status).await })
    }

    fn op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.get_op(op_id).await })
    }

    fn retry_op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.retry_op(op_id).await })
    }

    fn discard_op(&self, op_id: String) -> BoxFuture<WriteResult<()>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.discard_op(op_id).await })
    }

    fn create_label(&self, _: NewLabelDto) -> BoxFuture<WriteResult<LabelDto>> {
        server_only()
    }

    fn update_label(&self, _: String, _: LabelPatchDto) -> BoxFuture<WriteResult<LabelChangeDto>> {
        server_only()
    }

    fn delete_label(&self, _: String) -> BoxFuture<WriteResult<LabelChangeDto>> {
        server_only()
    }

    fn label_jobs(&self) -> BoxFuture<WriteResult<Vec<LabelJobDto>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), 5);
        assert_eq!(backoff(1), 10);
        assert_eq!(backoff(30), RETRY_MAX_SECS);
    }
}
