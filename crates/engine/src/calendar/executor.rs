//! Calendar ops (appointments plan §4 M-C step 1): `cal_op` rows written by
//! the API and the invitation runner, run by the calendar runner like the
//! mail executor runs `pending_op` (backoff, [`MAX_ATTEMPTS`], op events).
//! They are listed, retried and discarded through the mail op endpoints,
//! as `OpDto`s with `domain: "cal"`.
//!
//! A 412 on an op that only sets my PARTSTAT (`rsvp`, `apply`) is rebased
//! onto the server copy up to [`MAX_REBASES`] times. On `put`, `move` and
//! `delete` the server wins: its copy is cached, the rejected object is
//! kept, and the op fails with `server_won` until it is re-applied (retry,
//! over the server copy's ETag) or discarded.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tagmail_api_core::WriteError;
use tagmail_api_core::ops_dto::{
    CalConflictDto, CalOpInfoDto, CalReplyStateDto, OpDomainDto, OpDto, OpKindDto, OpStatusDto,
};
use tagmail_api_core::{WriteResult, itip_auth};
use tagmail_calendar::{Calendar, ParticipationStatus};
use tagmail_core::AccountId;
use tagmail_core::ops::OpState;
use tagmail_dav::{DavError, PutCondition, Resource};
use tagmail_store::calendar;
use tagmail_store::rusqlite::{self, Connection, OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, NewChange, changes};
use time::OffsetDateTime;
use url::Url;

use super::sync::{Window, href, parse};
use super::{CalAccount, CalDav};
use crate::compose::{self, GeneratedMessage, queue_generated};
use crate::executor::{MAX_ATTEMPTS, OP_SEEN_SECS, backoff, now_secs};
use crate::supervisor::Stores;

/// 412s an RSVP or apply is rebased over before it fails.
pub(crate) const MAX_REBASES: usize = 3;
/// Calendar objects of ops, under the outbox directory.
pub(crate) const ICS_DIR: &str = crate::compose::CAL_OP_DIR;
/// `op_seen.kind` of calendar ops.
pub(crate) const CAL_OP: &str = "cal";
/// The outbox op id suffix of an RSVP's REPLY.
pub(crate) const REPLY_SUFFIX: &str = "#reply";
const SERVER_WON: &str = "server_won";
const ICS_EXTENSION: &str = "ics";
const CONFLICT_EXTENSION: &str = "conflict.ics";
/// Ops run per poll.
const BATCH: usize = 50;
/// SCHEDULE-STATUS classes that mean the server delivered the REPLY
/// (RFC 6638 §3.2.9: 1.x pending/sent/delivered, 2.x success).
const DELIVERED_CLASSES: [&str; 2] = ["1.", "2."];
/// HTTP statuses worth retrying.
const SERVER_ERROR: u16 = 500;
const TOO_MANY_REQUESTS: u16 = 429;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalOpKind {
    Put,
    Delete,
    Move,
    Rsvp,
    Import,
    Apply,
}

impl CalOpKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CalOpKind::Put => "put",
            CalOpKind::Delete => "delete",
            CalOpKind::Move => "move",
            CalOpKind::Rsvp => "rsvp",
            CalOpKind::Import => "import",
            CalOpKind::Apply => "apply",
        }
    }

    fn parse(text: &str) -> Option<CalOpKind> {
        [
            CalOpKind::Put,
            CalOpKind::Delete,
            CalOpKind::Move,
            CalOpKind::Rsvp,
            CalOpKind::Import,
            CalOpKind::Apply,
        ]
        .into_iter()
        .find(|k| k.as_str() == text)
    }

    fn dto(self) -> OpKindDto {
        match self {
            CalOpKind::Put => OpKindDto::CalPut,
            CalOpKind::Delete => OpKindDto::CalDelete,
            CalOpKind::Move => OpKindDto::CalMove,
            CalOpKind::Rsvp => OpKindDto::CalRsvp,
            CalOpKind::Import => OpKindDto::CalImport,
            CalOpKind::Apply => OpKindDto::CalApply,
        }
    }
}

/// A `cal_op` row.
#[derive(Debug, Clone)]
pub(crate) struct CalOp {
    pub op_id: String,
    /// The source account: the invitation's, whose identity replies.
    pub account: AccountId,
    /// The account whose calendar is written.
    pub target: AccountId,
    pub calendar_href: Option<String>,
    pub uid: String,
    pub recurrence_id: Option<String>,
    pub kind: CalOpKind,
    /// Upper case, as stored in iCalendar.
    pub partstat: Option<String>,
    pub ics_file: Option<String>,
    pub if_match: Option<String>,
    pub reply_op_id: Option<String>,
    pub state: OpState,
    pub attempts: u32,
    pub next_at: Option<i64>,
    pub error: Option<String>,
    pub server_won: bool,
    pub conflict_ics_file: Option<String>,
    pub created_at: i64,
    pub seq: i64,
    /// My calendar address in the event.
    pub attendee: Option<String>,
    pub identity_id: Option<i64>,
    /// The resource written; `None` until known.
    pub href: Option<String>,
    /// The calendar a move goes to.
    pub dest_href: Option<String>,
    pub reply_cancelled: bool,
    /// My PARTSTAT before an RSVP wrote its answer.
    pub prior_partstat: Option<String>,
}

impl CalOp {
    /// A new pending op.
    pub(crate) fn new(op_id: String, account: AccountId, kind: CalOpKind, uid: String) -> CalOp {
        CalOp {
            op_id,
            account,
            target: account,
            calendar_href: None,
            uid,
            recurrence_id: None,
            kind,
            partstat: None,
            ics_file: None,
            if_match: None,
            reply_op_id: None,
            state: OpState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
            server_won: false,
            conflict_ics_file: None,
            created_at: now_secs(),
            seq: crate::executor::next_seq(),
            attendee: None,
            identity_id: None,
            href: None,
            dest_href: None,
            reply_cancelled: false,
            prior_partstat: None,
        }
    }
}

const COLUMNS: &str = "op_id, account_id, target_account_id, calendar_href, uid, recurrence_id, \
     kind, partstat, ics_file, if_match, reply_op_id, state, attempts, next_at, error, \
     conflict_kind, conflict_ics_file, created_at, op_seq, attendee, identity_id, href, dest_href, \
     reply_cancelled, prior_partstat";

