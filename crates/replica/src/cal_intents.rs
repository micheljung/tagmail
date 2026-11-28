//! Calendar writes of the desktop (appointments plan §4 M-C step 6): an
//! RSVP, import or apply becomes a durable `cal_intent` at once, even
//! offline, and is flushed to the server's route under the client's op id,
//! so a lost acknowledgement replays without a second op.

use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Map, Value};
use tagmail_api_core::calendar_dto::{
    CalApplyDto, CalEditDto, CalEditKindDto, CalImportDto, CalRsvpDto,
};
use tagmail_api_core::dto::{ChangeDto, ChangeOpDto, EntityDto};
use tagmail_api_core::ops_dto::{CalOpInfoDto, OpDomainDto, OpDto, OpKindDto, OpStatusDto};
use tagmail_api_core::writes::{Submitted, WriteError, WriteResult, valid_op_id};
use tagmail_store::changes::{self, ChangeOp, Entity, NewChange};

use crate::client::ClientError;
use crate::durable::Durable;
use crate::error::{Error, Result};
use crate::intents::{
    DONE_RETENTION_SECS, EXPIRED, INTENT_TTL_SECS, IntentState, NO_VERDICT, SENT_RECHECK_SECS,
    SENT_TTL_SECS,
};
use crate::router::NEEDS_CONNECTION;
use crate::sync::{Replica, now};
use crate::writes::{DISCARDED_ON_SERVER, NOT_ON_SERVER, backoff};

const MESSAGE_ID: &str = "messageId";
const OP_ID: &str = "opId";
const ACCOUNT_ID: &str = "accountId";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalKind {
    Rsvp,
    Import,
    Apply,
    /// A new, edited, deleted or moved event; it names no message.
    Edit,
}