fn corrupt(what: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("cal_op: {what}").into(),
    )
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CalOp> {
    let kind: String = r.get(6)?;
    let state: String = r.get(11)?;
    Ok(CalOp {
        op_id: r.get(0)?,
        account: AccountId(r.get(1)?),
        target: AccountId(r.get(2)?),
        calendar_href: r.get(3)?,
        uid: r.get(4)?,
        recurrence_id: r.get(5)?,
        kind: CalOpKind::parse(&kind).ok_or_else(|| corrupt(&kind))?,
        partstat: r.get(7)?,
        ics_file: r.get(8)?,
        if_match: r.get(9)?,
        reply_op_id: r.get(10)?,
        state: state.parse().map_err(corrupt)?,
        attempts: r.get(12)?,
        next_at: r.get(13)?,
        error: r.get(14)?,
        server_won: r.get::<_, Option<String>>(15)?.as_deref() == Some(SERVER_WON),
        conflict_ics_file: r.get(16)?,
        created_at: r.get(17)?,
        seq: r.get(18)?,
        attendee: r.get(19)?,
        identity_id: r.get(20)?,
        href: r.get(21)?,
        dest_href: r.get(22)?,
        reply_cancelled: r.get(23)?,
        prior_partstat: r.get(24)?,
    })
}

pub(crate) fn load(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<CalOp>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM cal_op WHERE {filter} ORDER BY op_seq"
    ))?;
    stmt.query_map(params, row)?.collect()
}

pub(crate) fn load_one(conn: &Connection, op_id: &str) -> rusqlite::Result<Option<CalOp>> {
    Ok(load(conn, "op_id = ?1", [op_id])?.pop())
}

/// Whether `op_id` was used before, and by which domain.
pub(crate) fn seen_kind(conn: &Connection, op_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT kind FROM op_seen WHERE op_id = ?1", [op_id], |r| {
        r.get(0)
    })
    .optional()
}

/// Inserts `op` with its `op_seen` row; `false` when the id was seen.
pub(crate) fn insert(conn: &Connection, op: &CalOp) -> rusqlite::Result<bool> {
    let fresh = conn.execute(
        "INSERT OR IGNORE INTO op_seen (op_id, account_id, kind) VALUES (?1, ?2, ?3)",
        params![op.op_id, op.account.0, CAL_OP],
    )? == 1;
    if !fresh {
        return Ok(false);
    }
    conn.execute(
        &format!(
            "INSERT INTO cal_op ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
             ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)"
        ),
        params![
            op.op_id,
            op.account.0,
            op.target.0,
            op.calendar_href,
            op.uid,
            op.recurrence_id,
            op.kind.as_str(),
            op.partstat,
            op.ics_file,
            op.if_match,
            op.reply_op_id,
            op.state.as_str(),
            op.attempts,
            op.next_at,
            op.error,
            op.server_won.then_some(SERVER_WON),
            op.conflict_ics_file,
            op.created_at,
            op.seq,
            op.attendee,
            op.identity_id,
            op.href,
            op.dest_href,
            op.reply_cancelled,
            op.prior_partstat,
        ],
    )?;
    Ok(true)
}

fn save_state(conn: &Connection, op: &CalOp) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE cal_op SET state = ?2, attempts = ?3, next_at = ?4, error = ?5, \
         conflict_kind = ?6, conflict_ics_file = ?7, ics_file = ?8, if_match = ?9, href = ?10, \
         reply_op_id = ?11, calendar_href = ?12, prior_partstat = ?13 WHERE op_id = ?1",
        params![
            op.op_id,
            op.state.as_str(),
            op.attempts,
            op.next_at,
            op.error,
            op.server_won.then_some(SERVER_WON),
            op.conflict_ics_file,
            op.ics_file,
            op.if_match,
            op.href,
            op.reply_op_id,
            op.calendar_href,
            op.prior_partstat,
        ],
    )?;
    Ok(())
}

fn reply_state(conn: &Connection, op: &CalOp) -> rusqlite::Result<Option<CalReplyStateDto>> {
    let Some(reply) = &op.reply_op_id else {
        return Ok(None);
    };
    if op.reply_cancelled {
        return Ok(Some(CalReplyStateDto::Cancelled));
    }
    let state: Option<String> = conn
        .query_row("SELECT state FROM outbox WHERE op_id = ?1", [reply], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(Some(match state.as_deref() {
        Some("queued") => CalReplyStateDto::Queued,
        Some("sending") => CalReplyStateDto::Sending,
        Some("failed") => CalReplyStateDto::Failed,
        _ => CalReplyStateDto::Sent,
    }))
}

fn dto(op: &CalOp, reply: Option<CalReplyStateDto>) -> OpDto {
    OpDto {
        op_id: op.op_id.clone(),
        seq: op.seq,
        account_id: op.account.0,
        kind: op.kind.dto(),
        status: op.state.into(),
        error: op.error.clone(),
        attempts: op.attempts,
        thread_ids: Vec::new(),
        label: None,
        mailbox_id: None,
        created_at: op.created_at,
        domain: OpDomainDto::Cal,
        calendar: Some(CalOpInfoDto {
            target_account_id: op.target.0,
            calendar_href: op.calendar_href.clone(),
            uid: op.uid.clone(),
            conflict: op.server_won.then_some(CalConflictDto::ServerWon),
            reply_op_id: op.reply_op_id.clone(),
            reply,
        }),
    }
}

pub(crate) fn dtos(conn: &Connection, ops: &[CalOp]) -> rusqlite::Result<Vec<OpDto>> {
    ops.iter()
        .map(|op| Ok(dto(op, reply_state(conn, op)?)))
        .collect()
}

/// Publishes the op's state, or its removal, as a `cal_op` change.
pub(crate) async fn emit(stores: &Stores, op_id: &str) {
    let id = op_id.to_owned();
    let read = stores
        .config
        .read(move |c| {
            let op = load_one(c, &id)?;
            Ok(match op {
                Some(op) => Some(dtos(c, std::slice::from_ref(&op))?.remove(0)),
                None => None,
            })
        })
        .await;
    let change = match read {
        Ok(Some(dto)) => NewChange {
            entity: Entity::CalOp,
            entity_id: dto.seq,
            op: ChangeOp::Upsert,
            payload: Some(
                serde_json::to_value(&dto)
                    .unwrap_or(Value::Null)
                    .to_string(),
            ),
        },
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(op = op_id, error = %e, "cannot read a calendar op");
            return;
        }
    };
    publish(stores, change).await;
}

/// Publishes the removal of a discarded op.
pub(crate) async fn emit_removed(stores: &Stores, op_id: &str, seq: i64) {
    publish(
        stores,
        NewChange {
            entity: Entity::CalOp,
            entity_id: seq,
            op: ChangeOp::Delete,
            payload: Some(serde_json::json!({ "opId": op_id }).to_string()),
        },
    )
    .await;
}

async fn publish(stores: &Stores, change: NewChange) {
    let epoch = stores.cache.epoch();
    let head = stores
        .cache
        .write(move |tx| {
            changes::append(tx, &change)?;
            changes::head(tx, epoch)
        })
        .await;
    match head {
        Ok(head) => stores.feed.publish(head),
        Err(e) => tracing::warn!(error = %e, "cannot publish a calendar op change"),
    }
}

pub(crate) fn ics_dir(stores: &Stores) -> PathBuf {
    stores.outbox.join(ICS_DIR)
}

fn read_ics(dir: &Path, name: Option<&str>) -> Result<String, String> {
    let name = name.ok_or("the op has no calendar object")?;
    std::fs::read_to_string(dir.join(name)).map_err(|e| format!("calendar object {name}: {e}"))
}

pub(crate) fn remove_files(dir: &Path, op: &CalOp) {
    for name in [&op.ics_file, &op.conflict_ics_file].into_iter().flatten() {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// The partstat an RSVP answers with.
pub(crate) fn partstat(text: &str) -> ParticipationStatus {
    match text {
        "ACCEPTED" => ParticipationStatus::Accepted,
        "DECLINED" => ParticipationStatus::Declined,
        "TENTATIVE" => ParticipationStatus::Tentative,
        "NEEDS-ACTION" => ParticipationStatus::NeedsAction,
        other => ParticipationStatus::Other(other.to_owned()),
    }
}

fn subject_prefix(partstat: &ParticipationStatus) -> &'static str {
    match partstat {
        ParticipationStatus::Accepted => "Accepted",
        ParticipationStatus::Declined => "Declined",
        ParticipationStatus::Tentative => "Tentative",
        _ => "Reply",
    }
}

/// Writes the iTIP REPLY of RSVP `op` to the organizer of `invite`, for
/// [`queue_reply`] under `<opId>#reply`.
pub(crate) async fn write_reply(
    stores: &Stores,
    op: &CalOp,
    invite: &Calendar,
) -> tagmail_api_core::compose::ComposeResult<GeneratedMessage> {
    let invalid = |message: String| tagmail_api_core::compose::ComposeError::Invalid {
        field: "messageId",
        message,
    };
    let event = invite
        .events
        .iter()
        .filter(|e| e.uid == op.uid)
        .min_by_key(|e| e.recurrence_id.is_some())
        .ok_or_else(|| invalid("the invitation has no such event".to_owned()))?;
    let organizer = event
        .organizer
        .as_ref()
        .filter(|o| o.address_is_email)
        .ok_or_else(|| invalid("the invitation has no organizer to reply to".to_owned()))?;
    let me = op.attendee.as_deref().unwrap_or_default();
    let answer = partstat(op.partstat.as_deref().unwrap_or_default());
    let ics = tagmail_calendar::reply(
        invite,
        &op.uid,
        event.recurrence_id.as_ref(),
        me,
        &answer,
        OffsetDateTime::now_utc(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let to = vec![tagmail_api_core::dto::AddressDto {
        name: organizer.name.clone(),
        email: organizer.address.clone(),
    }];
    let summary = event.summary.clone().unwrap_or_default();
    let subject = format!("{}: {summary}", subject_prefix(&answer));
    let method = tagmail_calendar::Method::Reply;
    let build = |from: &tagmail_api_core::dto::AddressDto, msgid: &str, date: i64| {
        crate::outgoing::itip_mime(from, &to, &subject, msgid, date, &ics, &method)
    };
    compose::write_generated(
        stores,
        format!("{}{REPLY_SUFFIX}", op.op_id),
        op.account.0,
        op.identity_id,
        &to,
        subject.clone(),
        &build,
    )
    .await
}

/// Queues `reply` of `op` in `tx` and links it to the op.
pub(crate) fn queue_reply(
    tx: &rusqlite::Transaction<'_>,
    outbox: &Path,
    op: &CalOp,
    reply: &GeneratedMessage,
) -> tagmail_api_core::compose::ComposeResult<()> {
    if queue_generated(tx, outbox, reply)?.is_none() {
        reply.discard();
    }
    tx.execute(
        "UPDATE cal_op SET reply_op_id = ?2 WHERE op_id = ?1",
        params![op.op_id, reply.op_id],
    )
    .map_err(|e| {
        tagmail_api_core::compose::ComposeError::Internal(Box::new(tagmail_store::Error::from(e)))
    })?;
    Ok(())
}

/// Whether the master of `uid` in `invite` is newer than the one in the
/// stored object `stored`, so an answer must not write the older version.
fn newer_than(invite: &Calendar, uid: &str, stored: &str) -> bool {
    let master = |events: &[tagmail_calendar::Event]| {
        events
            .iter()
            .find(|e| e.uid == uid && e.recurrence_id.is_none())
            .map(|e| itip_auth::IncomingVersion {
                sequence: e.sequence,
                dtstamp: e.dtstamp.map(OffsetDateTime::unix_timestamp),
            })
    };
    let Some(incoming) = master(&invite.events) else {
        return false;
    };
    let Some(held) = tagmail_calendar::parse(stored.as_bytes())
        .ok()
        .and_then(|c| master(&c.events))
    else {
        return false;
    };
    let held = itip_auth::StoredVersion {
        sequence: held.sequence,
        dtstamp: held.dtstamp,
        cancelled: false,
        partstat: None,
        organizer: None,
    };
    !itip_auth::already_applied(false, incoming, &held)
}

/// The server copy after a write, as [`Exec::refresh`] found it.
#[derive(Debug)]
enum Refreshed {
    Found(Resource),
    Gone,
    /// It could not be read; nothing is known about it.
    Unknown,
}

/// How one run of an op ended.
#[derive(Debug)]
enum Outcome {
    Done,
    Retry(String),
    Failed(String),
    /// The server copy changed under a full edit; it has `etag`.
    ServerWon(Option<String>),
}

fn retryable(e: &DavError) -> bool {
    match e {
        DavError::Network(_) => true,
        DavError::Status { status, .. } => *status >= SERVER_ERROR || *status == TOO_MANY_REQUESTS,
        _ => false,
    }
}

fn failure(e: DavError) -> Outcome {
    if retryable(&e) {
        Outcome::Retry(e.to_string())
    } else {
        Outcome::Failed(e.to_string())
    }
}

/// The target account's calendars, the op's own first.
fn calendar_urls(home: &Url, rows: &[calendar::CalendarRow], first: Option<&str>) -> Vec<Url> {
    let mut rows: Vec<&calendar::CalendarRow> = rows.iter().collect();
    rows.sort_by_key(|r| Some(r.href.as_str()) != first);
    rows.iter()
        .filter_map(|r| home.join(&r.href).ok())
        .collect()
}

/// A new resource for `uid` in `calendar`.
fn new_resource(calendar: &Url, uid: &str) -> Url {
    let mut url = calendar.clone();
    if let Ok(mut segments) = url.path_segments_mut() {
        segments
            .pop_if_empty()
            .push(&format!("{uid}.{ICS_EXTENSION}"));
    }
    url
}

fn delivered(status: &str) -> bool {
    status
        .split(',')
        .any(|s| DELIVERED_CLASSES.iter().any(|c| s.trim().starts_with(c)))
}

/// One op's run against the target account's server.
pub(crate) struct Exec<'a> {
    pub stores: &'a Stores,
    pub dav: &'a dyn CalDav,
    pub target: &'a CalAccount,
    pub home: Url,
    pub calendars: Vec<calendar::CalendarRow>,
    /// The outbox gained a REPLY.
    pub replied: &'a mut bool,
}

impl Exec<'_> {
    fn dir(&self) -> PathBuf {
        ics_dir(self.stores)
    }

    fn urls(&self, first: Option<&str>) -> Vec<Url> {
        calendar_urls(&self.home, &self.calendars, first)
    }

    fn calendar_url(&self, href: Option<&str>) -> Option<Url> {
        let href = href?;
        self.home.join(href).ok()
    }

    fn server_mode(&self) -> bool {
        self.target.config.scheduling() == Some(calendar::Scheduling::Server)
    }

    /// Caches the resource at `url` as the server has it now; a 404 drops
    /// it from the cache.
    async fn refresh(&self, url: &Url) -> Refreshed {
        let fetched = self.dav.get(url).await;
        let key = href(url);
        let calendar = self
            .calendars
            .iter()
            .filter(|c| key.starts_with(&c.href))
            .max_by_key(|c| c.href.len())
            .map(|c| c.id);
        let resource = match fetched {
            Ok(r) => Some(r),
            Err(DavError::NotFound { .. }) => None,
            Err(e) => {
                tracing::debug!(href = %url, error = %e, "cannot refetch a written event");
                return Refreshed::Unknown;
            }
        };
        if let Some(calendar_id) = calendar {
            let me = self.target.addresses.clone();
            let window = Window::around(OffsetDateTime::now_utc().date());
            let zone = super::sync::owner_zone(self.stores).await;
            let parsed = resource
                .as_ref()
                .and_then(|r| parse(&key, r.etag.clone(), &r.data, &me, &window, &zone));
            let account = self.target.id;
            let gone = key.clone();
            let epoch = self.stores.cache.epoch();
            let written = self
                .stores
                .cache
                .write(move |tx| {
                    let live = calendar::calendars(tx, Some(account))?
                        .iter()
                        .any(|c| c.id == calendar_id);
                    if live {
                        match &parsed {
                            Some(p) => {
                                let id = calendar::put_event(
                                    tx,
                                    calendar_id,
                                    &p.event,
                                    &p.ics,
                                    &p.instances,
                                )?;
                                tagmail_store::alarms::replace_alarms(tx, id, &p.alarms)?;
                            }
                            None => {
                                calendar::delete_events(tx, calendar_id, &[gone])?;
                            }
                        }
                    }
                    changes::head(tx, epoch)
                })
                .await;
            match written {
                Ok(head) => self.stores.feed.publish(head),
                Err(e) => tracing::warn!(error = %e, "cannot cache a written event"),
            }
        }
        match resource {
            Some(r) => Refreshed::Found(r),
            None => Refreshed::Gone,
        }
    }

    async fn queue_reply(&mut self, op: &mut CalOp, invite: &Calendar) -> Result<(), String> {
        if op.reply_op_id.is_some() || op.reply_cancelled {
            return Ok(());
        }
        let reply = write_reply(self.stores, op, invite)
            .await
            .map_err(|e| e.to_string())?;
        let outbox = self.stores.outbox.clone();
        let row = op.clone();
        let queued = reply.clone();
        let written = compose::write(self.stores, move |tx| {
            queue_reply(tx, &outbox, &row, &queued)
        })
        .await;
        if let Err(e) = written {
            reply.discard();
            return Err(e.to_string());
        }
        op.reply_op_id = Some(reply.op_id);
        *self.replied = true;
        Ok(())
    }

    async fn run(&mut self, op: &mut CalOp) -> Outcome {
        match op.kind {
            CalOpKind::Rsvp => self.rsvp(op).await,
            CalOpKind::Import => self.import(op).await,
            CalOpKind::Apply => self.apply(op).await,
            CalOpKind::Put => self.put(op).await,
            CalOpKind::Delete => self.delete(op).await,
            CalOpKind::Move => self.move_op(op).await,
        }
    }

    /// Plan §3.3: my PARTSTAT on the stored copy (or the invitation's
    /// object when the server has none), rebased over 412s. A UID the
    /// server does not have, or a server-mode PUT the server did not
    /// deliver, gets the outbox REPLY.
    async fn rsvp(&mut self, op: &mut CalOp) -> Outcome {
        let invite_text = match read_ics(&self.dir(), op.ics_file.as_deref()) {
            Ok(text) => text,
            Err(e) => return Outcome::Failed(e),
        };
        let invite = match tagmail_calendar::parse(invite_text.as_bytes()) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(format!("the invitation cannot be read: {e}")),
        };
        let me = op.attendee.clone().unwrap_or_default();
        let answer = partstat(op.partstat.as_deref().unwrap_or_default());
        let declined = answer == ParticipationStatus::Declined;
        let Some(calendar) = self.calendar_url(op.calendar_href.as_deref()) else {
            return Outcome::Failed("no calendar to save the answer in".to_owned());
        };
        let calendars = self.urls(op.calendar_href.as_deref());
        let earlier = op.prior_partstat.clone();
        let mut uid_retried = false;
        for _ in 0..MAX_REBASES {
            let found = match self.dav.find_by_uid(&calendars, &op.uid).await {
                Ok(found) => found,
                Err(e) => return failure(e),
            };
            let server_handles = found.is_some() && self.server_mode();
            let prior = |ics: &str| {
                tagmail_calendar::parse(ics.as_bytes())
                    .ok()
                    .and_then(|c| c.summary(std::slice::from_ref(&me)))
                    .and_then(|s| s.my_partstat)
            };
            let (url, condition, base) = match found {
                Some(m) => {
                    op.prior_partstat = earlier.clone().or_else(|| prior(&m.resource.data));
                    let condition = m
                        .resource
                        .etag
                        .clone()
                        .map_or(PutCondition::Overwrite, PutCondition::IfMatch);
                    let base = if newer_than(&invite, &op.uid, &m.resource.data) {
                        match tagmail_calendar::apply_update(
                            m.resource.data.as_bytes(),
                            invite_text.as_bytes(),
                            &me,
                        ) {
                            Ok(updated) => updated,
                            Err(e) => return Outcome::Failed(e.to_string()),
                        }
                    } else {
                        m.resource.data
                    };
                    (m.resource.href, condition, base)
                }
                None => {
                    let object = tagmail_calendar::for_import(invite_text.as_bytes())
                        .ok()
                        .and_then(|objects| objects.into_iter().find(|(uid, _)| *uid == op.uid));
                    let Some((_, object)) = object else {
                        return Outcome::Failed("the invitation has no such event".to_owned());
                    };
                    op.prior_partstat = earlier.clone().or_else(|| prior(&object));
                    (
                        new_resource(&calendar, &op.uid),
                        PutCondition::Create,
                        object,
                    )
                }
            };
            if !server_handles && let Err(e) = self.queue_reply(op, &invite).await {
                return Outcome::Failed(e);
            }
            let body = match tagmail_calendar::set_my_partstat(
                base.as_bytes(),
                &me,
                &answer,
                !server_handles || op.reply_cancelled,
            ) {
                Ok(body) => body,
                Err(e) => return Outcome::Failed(e.to_string()),
            };
            match self.dav.put(&url, &body, condition).await {
                Ok(_) => {
                    op.href = Some(href(&url));
                    let stored = self.refresh(&url).await;
                    if server_handles {
                        let organizer = invite
                            .events
                            .iter()
                            .find(|e| e.uid == op.uid)
                            .and_then(|e| e.organizer.as_ref())
                            .map(|o| o.address.clone())
                            .unwrap_or_default();
                        let status = match &stored {
                            Refreshed::Found(r) => {
                                tagmail_calendar::schedule_status(r.data.as_bytes(), &organizer)
                                    .ok()
                                    .flatten()
                            }
                            Refreshed::Gone | Refreshed::Unknown => None,
                        };
                        let handled = status.as_deref().is_some_and(delivered);
                        let removed = matches!(stored, Refreshed::Gone) && declined;
                        if !handled
                            && !removed
                            && let Err(e) = self.queue_reply(op, &invite).await
                        {
                            return Outcome::Failed(e);
                        }
                    }
                    return Outcome::Done;
                }
                Err(DavError::PreconditionFailed) => {}
                Err(DavError::UidConflict { .. }) if !uid_retried => uid_retried = true,
                Err(DavError::NotFound { .. }) if declined => return Outcome::Done,
                Err(e) => return failure(e),
            }
        }
        Outcome::Failed("the event kept changing on the server; try again".to_owned())
    }

    /// Puts every event of the calendar part, updating a UID the server
    /// already has in place.
    async fn import(&mut self, op: &mut CalOp) -> Outcome {
        let text = match read_ics(&self.dir(), op.ics_file.as_deref()) {
            Ok(text) => text,
            Err(e) => return Outcome::Failed(e),
        };
        let objects = match tagmail_calendar::for_import(text.as_bytes()) {
            Ok(objects) => objects,
            Err(e) => return Outcome::Failed(format!("the calendar cannot be read: {e}")),
        };
        let Some(calendar) = self.calendar_url(op.calendar_href.as_deref()) else {
            return Outcome::Failed("no calendar to import into".to_owned());
        };
        let calendars = self.urls(op.calendar_href.as_deref());
        for (uid, object) in objects {
            let mut uid_retried = false;
            let mut written = false;
            for _ in 0..MAX_REBASES {
                let found = match self.dav.find_by_uid(&calendars, &uid).await {
                    Ok(found) => found,
                    Err(e) => return failure(e),
                };
                let (url, condition) = match found {
                    Some(m) => (
                        m.resource.href,
                        m.resource
                            .etag
                            .map_or(PutCondition::Overwrite, PutCondition::IfMatch),
                    ),
                    None => (new_resource(&calendar, &uid), PutCondition::Create),
                };
                match self.dav.put(&url, &object, condition).await {
                    Ok(_) => {
                        self.refresh(&url).await;
                        written = true;
                        break;
                    }
                    Err(DavError::PreconditionFailed) => {}
                    Err(DavError::UidConflict { .. }) if !uid_retried => uid_retried = true,
                    Err(e) => return failure(e),
                }
            }
            if !written {
                return Outcome::Failed(format!("{uid} kept changing on the server"));
            }
        }
        Outcome::Done
    }

    /// An update or CANCEL onto the stored copy, which keeps my PARTSTAT;
    /// nothing is written when the server copy already has it (§3.3).
    async fn apply(&mut self, op: &mut CalOp) -> Outcome {
        let text = match read_ics(&self.dir(), op.ics_file.as_deref()) {
            Ok(text) => text,
            Err(e) => return Outcome::Failed(e),
        };
        let incoming = match tagmail_calendar::parse(text.as_bytes()) {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(format!("the invitation cannot be read: {e}")),
        };
        let cancel = incoming.method == Some(tagmail_calendar::Method::Cancel);
        let of_uid: Vec<_> = incoming.events.iter().filter(|e| e.uid == op.uid).collect();
        let Some(newest) = of_uid
            .iter()
            .max_by_key(|e| (e.recurrence_id.is_none(), e.sequence, e.dtstamp))
        else {
            return Outcome::Failed("the invitation has no such event".to_owned());
        };
        let version = itip_auth::IncomingVersion {
            sequence: newest.sequence,
            dtstamp: newest.dtstamp.map(OffsetDateTime::unix_timestamp),
        };
        let instance = of_uid
            .iter()
            .all(|e| e.recurrence_id.is_some())
            .then(|| newest.recurrence_id.as_ref().map(itip_auth::recurrence_key))
            .flatten();
        let me = op.attendee.clone().unwrap_or_default();
        let calendars = self.urls(op.calendar_href.as_deref());
        for _ in 0..MAX_REBASES {
            let found = match self.dav.find_by_uid(&calendars, &op.uid).await {
                Ok(found) => found,
                Err(e) => return failure(e),
            };
            let Some(found) = found else {
                return Outcome::Failed("the event is not in the calendar".to_owned());
            };
            let stored = found.resource;
            if let Ok(parsed) = tagmail_calendar::parse(stored.data.as_bytes()) {
                let held = match &instance {
                    Some(rid) => parsed.events.iter().find(|e| {
                        e.recurrence_id
                            .as_ref()
                            .is_some_and(|r| itip_auth::recurrence_key(r) == *rid)
                    }),
                    None => parsed
                        .events
                        .iter()
                        .find(|e| e.recurrence_id.is_none())
                        .or_else(|| parsed.events.first()),
                };
                let held = held.map(|m| itip_auth::StoredVersion {
                    sequence: m.sequence,
                    dtstamp: m.dtstamp.map(OffsetDateTime::unix_timestamp),
                    cancelled: m.status == Some(tagmail_calendar::EventStatus::Cancelled),
                    partstat: None,
                    organizer: None,
                });
                if held.is_some_and(|held| itip_auth::already_applied(cancel, version, &held)) {
                    return Outcome::Done;
                }
            }
            let body = if cancel {
                tagmail_calendar::apply_cancel(stored.data.as_bytes(), text.as_bytes())
            } else {
                tagmail_calendar::apply_update(stored.data.as_bytes(), text.as_bytes(), &me)
            };
            let body = match body {
                Ok(body) => body,
                Err(e) => return Outcome::Failed(e.to_string()),
            };
            let condition = stored
                .etag
                .clone()
                .map_or(PutCondition::Overwrite, PutCondition::IfMatch);
            match self.dav.put(&stored.href, &body, condition).await {
                Ok(_) => {
                    op.href = Some(href(&stored.href));
                    self.refresh(&stored.href).await;
                    return Outcome::Done;
                }
                Err(DavError::PreconditionFailed) => {}
                Err(DavError::NotFound { .. }) if cancel => return Outcome::Done,
                Err(e) => return failure(e),
            }
        }
        Outcome::Failed("the event kept changing on the server; try again".to_owned())
    }

    fn resource_url(&self, op: &CalOp) -> Option<Url> {
        match &op.href {
            Some(href) => self.home.join(href).ok(),
            None => self
                .calendar_url(op.calendar_href.as_deref())
                .map(|c| new_resource(&c, &op.uid)),
        }
    }

    async fn server_won(&self, url: &Url) -> Outcome {
        let etag = match self.refresh(url).await {
            Refreshed::Found(r) => r.etag,
            Refreshed::Gone | Refreshed::Unknown => None,
        };
        Outcome::ServerWon(etag)
    }

    async fn put(&mut self, op: &mut CalOp) -> Outcome {
        let body = match read_ics(&self.dir(), op.ics_file.as_deref()) {
            Ok(body) => body,
            Err(e) => return Outcome::Failed(e),
        };
        let Some(url) = self.resource_url(op) else {
            return Outcome::Failed("no calendar to write to".to_owned());
        };
        let condition = match (&op.if_match, &op.href) {
            (Some(etag), _) => PutCondition::IfMatch(etag.clone()),
            (None, Some(_)) => PutCondition::Overwrite,
            (None, None) => PutCondition::Create,
        };
        match self.dav.put(&url, &body, condition).await {
            Ok(_) => {
                op.href = Some(href(&url));
                self.refresh(&url).await;
                Outcome::Done
            }
            Err(DavError::PreconditionFailed) => self.server_won(&url).await,
            Err(e) => failure(e),
        }
    }

    async fn delete(&mut self, op: &mut CalOp) -> Outcome {
        let Some(url) = op.href.as_deref().and_then(|h| self.home.join(h).ok()) else {
            return Outcome::Failed("no event to delete".to_owned());
        };
        match self.dav.delete(&url, op.if_match.as_deref()).await {
            Ok(()) | Err(DavError::NotFound { .. }) => {
                self.refresh(&url).await;
                Outcome::Done
            }
            Err(DavError::PreconditionFailed) => self.server_won(&url).await,
            Err(e) => failure(e),
        }
    }

    /// dav's MOVE already falls back to PUT + DELETE. A run after a failed
    /// one first checks the destination, so a copy left by a DELETE that
    /// failed is finished rather than moved again.
    async fn move_op(&mut self, op: &mut CalOp) -> Outcome {
        let (Some(from), Some(dest)) = (
            op.href.as_deref().and_then(|h| self.home.join(h).ok()),
            self.calendar_url(op.dest_href.as_deref()),
        ) else {
            return Outcome::Failed("no event or destination to move".to_owned());
        };
        let name = from
            .path_segments()
            .and_then(|mut s| s.next_back().map(str::to_owned))
            .unwrap_or_else(|| format!("{}.{ICS_EXTENSION}", op.uid));
        let mut to = dest.clone();
        if let Ok(mut segments) = to.path_segments_mut() {
            segments.pop_if_empty().push(&name);
        }
        if op.attempts > 0 && self.dav.get(&to).await.is_ok() {
            return match self.dav.delete(&from, None).await {
                Ok(()) | Err(DavError::NotFound { .. }) => {
                    self.refresh(&from).await;
                    self.refresh(&to).await;
                    op.href = Some(href(&to));
                    Outcome::Done
                }
                Err(e) => failure(e),
            };
        }
        match self
            .dav
            .move_resource(&from, &to, op.if_match.as_deref())
            .await
        {
            Ok(()) => {
                self.refresh(&from).await;
                self.refresh(&to).await;
                op.href = Some(href(&to));
                op.calendar_href = op.dest_href.clone();
                Outcome::Done
            }
            Err(DavError::PreconditionFailed) => self.server_won(&from).await,
            Err(e) => failure(e),
        }
    }
}