impl CalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CalKind::Rsvp => "rsvp",
            CalKind::Import => "import",
            CalKind::Apply => "apply",
            CalKind::Edit => "edit",
        }
    }

    fn parse(s: &str) -> Result<CalKind> {
        Ok(match s {
            "rsvp" => CalKind::Rsvp,
            "import" => CalKind::Import,
            "apply" => CalKind::Apply,
            "edit" => CalKind::Edit,
            other => return Err(Error::Corrupt(format!("cal_intent kind {other:?}"))),
        })
    }

    /// The server route the intent is flushed to.
    pub fn path(self) -> &'static str {
        match self {
            CalKind::Rsvp => "api/calendar/rsvp",
            CalKind::Import => "api/calendar/import",
            CalKind::Apply => "api/calendar/apply",
            CalKind::Edit => "api/calendar/ops",
        }
    }

    fn op_kind(self, payload: &Map<String, Value>) -> OpKindDto {
        let edit = || serde_json::from_value(payload.get("kind").cloned().unwrap_or_default());
        match self {
            CalKind::Rsvp => OpKindDto::CalRsvp,
            CalKind::Import => OpKindDto::CalImport,
            CalKind::Apply => OpKindDto::CalApply,
            CalKind::Edit => match edit() {
                Ok(CalEditKindDto::Delete) => OpKindDto::CalDelete,
                Ok(CalEditKindDto::Move) => OpKindDto::CalMove,
                _ => OpKindDto::CalPut,
            },
        }
    }

    /// Checks `body` against the route's request shape.
    fn validate(self, body: &[u8]) -> std::result::Result<(), serde_json::Error> {
        match self {
            CalKind::Rsvp => serde_json::from_slice::<CalRsvpDto>(body).map(drop),
            CalKind::Import => serde_json::from_slice::<CalImportDto>(body).map(drop),
            CalKind::Apply => serde_json::from_slice::<CalApplyDto>(body).map(drop),
            CalKind::Edit => serde_json::from_slice::<CalEditDto>(body).map(drop),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalIntent {
    pub op_id: String,
    pub seq: i64,
    pub kind: CalKind,
    pub account_id: i64,
    pub email_id: String,
    /// The request without its `messageId`.
    pub payload: Map<String, Value>,
    pub created_at: i64,
    pub state: IntentState,
    pub attempts: u32,
    pub next_at: Option<i64>,
    pub error: Option<String>,
    /// What the server reported the op writes.
    pub calendar: Option<CalOpInfoDto>,
    /// The server holds the op: retry and discard go to it.
    pub on_server: bool,
}

impl CalIntent {
    /// As the SPA lists it, beside the mail ops.
    pub fn op_dto(&self) -> OpDto {
        OpDto {
            op_id: self.op_id.clone(),
            seq: self.seq,
            account_id: self.account_id,
            kind: self.kind.op_kind(&self.payload),
            status: match self.state {
                IntentState::Pending | IntentState::Sent => OpStatusDto::Pending,
                IntentState::Done => OpStatusDto::Done,
                IntentState::Failed => OpStatusDto::Failed,
            },
            error: self.error.clone(),
            attempts: self.attempts,
            thread_ids: Vec::new(),
            label: None,
            mailbox_id: None,
            created_at: self.created_at,
            domain: OpDomainDto::Cal,
            calendar: self.calendar.clone(),
        }
    }

    fn change(&self) -> NewChange {
        NewChange {
            entity: Entity::CalOp,
            entity_id: self.seq,
            op: ChangeOp::Upsert,
            payload: serde_json::to_string(&self.op_dto()).ok(),
        }
    }
}

const COLUMNS: &str = "op_id, seq, kind, account_id, email_id, payload, created_at, state, \
     attempts, next_at, error, calendar, on_server";

fn decode(r: &Row<'_>) -> rusqlite::Result<(CalIntent, String, String)> {
    let kind: String = r.get(2)?;
    let state: String = r.get(7)?;
    Ok((
        CalIntent {
            op_id: r.get(0)?,
            seq: r.get(1)?,
            kind: CalKind::Rsvp,
            account_id: r.get(3)?,
            email_id: r.get(4)?,
            payload: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            created_at: r.get(6)?,
            state: IntentState::Pending,
            attempts: r.get(8)?,
            next_at: r.get(9)?,
            error: r.get(10)?,
            calendar: r
                .get::<_, Option<String>>(11)?
                .and_then(|c| serde_json::from_str(&c).ok()),
            on_server: r.get(12)?,
        },
        kind,
        state,
    ))
}

fn finish((mut i, kind, state): (CalIntent, String, String)) -> Result<CalIntent> {
    i.kind = CalKind::parse(&kind)?;
    i.state = IntentState::parse(&state)?;
    Ok(i)
}

impl Durable {
    /// Inserts unless the op id exists; returns whether it was new.
    pub fn add_cal_intent(&self, i: &CalIntent) -> Result<bool> {
        let payload =
            serde_json::to_string(&i.payload).map_err(|e| Error::Corrupt(e.to_string()))?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let known = tx
            .query_row(
                "SELECT 1 FROM cal_intent WHERE op_id = ?1",
                [&i.op_id],
                |_| Ok(()),
            )
            .optional()?;
        if known.is_some() {
            return Ok(false);
        }
        let seq = crate::durable::next_op_seq(&tx)?;
        tx.execute(
            "INSERT INTO cal_intent (op_id, seq, kind, account_id, email_id, payload, \
             created_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                i.op_id,
                seq,
                i.kind.as_str(),
                i.account_id,
                i.email_id,
                payload,
                i.created_at,
                i.state.as_str()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn cal_intent(&self, op_id: &str) -> Result<Option<CalIntent>> {
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM cal_intent WHERE op_id = ?1"),
                [op_id],
                decode,
            )
            .optional()?;
        row.map(finish).transpose()
    }

    /// Calendar intents in these states, oldest first.
    pub fn cal_intents(&self, states: &[IntentState]) -> Result<Vec<CalIntent>> {
        let wanted: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let wanted = serde_json::to_string(&wanted).map_err(|e| Error::Corrupt(e.to_string()))?;
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM cal_intent WHERE state IN (SELECT value FROM json_each(?1)) \
             ORDER BY seq"
        ))?;
        let rows = stmt
            .query_map([wanted], decode)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(finish).collect()
    }

    /// Moves a calendar intent forward only, as
    /// [`Durable::advance_intent`]. Whether it moved.
    pub fn advance_cal_intent(
        &self,
        op_id: &str,
        state: IntentState,
        error: Option<&str>,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE cal_intent SET state = ?2, error = ?3,
                 next_at = CASE WHEN ?2 = 'sent' THEN ?4 + ?5 END,
                 sent_at = CASE WHEN ?2 = 'sent' THEN ?4 ELSE sent_at END
             WHERE op_id = ?1
               AND (state = 'pending' AND ?2 != 'pending'
                    OR state = 'sent' AND ?2 IN ('done', 'failed'))",
            params![op_id, state.as_str(), error, now(), SENT_RECHECK_SECS],
        )? == 1)
    }

    /// Takes the server's view of the op, which is authoritative once it
    /// holds it: a retry there may turn a failed op pending again.
    /// Whether the intent exists.
    pub fn settle_cal_intent(&self, op: &OpDto) -> Result<bool> {
        let (state, error) = verdict(op);
        let calendar = op
            .calendar
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| Error::Corrupt(e.to_string()))?;
        let now = now();
        Ok(self.conn().execute(
            "UPDATE cal_intent SET state = ?2, error = ?3, calendar = ?4, on_server = 1,
                 attempts = ?5,
                 next_at = CASE WHEN ?2 = 'sent' THEN ?6 + ?7 END,
                 sent_at = CASE WHEN ?2 = 'sent' AND state != 'sent' THEN ?6 ELSE sent_at END
             WHERE op_id = ?1",
            params![
                op.op_id,
                state.as_str(),
                error,
                calendar,
                op.attempts,
                now,
                SENT_RECHECK_SECS
            ],
        )? == 1)
    }

    /// The server no longer has the op.
    fn forget_on_server(&self, op_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE cal_intent SET on_server = 0 WHERE op_id = ?1",
            [op_id],
        )?;
        Ok(())
    }

    fn defer_cal_intent(&self, op_id: &str, next_at: i64, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE cal_intent SET attempts = attempts + 1, next_at = ?2, error = ?3
             WHERE op_id = ?1 AND state = 'pending'",
            params![op_id, next_at, error],
        )?;
        Ok(())
    }

    fn recheck_cal_at(&self, op_id: &str, next_at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE cal_intent SET next_at = ?2 WHERE op_id = ?1 AND state = 'sent'",
            params![op_id, next_at],
        )?;
        Ok(())
    }

    pub fn retry_cal_intent(&self, op_id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE cal_intent SET state = 'pending', attempts = 0, next_at = NULL, error = NULL, \
             created_at = max(created_at, ?2) WHERE op_id = ?1 AND state = 'failed'",
            params![op_id, now() - INTENT_TTL_SECS + 1],
        )? == 1)
    }

    pub fn remove_cal_intent(&self, op_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM cal_intent WHERE op_id = ?1", [op_id])?
            == 1)
    }

    /// Fails every open calendar intent with `error`; returns them.
    pub fn fail_cal_intents(&self, error: &str) -> Result<Vec<CalIntent>> {
        let mut failed = Vec::new();
        for i in self.cal_intents(&[IntentState::Pending, IntentState::Sent])? {
            if self.advance_cal_intent(&i.op_id, IntentState::Failed, Some(error))? {
                failed.extend(self.cal_intent(&i.op_id)?);
            }
        }
        Ok(failed)
    }

    /// As [`Durable::expire_intents`], for calendar intents.
    fn expire_cal_intents(&self, now: i64) -> Result<Vec<CalIntent>> {
        let mut expired = Vec::new();
        for i in self.cal_intents(&[IntentState::Pending, IntentState::Sent])? {
            let sent_at: Option<i64> = self.conn().query_row(
                "SELECT sent_at FROM cal_intent WHERE op_id = ?1",
                [&i.op_id],
                |r| r.get(0),
            )?;
            let error = match i.state {
                IntentState::Pending if i.created_at <= now - INTENT_TTL_SECS => EXPIRED,
                IntentState::Sent if sent_at.unwrap_or(i.created_at) <= now - SENT_TTL_SECS => {
                    NO_VERDICT
                }
                _ => continue,
            };
            if self.advance_cal_intent(&i.op_id, IntentState::Failed, Some(error))? {
                expired.extend(self.cal_intent(&i.op_id)?);
            }
        }
        self.conn().execute(
            "DELETE FROM cal_intent WHERE state = 'done' AND created_at <= ?1",
            [now - DONE_RETENTION_SECS],
        )?;
        Ok(expired)
    }
}