/// Records `outcome` on `op`.
fn settle(dir: &Path, op: &mut CalOp, outcome: Outcome) {
    match outcome {
        Outcome::Done => {
            op.state = OpState::Done;
            op.error = None;
            op.next_at = None;
            remove_files(dir, op);
            op.ics_file = None;
            op.conflict_ics_file = None;
        }
        Outcome::Retry(error) => {
            op.attempts += 1;
            if op.attempts >= MAX_ATTEMPTS {
                op.state = OpState::Failed;
                op.next_at = None;
            } else {
                op.state = OpState::Pending;
                let wait = i64::try_from(backoff(op.attempts).as_secs()).unwrap_or(i64::MAX);
                op.next_at = Some(now_secs().saturating_add(wait));
            }
            op.error = Some(error);
        }
        Outcome::Failed(error) => {
            op.attempts += 1;
            op.state = OpState::Failed;
            op.next_at = None;
            op.error = Some(error);
        }
        Outcome::ServerWon(etag) => {
            op.attempts += 1;
            op.state = OpState::Failed;
            op.next_at = None;
            op.error =
                Some("the event changed on the server; re-apply or discard this edit".to_owned());
            op.server_won = true;
            if let Some(name) = op.ics_file.take() {
                let conflict = format!("{}.{CONFLICT_EXTENSION}", op.op_id);
                match std::fs::rename(dir.join(&name), dir.join(&conflict)) {
                    Ok(()) => op.conflict_ics_file = Some(conflict),
                    Err(e) => {
                        tracing::warn!(op = %op.op_id, error = %e, "cannot keep a rejected edit");
                        op.ics_file = Some(name);
                    }
                }
            }
            op.if_match = etag;
        }
    }
}

/// Runs due ops of `account`'s calendars on `dav`; returns whether a
/// REPLY was queued.
pub(crate) async fn run_account(
    stores: &Stores,
    dav: &dyn CalDav,
    target: &CalAccount,
    ops: Vec<CalOp>,
) -> bool {
    let mut replied = false;
    let Some(home) = target
        .config
        .home_url
        .as_deref()
        .and_then(|h| Url::parse(h).ok())
    else {
        defer(stores, ops, "the calendar home is not known").await;
        return false;
    };
    let id = target.id;
    let calendars = match stores
        .cache
        .read(move |c| calendar::calendars(c, Some(id)))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            defer(stores, ops, &e.to_string()).await;
            return false;
        }
    };
    let dir = ics_dir(stores);
    for mut op in ops {
        let claimed = {
            let op_id = op.op_id.clone();
            stores
                .config
                .write(move |tx| {
                    Ok(tx.execute(
                        "UPDATE cal_op SET state = 'running' WHERE op_id = ?1 AND state = 'pending'",
                        [op_id],
                    )? == 1)
                })
                .await
        };
        if !matches!(claimed, Ok(true)) {
            continue;
        }
        let mut exec = Exec {
            stores,
            dav,
            target,
            home: home.clone(),
            calendars: calendars.clone(),
            replied: &mut replied,
        };
        let outcome = exec.run(&mut op).await;
        tracing::info!(op = %op.op_id, kind = op.kind.as_str(), outcome = ?outcome, "calendar op ran");
        settle(&dir, &mut op, outcome);
        let row = op.clone();
        if let Err(e) = stores
            .config
            .write(move |tx| Ok(save_state(tx, &row)?))
            .await
        {
            tracing::error!(op = %op.op_id, error = %e, "cannot record a calendar op");
        }
        emit(stores, &op.op_id).await;
    }
    replied
}