fn verdict(op: &OpDto) -> (IntentState, Option<String>) {
    match op.status {
        OpStatusDto::Pending => (IntentState::Sent, None),
        OpStatusDto::Done => (IntentState::Done, None),
        OpStatusDto::Failed => (IntentState::Failed, op.error.clone()),
    }
}

fn internal(e: Error) -> WriteError {
    WriteError::internal(e)
}

fn op_path(op_id: &str) -> String {
    format!(
        "api/ops/{}",
        percent_encoding::utf8_percent_encode(op_id, percent_encoding::NON_ALPHANUMERIC)
    )
}

/// A failed retry or discard on the server, as the SPA should see it.
fn server_error(e: ClientError) -> WriteError {
    match e {
        ClientError::Api { status: 404, body } => WriteError::NotFound(body.message),
        ClientError::Api { status, body } if (400..500).contains(&status) => {
            WriteError::Conflict(body.message)
        }
        ClientError::Offline(_) => WriteError::Conflict(NEEDS_CONNECTION.into()),
        e => WriteError::internal(e),
    }
}

impl Replica {
    /// Queues `body`, a request to the route of `kind`, as a calendar
    /// intent of the message it names.
    pub async fn submit_cal(&self, kind: CalKind, body: &[u8]) -> WriteResult<Submitted> {
        if let Err(e) = kind.validate(body) {
            return Err(WriteError::invalid("body", e.to_string()));
        }
        let mut payload: Map<String, Value> =
            serde_json::from_slice(body).map_err(|e| WriteError::invalid("body", e.to_string()))?;
        let op_id = payload
            .get(OP_ID)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !valid_op_id(&op_id) {
            return Err(WriteError::invalid(
                "opId",
                "1 to 64 characters of letters, digits, '-', '_' or '.'",
            ));
        }
        let durable = self.durable();
        if let Some(existing) = durable.cal_intent(&op_id).map_err(internal)? {
            return Ok(Submitted::Existing(existing.op_dto()));
        }
        if kind == CalKind::Edit {
            let account_id = payload
                .get(ACCOUNT_ID)
                .and_then(Value::as_i64)
                .unwrap_or_default();
            return self
                .queue_cal(kind, op_id, account_id, String::new(), payload)
                .await;
        }
        let message = payload
            .remove(MESSAGE_ID)
            .and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            .ok_or_else(|| WriteError::invalid("messageId", "not a message id"))?;
        let store = self
            .store()
            .ok_or_else(|| WriteError::Conflict("the replica is not ready yet".into()))?;
        let found: Option<(i64, String, bool)> = store
            .cache()
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT account_id, email_id, has_calendar FROM message \
                     WHERE id = ?1 AND tombstoned_at IS NULL",
                    [message],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
            })
            .await
            .map_err(WriteError::internal)?;
        let Some((account_id, email_id, has_calendar)) = found else {
            return Err(WriteError::NotFound(format!("no message {message}")));
        };
        if !has_calendar {
            return Err(WriteError::invalid(
                "messageId",
                "the message has no invitation",
            ));
        }
        self.queue_cal(kind, op_id, account_id, email_id, payload)
            .await
    }

    async fn queue_cal(
        &self,
        kind: CalKind,
        op_id: String,
        account_id: i64,
        email_id: String,
        payload: Map<String, Value>,
    ) -> WriteResult<Submitted> {
        let durable = self.durable();
        let intent = CalIntent {
            op_id: op_id.clone(),
            seq: 0,
            kind,
            account_id,
            email_id,
            payload,
            created_at: now(),
            state: IntentState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
            calendar: None,
            on_server: false,
        };
        let created = durable.add_cal_intent(&intent).map_err(internal)?;
        let stored = durable
            .cal_intent(&op_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::NotFound(op_id.clone()))?;
        if !created {
            return Ok(Submitted::Existing(stored.op_dto()));
        }
        self.log_cal(vec![stored.clone()]).await.map_err(internal)?;
        self.kick_flush();
        Ok(Submitted::Created(stored.op_dto()))
    }

    /// Logs the intents' states as local `cal_op` changes.
    pub(crate) async fn log_cal(&self, intents: Vec<CalIntent>) -> Result<()> {
        let Some(store) = self.store() else {
            return Ok(());
        };
        if intents.is_empty() {
            return Ok(());
        }
        let epoch = store.cache().epoch();
        let head = store
            .cache()
            .write(move |tx| {
                let log: Vec<NewChange> = intents.iter().map(CalIntent::change).collect();
                changes::append_all(tx, &log)?;
                changes::head(tx, epoch)
            })
            .await?;
        self.feed().publish(head);
        Ok(())
    }

    /// The local id of the intent's message, or `None` once it is gone.
    async fn message_of(&self, i: &CalIntent) -> Result<Option<i64>> {
        let Some(store) = self.store() else {
            return Ok(None);
        };
        let (account, email) = (i.account_id, i.email_id.clone());
        Ok(store
            .cache()
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT id FROM message WHERE account_id = ?1 AND email_id = ?2 \
                     AND tombstoned_at IS NULL ORDER BY id LIMIT 1",
                    params![account, email],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await?)
    }

    /// Sends due calendar intents and rechecks sent ones; the error that
    /// stopped it, if the server is unreachable.
    pub(crate) async fn flush_cal(&self) -> Result<Option<ClientError>> {
        let durable = self.durable();
        let now = now();
        let mut changed = durable.expire_cal_intents(now)?;
        let sent: Vec<CalIntent> = durable
            .cal_intents(&[IntentState::Sent])?
            .into_iter()
            .filter(|i| i.next_at.is_none_or(|t| t <= now))
            .collect();
        for i in sent {
            let verdict = match self.client().get_json::<OpDto>(&op_path(&i.op_id)).await {
                Ok(op) => {
                    durable.settle_cal_intent(&op)?;
                    changed.extend(durable.cal_intent(&i.op_id)?);
                    continue;
                }
                Err(ClientError::Api { status: 404, .. } | ClientError::Status { status: 404 }) => {
                    durable.forget_on_server(&i.op_id)?;
                    Some((IntentState::Failed, Some(NOT_ON_SERVER.to_owned())))
                }
                Err(
                    e @ (ClientError::Offline(_) | ClientError::Unauthorized | ClientError::Gone),
                ) => {
                    self.log_cal(changed).await?;
                    return Ok(Some(e));
                }
                Err(e) => {
                    tracing::debug!(error = %e, "calendar op verdict unknown");
                    None
                }
            };
            match verdict {
                Some((state, error)) => {
                    if durable.advance_cal_intent(&i.op_id, state, error.as_deref())? {
                        changed.extend(durable.cal_intent(&i.op_id)?);
                    }
                }
                None => durable.recheck_cal_at(&i.op_id, now + SENT_RECHECK_SECS)?,
            }
        }
        let due: Vec<CalIntent> = durable
            .cal_intents(&[IntentState::Pending])?
            .into_iter()
            .filter(|i| i.next_at.is_none_or(|t| t <= now))
            .collect();
        let mut stop = None;
        for i in due {
            let mut body = i.payload.clone();
            if i.kind != CalKind::Edit {
                let Some(message) = self.message_of(&i).await? else {
                    durable.advance_cal_intent(
                        &i.op_id,
                        IntentState::Failed,
                        Some(NOT_ON_SERVER),
                    )?;
                    changed.extend(durable.cal_intent(&i.op_id)?);
                    continue;
                };
                body.insert(MESSAGE_ID.to_owned(), Value::String(message.to_string()));
            }
            match self
                .client()
                .post_json::<_, OpDto>(i.kind.path(), &Value::Object(body))
                .await
            {
                Ok(op) => {
                    durable.settle_cal_intent(&op)?;
                }
                Err(ClientError::Api { status, body })
                    if (400..500).contains(&status) && status != 429 =>
                {
                    durable.advance_cal_intent(
                        &i.op_id,
                        IntentState::Failed,
                        Some(&body.message),
                    )?;
                }
                Err(
                    e @ (ClientError::Offline(_) | ClientError::Unauthorized | ClientError::Gone),
                ) => {
                    stop = Some(e);
                    break;
                }
                Err(e) => {
                    durable.defer_cal_intent(
                        &i.op_id,
                        now + backoff(i.attempts),
                        &e.to_string(),
                    )?;
                }
            }
            changed.extend(durable.cal_intent(&i.op_id)?);
        }
        self.log_cal(changed).await?;
        Ok(stop)
    }

    /// Retries a failed intent: on the server once it holds the op, else by
    /// sending it again.
    pub(crate) async fn retry_cal(&self, i: CalIntent) -> WriteResult<OpDto> {
        let durable = self.durable();
        if i.on_server {
            let op: OpDto = self
                .client()
                .post_json(&format!("{}/retry", op_path(&i.op_id)), &Map::new())
                .await
                .map_err(server_error)?;
            durable.settle_cal_intent(&op).map_err(internal)?;
        } else if !durable.retry_cal_intent(&i.op_id).map_err(internal)? {
            return Err(WriteError::Conflict(
                "only failed ops can be retried".into(),
            ));
        }
        let i = durable
            .cal_intent(&i.op_id)
            .map_err(internal)?
            .ok_or_else(|| WriteError::NotFound(i.op_id.clone()))?;
        self.log_cal(vec![i.clone()]).await.map_err(internal)?;
        self.kick_flush();
        Ok(i.op_dto())
    }

    /// Drops an intent, and the server's op once it holds it.
    pub(crate) async fn discard_cal(&self, i: CalIntent) -> WriteResult<()> {
        if i.on_server {
            let builder = self
                .client()
                .request(reqwest::Method::DELETE, &op_path(&i.op_id))
                .map_err(server_error)?;
            match self.client().send(builder).await {
                Ok(_) | Err(ClientError::Api { status: 404, .. }) => {}
                Err(e) => return Err(server_error(e)),
            }
        } else if i.state != IntentState::Pending && i.state != IntentState::Failed {
            return Err(WriteError::Conflict(
                "the op already reached the server".into(),
            ));
        }
        self.durable()
            .remove_cal_intent(&i.op_id)
            .map_err(internal)?;
        let Some(store) = self.store() else {
            return Ok(());
        };
        let epoch = store.cache().epoch();
        let change = NewChange {
            entity: Entity::CalOp,
            entity_id: i.seq,
            op: ChangeOp::Delete,
            payload: Some(serde_json::json!({ OP_ID: i.op_id }).to_string()),
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
        Ok(())
    }

    /// Server `cal_op` changes settle the intents with the same op id.
    pub(crate) async fn apply_server_cal_ops(&self, writes: &[ChangeDto]) -> Result<()> {
        let durable = self.durable();
        let mut changed = Vec::new();
        for c in writes.iter().filter(|c| c.entity == EntityDto::CalOp) {
            let Some(payload) = &c.payload else {
                continue;
            };
            match c.op {
                ChangeOpDto::Upsert => {
                    let Ok(op) = serde_json::from_value::<OpDto>(payload.clone()) else {
                        continue;
                    };
                    if durable.settle_cal_intent(&op)? {
                        changed.extend(durable.cal_intent(&op.op_id)?);
                    }
                }
                ChangeOpDto::Delete => {
                    let Some(op_id) = payload.get(OP_ID).and_then(Value::as_str) else {
                        continue;
                    };
                    durable.forget_on_server(op_id)?;
                    let failed = durable.advance_cal_intent(
                        op_id,
                        IntentState::Failed,
                        Some(DISCARDED_ON_SERVER),
                    )?;
                    if failed {
                        changed.extend(durable.cal_intent(op_id)?);
                    }
                }
                _ => {}
            }
        }
        self.log_cal(changed).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(op_id: &str) -> CalIntent {
        CalIntent {
            op_id: op_id.into(),
            seq: 0,
            kind: CalKind::Rsvp,
            account_id: 1,
            email_id: "g:1".into(),
            payload: serde_json::from_str(r#"{"opId":"x","partstat":"accepted"}"#).unwrap(),
            created_at: now(),
            state: IntentState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
            calendar: None,
            on_server: false,
        }
    }

    #[test]
    fn round_trips_once_per_op_id_and_only_moves_forward() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        assert!(d.add_cal_intent(&intent("a")).unwrap());
        assert!(!d.add_cal_intent(&intent("a")).unwrap());
        let a = d.cal_intent("a").unwrap().unwrap();
        assert_eq!(
            a,
            CalIntent {
                seq: 1,
                ..intent("a")
            }
        );
        assert_eq!(a.op_dto().domain, OpDomainDto::Cal);

        assert!(d.advance_cal_intent("a", IntentState::Sent, None).unwrap());
        assert!(
            !d.advance_cal_intent("a", IntentState::Pending, None)
                .unwrap()
        );
        assert!(d.advance_cal_intent("a", IntentState::Done, None).unwrap());
        assert!(
            !d.advance_cal_intent("a", IntentState::Failed, None)
                .unwrap()
        );
    }

    #[test]
    fn failing_open_intents_leaves_settled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let d = Durable::open(&dir.path().join("d.db")).unwrap();
        d.add_cal_intent(&intent("open")).unwrap();
        d.add_cal_intent(&intent("done")).unwrap();
        d.advance_cal_intent("done", IntentState::Done, None)
            .unwrap();

        let failed = d.fail_cal_intents("off").unwrap();

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].error.as_deref(), Some("off"));
        assert_eq!(
            d.cal_intent("done").unwrap().unwrap().state,
            IntentState::Done
        );
    }
}