/// Puts off `ops`, which could not run for `error`, as after a transient
/// failure.
pub(crate) async fn defer(stores: &Stores, ops: Vec<CalOp>, error: &str) {
    tracing::warn!(error, ops = ops.len(), "calendar ops wait");
    let dir = ics_dir(stores);
    for mut op in ops {
        settle(&dir, &mut op, Outcome::Retry(error.to_owned()));
        let row = op.clone();
        if let Err(e) = stores
            .config
            .write(move |tx| Ok(save_state(tx, &row)?))
            .await
        {
            tracing::error!(op = %op.op_id, error = %e, "cannot record a calendar op");
        }
        emit(stores, &op.op_id).await;
    }
}

/// Pending ops due now, by target account.
pub(crate) async fn due(stores: &Stores) -> tagmail_store::Result<Vec<CalOp>> {
    let now = now_secs();
    stores
        .config
        .read(move |c| {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM cal_op \
                 WHERE state = 'pending' AND coalesce(next_at, 0) <= ?1 ORDER BY op_seq LIMIT ?2"
            ))?;
            Ok(stmt
                .query_map(params![now, BATCH as i64], row)?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
}

/// When the next pending op is due, Unix seconds.
pub(crate) async fn next_due(stores: &Stores) -> Option<i64> {
    stores
        .config
        .read(|c| {
            Ok(c.query_row(
                "SELECT min(coalesce(next_at, 0)) FROM cal_op WHERE state = 'pending'",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )?)
        })
        .await
        .ok()
        .flatten()
}

/// Startup: interrupted ops run again, done ones past the `op_seen`
/// retention are dropped.
pub(crate) async fn recover(stores: &Stores) {
    let cutoff = now_secs() - OP_SEEN_SECS;
    let cleaned = stores
        .config
        .write(move |tx| {
            tx.execute(
                "UPDATE cal_op SET state = 'pending' WHERE state = 'running'",
                [],
            )?;
            let done = load(tx, "state = 'done' AND ics_file IS NOT NULL", [])?;
            tx.execute("UPDATE cal_op SET ics_file = NULL WHERE state = 'done'", [])?;
            tx.execute(
                "DELETE FROM cal_op WHERE state = 'done' AND created_at < ?1",
                [cutoff],
            )?;
            Ok(done)
        })
        .await;
    match cleaned {
        Ok(done) => {
            let dir = ics_dir(stores);
            for op in &done {
                remove_files(&dir, op);
            }
        }
        Err(e) => tracing::error!(error = %e, "cannot recover calendar ops"),
    }
}

/// The ops endpoints' calendar half.
impl<F: crate::supervisor::ConnectorFactory> crate::supervisor::Engine<F> {
    /// The calendar op `op_id`; `None` when there is none.
    pub(crate) async fn cal_op_dto(&self, op_id: String) -> WriteResult<Option<OpDto>> {
        self.inner
            .stores
            .config
            .read(move |c| {
                Ok(match load_one(c, &op_id)? {
                    Some(op) => Some(dtos(c, std::slice::from_ref(&op))?.remove(0)),
                    None => None,
                })
            })
            .await
            .map_err(WriteError::internal)
    }

    pub(crate) async fn cal_ops(&self, status: Option<OpStatusDto>) -> WriteResult<Vec<OpDto>> {
        let filter = match status {
            Some(OpStatusDto::Failed) => "state = 'failed'",
            Some(OpStatusDto::Pending) => "state IN ('pending', 'running')",
            _ => "state != 'done'",
        };
        self.inner
            .stores
            .config
            .read(move |c| Ok(dtos(c, &load(c, filter, [])?)?))
            .await
            .map_err(WriteError::internal)
    }

    /// Puts a failed calendar op back in the queue; a `server_won` edit is
    /// re-applied over the server copy it lost to. `None` when no calendar
    /// op has this id.
    pub(crate) async fn retry_cal_op(&self, op_id: String) -> WriteResult<Option<OpDto>> {
        let stores = &self.inner.stores;
        let dir = ics_dir(stores);
        let id = op_id.clone();
        let retried = stores
            .config
            .write(move |tx| {
                let Some(mut op) = load_one(tx, &id)? else {
                    return Ok(None);
                };
                if op.state != OpState::Failed {
                    return Ok(Some(false));
                }
                if op.server_won {
                    op.server_won = false;
                    if let Some(conflict) = op.conflict_ics_file.take() {
                        let name = format!("{}.{ICS_EXTENSION}", op.op_id);
                        std::fs::rename(dir.join(&conflict), dir.join(&name))?;
                        op.ics_file = Some(name);
                    }
                }
                op.state = OpState::Pending;
                op.attempts = 0;
                op.next_at = None;
                op.error = None;
                save_state(tx, &op)?;
                Ok(Some(true))
            })
            .await
            .map_err(WriteError::internal)?;
        match retried {
            None => return Ok(None),
            Some(false) => {
                return Err(WriteError::Conflict(
                    "only failed ops can be retried".to_owned(),
                ));
            }
            Some(true) => {}
        }
        emit(stores, &op_id).await;
        self.wake_calendar_ops();
        self.cal_op_dto(op_id).await
    }

    /// Drops a pending or failed calendar op; its id stays used. `None`
    /// when no calendar op has this id.
    pub(crate) async fn discard_cal_op(&self, op_id: String) -> WriteResult<Option<()>> {
        let stores = &self.inner.stores;
        let id = op_id.clone();
        let removed = stores
            .config
            .write(move |tx| {
                let Some(op) = load_one(tx, &id)? else {
                    return Ok(None);
                };
                let deleted = tx.execute(
                    "DELETE FROM cal_op WHERE op_id = ?1 AND state != 'running'",
                    [&id],
                )?;
                Ok(Some((deleted > 0).then_some(op)))
            })
            .await
            .map_err(WriteError::internal)?;
        match removed {
            None => Ok(None),
            Some(None) => Err(WriteError::Conflict(
                "the op is being applied right now".to_owned(),
            )),
            Some(Some(op)) => {
                remove_files(&ics_dir(stores), &op);
                emit_removed(stores, &op.op_id, op.seq).await;
                Ok(Some(()))
            }
        }
    }

    /// The REPLY `<parent>#reply` was cancelled in the outbox (plan
    /// §3.6): a still pending RSVP is discarded; one that already wrote
    /// keeps its PARTSTAT and reports the reply as cancelled.
    pub(crate) async fn calendar_reply_cancelled(&self, parent: String) {
        let stores = &self.inner.stores;
        let id = parent.clone();
        let changed = stores
            .config
            .write(move |tx| {
                let Some(op) = load_one(tx, &id)? else {
                    return Ok(None);
                };
                if op.state == OpState::Pending {
                    tx.execute("DELETE FROM cal_op WHERE op_id = ?1", [&id])?;
                    return Ok(Some((op, true)));
                }
                tx.execute(
                    "UPDATE cal_op SET reply_cancelled = 1 WHERE op_id = ?1",
                    [&id],
                )?;
                Ok(Some((op, false)))
            })
            .await;
        match changed {
            Ok(Some((op, true))) => {
                remove_files(&ics_dir(stores), &op);
                emit_removed(stores, &op.op_id, op.seq).await;
            }
            Ok(Some((_, false))) => emit(stores, &parent).await,
            Ok(None) => {}
            Err(e) => tracing::error!(op = %parent, error = %e, "cannot settle a cancelled reply"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_schedule_status_classes() {
        assert!(delivered("1.2"));
        assert!(delivered("2.0"));
        assert!(delivered("3.7,1.1"));
        assert!(!delivered("3.7"));
        assert!(!delivered("5.1"));
    }

    #[test]
    fn new_resources_are_named_after_the_uid() {
        let calendar = Url::parse("https://dav.example.com/home/work/").unwrap();
        assert_eq!(
            new_resource(&calendar, "a b@x").as_str(),
            "https://dav.example.com/home/work/a%20b@x.ics"
        );
    }
}
