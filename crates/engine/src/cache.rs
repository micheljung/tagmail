//! Writes to `cache.db` for one account inside one writer transaction:
//! mailboxes, messages and locations, derived labels and thread rows, and
//! the change-log entries describing them (plan §3.2–3.6).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use tagmail_api_core::dto::AddressDto;
use tagmail_core::{KeywordClass, KeywordSet, MessageRowId, SpecialUse, classify_keyword};
use tagmail_imap::UidSet;
use tagmail_store::rusqlite::{Connection, OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, Error, FtsDocument, NewChange, Result, changes, fts};

use crate::message::{Fetched, LocationState, NewMessage, address_text};
use crate::threading::{self, ThreadInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Initial,
    Catchup,
    Live,
}

impl SyncState {
    fn as_str(self) -> &'static str {
        match self {
            SyncState::Initial => "initial",
            SyncState::Catchup => "catchup",
            SyncState::Live => "live",
        }
    }

    fn parse(s: &str) -> Result<SyncState> {
        match s {
            "initial" => Ok(SyncState::Initial),
            "catchup" => Ok(SyncState::Catchup),
            "live" => Ok(SyncState::Live),
            _ => Err(Error::Corrupt(format!("mailbox sync_state {s:?}"))),
        }
    }
}

/// A `mailbox` row. `uidnext` is the bound of the initial sync while it
/// runs and the last seen UIDNEXT afterwards; `sync_cursor_uid` is the
/// lowest UID of the last committed window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxRow {
    pub id: i64,
    pub name: String,
    pub special_use: Option<SpecialUse>,
    pub uidvalidity: u32,
    pub uidnext: Option<u32>,
    pub highestmodseq: Option<u64>,
    pub nomodseq: bool,
    pub keyword_capable: bool,
    pub state: SyncState,
    pub cursor: Option<u32>,
    pub snapshot: Option<u64>,
    pub last_poll_at: Option<i64>,
}

impl MailboxRow {
    pub fn is_inbox(&self) -> bool {
        self.name.eq_ignore_ascii_case("INBOX")
    }
}

/// A mailbox as LISTed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMailbox {
    pub name: String,
    pub delimiter: Option<char>,
    pub special_use: Option<SpecialUse>,
}

fn corrupt(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Corrupt(format!("{what}: {e}"))
}

fn to_u32(v: Option<i64>) -> Option<u32> {
    v.and_then(|v| u32::try_from(v).ok())
}

fn to_u64(v: Option<i64>) -> Option<u64> {
    v.and_then(|v| u64::try_from(v).ok())
}

fn modseq_sql(v: Option<u64>) -> Option<i64> {
    v.and_then(|v| i64::try_from(v).ok())
}

const MAILBOX_COLUMNS: &str = "id, name, special_use, uidvalidity, uidnext, highestmodseq, \
     nomodseq, keyword_capable, sync_state, sync_cursor_uid, snapshot_modseq, last_poll_at";

fn mailbox_row(
    r: &tagmail_store::rusqlite::Row<'_>,
) -> tagmail_store::rusqlite::Result<RawMailbox> {
    Ok(RawMailbox {
        id: r.get(0)?,
        name: r.get(1)?,
        special_use: r.get(2)?,
        uidvalidity: r.get(3)?,
        uidnext: r.get(4)?,
        highestmodseq: r.get(5)?,
        nomodseq: r.get(6)?,
        keyword_capable: r.get(7)?,
        state: r.get(8)?,
        cursor: r.get(9)?,
        snapshot: r.get(10)?,
        last_poll_at: r.get(11)?,
    })
}

struct RawMailbox {
    id: i64,
    name: String,
    special_use: Option<String>,
    uidvalidity: i64,
    uidnext: Option<i64>,
    highestmodseq: Option<i64>,
    nomodseq: bool,
    keyword_capable: bool,
    state: String,
    cursor: Option<i64>,
    snapshot: Option<i64>,
    last_poll_at: Option<i64>,
}

impl RawMailbox {
    fn into_row(self) -> Result<MailboxRow> {
        Ok(MailboxRow {
            id: self.id,
            special_use: self.special_use.as_deref().and_then(|s| s.parse().ok()),
            name: self.name,
            uidvalidity: u32::try_from(self.uidvalidity).unwrap_or(0),
            uidnext: to_u32(self.uidnext),
            highestmodseq: to_u64(self.highestmodseq),
            nomodseq: self.nomodseq,
            keyword_capable: self.keyword_capable,
            state: SyncState::parse(&self.state)?,
            cursor: to_u32(self.cursor),
            snapshot: to_u64(self.snapshot),
            last_poll_at: self.last_poll_at,
        })
    }
}

pub fn mailboxes(conn: &Connection, account: i64) -> Result<Vec<MailboxRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MAILBOX_COLUMNS} FROM mailbox WHERE account_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt
        .query_map([account], mailbox_row)?
        .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(RawMailbox::into_row).collect()
}

pub fn mailbox(conn: &Connection, id: i64) -> Result<MailboxRow> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MAILBOX_COLUMNS} FROM mailbox WHERE id = ?1"
    ))?;
    stmt.query_row([id], mailbox_row)?.into_row()
}

/// UIDs of the mailbox's locations.
pub fn known_uids(conn: &Connection, mailbox: i64) -> Result<UidSet> {
    let mut stmt =
        conn.prepare_cached("SELECT uid FROM location WHERE mailbox_id = ?1 ORDER BY uid")?;
    let mut set = UidSet::new();
    for uid in stmt.query_map([mailbox], |r| r.get::<_, u32>(0))? {
        let uid = uid?;
        set.insert_range(uid, uid);
    }
    Ok(set)
}

pub fn location_count(conn: &Connection, mailbox: i64) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT count(*) FROM location WHERE mailbox_id = ?1",
        [mailbox],
        |r| r.get(0),
    )?)
}

/// A message still missing its text blob, at one of its locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingText {
    pub message: i64,
    pub email_id: String,
    pub mailbox: i64,
    pub uid: u32,
}

/// Served by the partial index `message_pending_text`.
pub(crate) const PENDING_TEXT_SQL: &str = "SELECT m.id, m.email_id, \
       (SELECT l.mailbox_id FROM location l WHERE l.message_id = m.id ORDER BY l.mailbox_id LIMIT 1), \
       (SELECT l.uid FROM location l WHERE l.message_id = m.id ORDER BY l.mailbox_id LIMIT 1) \
     FROM message m \
     WHERE m.account_id = ?1 AND m.text_blob IS NULL AND m.tombstoned_at IS NULL \
     ORDER BY m.date DESC, m.id DESC LIMIT ?2";

/// Up to `limit` messages without text, newest first, skipping `exclude`.
pub fn pending_text(
    conn: &Connection,
    account: i64,
    limit: usize,
    exclude: &BTreeSet<i64>,
) -> Result<Vec<PendingText>> {
    let mut stmt = conn.prepare_cached(PENDING_TEXT_SQL)?;
    let fetch = i64::try_from(limit + exclude.len()).unwrap_or(i64::MAX);
    let rows = stmt.query_map(params![account, fetch], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<u32>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (message, email_id, mailbox, uid) = row?;
        if let (Some(mailbox), Some(uid)) = (mailbox, uid)
            && !exclude.contains(&message)
        {
            out.push(PendingText {
                message,
                email_id,
                mailbox,
                uid,
            });
        }
        if out.len() == limit {
            break;
        }
    }
    Ok(out)
}

/// One writer transaction for an account. Collects the messages and threads
/// it touched and derives labels, thread rows and change-log entries from
/// them in [`W::finish`].
pub struct W<'a> {
    pub conn: &'a Connection,
    pub account: i64,
    /// Unix milliseconds of this transaction; tombstones and polls compare
    /// against it.
    pub now_ms: i64,
    changes: Vec<NewChange>,
    messages: BTreeSet<i64>,
    threads: BTreeSet<i64>,
    /// Initial-sync windows emit one `bulk:<mailbox>` entry instead of
    /// per-message and per-thread entries.
    bulk: BTreeSet<i64>,
}

impl<'a> W<'a> {
    pub fn new(conn: &'a Connection, account: i64, now_ms: i64) -> W<'a> {
        W {
            conn,
            account,
            now_ms,
            changes: Vec::new(),
            messages: BTreeSet::new(),
            threads: BTreeSet::new(),
            bulk: BTreeSet::new(),
        }
    }

    pub fn emit(&mut self, entity: Entity, id: i64, op: ChangeOp, payload: Option<Value>) {
        self.changes.push(NewChange {
            entity,
            entity_id: id,
            op,
            payload: payload.map(|p| p.to_string()),
        });
    }

    pub fn touch_message(&mut self, id: i64) {
        self.messages.insert(id);
    }

    pub fn touch_thread(&mut self, id: i64) {
        self.threads.insert(id);
    }

    /// Reports the changes of this transaction as one bulk entry for
    /// `mailbox`.
    pub fn bulk(&mut self, mailbox: i64) {
        self.bulk.insert(mailbox);
    }

    /// Derives labels, tombstones and thread rows for everything touched and
    /// appends the change-log entries. Returns whether any were appended.
    pub fn finish(mut self) -> Result<bool> {
        let bulk = !self.bulk.is_empty();
        let messages = std::mem::take(&mut self.messages);
        for &id in &messages {
            let Some((thread, tombstoned)) = self
                .conn
                .query_row(
                    "SELECT thread_id, tombstoned_at FROM message WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .optional()?
            else {
                continue;
            };
            let has_location = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM location WHERE message_id = ?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )?;
            if !has_location && tombstoned.is_none() {
                self.conn.execute(
                    "UPDATE message SET tombstoned_at = ?2 WHERE id = ?1",
                    params![id, self.now_ms],
                )?;
                if !bulk {
                    self.emit(Entity::Message, id, ChangeOp::Delete, None);
                }
            } else if has_location && !bulk {
                self.emit(
                    Entity::Message,
                    id,
                    ChangeOp::Upsert,
                    Some(json!({ "threadId": thread })),
                );
            }
            derive_labels(&mut self, id)?;
            if let Some(thread) = thread {
                self.threads.insert(thread);
            }
        }
        for thread in std::mem::take(&mut self.threads) {
            refresh_thread(&mut self, thread, bulk)?;
        }
        for mailbox in std::mem::take(&mut self.bulk) {
            self.emit(Entity::Mailbox, mailbox, ChangeOp::Bulk { mailbox }, None);
        }
        Ok(changes::append_all(self.conn, &self.changes)?.is_some())
    }
}

/// Reconciles the account's mailbox rows with a LIST result; removed
/// mailboxes drop their locations (their messages become orphans).
pub fn sync_mailbox_list(w: &mut W<'_>, listed: &[ListedMailbox]) -> Result<()> {
    let existing = mailboxes(w.conn, w.account)?;
    for row in &existing {
        if !listed.iter().any(|l| l.name == row.name) {
            for message in delete_locations(w.conn, row.id, None)? {
                w.touch_message(message);
            }
            let threads: Vec<i64> = {
                let mut stmt = w
                    .conn
                    .prepare_cached("SELECT thread_id FROM thread_mailbox WHERE mailbox_id = ?1")?;
                stmt.query_map([row.id], |r| r.get(0))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?
            };
            for t in threads {
                w.touch_thread(t);
            }
            w.conn
                .execute("DELETE FROM mailbox WHERE id = ?1", [row.id])?;
            w.emit(Entity::Mailbox, row.id, ChangeOp::Delete, None);
        }
    }
    for l in listed {
        let special = l.special_use.map(|s| s.as_imap());
        let delimiter = l.delimiter.map(String::from);
        match existing.iter().find(|r| r.name == l.name) {
            Some(row) if row.special_use == l.special_use => {}
            Some(row) => {
                w.conn.execute(
                    "UPDATE mailbox SET special_use = ?2, delimiter = ?3 WHERE id = ?1",
                    params![row.id, special, delimiter],
                )?;
                w.emit(
                    Entity::Mailbox,
                    row.id,
                    ChangeOp::Upsert,
                    Some(json!({ "name": l.name, "specialUse": special })),
                );
            }
            None => {
                w.conn.execute(
                    "INSERT INTO mailbox (account_id, name, delimiter, special_use, uidvalidity) \
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    params![w.account, l.name, delimiter, special],
                )?;
                let id = w.conn.last_insert_rowid();
                w.emit(
                    Entity::Mailbox,
                    id,
                    ChangeOp::Upsert,
                    Some(json!({ "name": l.name, "specialUse": special })),
                );
            }
        }
    }
    Ok(())
}

/// Deletes the mailbox's locations (all, or those in `uids`); returns the
/// affected message ids.
fn delete_locations(conn: &Connection, mailbox: i64, uids: Option<&UidSet>) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    let ranges: Vec<(u32, u32)> = match uids {
        Some(set) => set.ranges().to_vec(),
        None => vec![(0, u32::MAX)],
    };
    let mut stmt = conn.prepare_cached(
        "DELETE FROM location WHERE mailbox_id = ?1 AND uid BETWEEN ?2 AND ?3 RETURNING message_id",
    )?;
    for (lo, hi) in ranges {
        for id in stmt.query_map(params![mailbox, lo, hi], |r| r.get::<_, i64>(0))? {
            out.push(id?);
        }
    }
    Ok(out)
}

/// UIDVALIDITY changed: forget the mailbox's locations and restart its
/// initial sync. Messages stay (tombstoned until seen again) and are
/// matched back by `email_id`.
pub fn reset_mailbox(w: &mut W<'_>, mailbox: i64) -> Result<()> {
    for message in delete_locations(w.conn, mailbox, None)? {
        w.touch_message(message);
    }
    w.conn.execute(
        "UPDATE mailbox SET uidvalidity = 0, uidnext = NULL, highestmodseq = NULL, \
         sync_state = 'initial', sync_cursor_uid = NULL, snapshot_modseq = NULL WHERE id = ?1",
        [mailbox],
    )?;
    Ok(())
}

/// Records the SELECT snapshot an initial sync starts from.
pub fn start_initial(
    w: &mut W<'_>,
    mailbox: i64,
    uidvalidity: u32,
    bound: u32,
    snapshot: Option<u64>,
    nomodseq: bool,
    keyword_capable: bool,
) -> Result<()> {
    w.conn.execute(
        "UPDATE mailbox SET uidvalidity = ?2, uidnext = ?3, snapshot_modseq = ?4, nomodseq = ?5, \
         keyword_capable = ?6, sync_state = 'initial', sync_cursor_uid = NULL, \
         highestmodseq = NULL WHERE id = ?1",
        params![
            mailbox,
            uidvalidity,
            bound,
            modseq_sql(snapshot),
            nomodseq,
            keyword_capable
        ],
    )?;
    Ok(())
}

pub fn set_cursor(w: &mut W<'_>, mailbox: i64, cursor: u32) -> Result<()> {
    w.conn.execute(
        "UPDATE mailbox SET sync_cursor_uid = ?2 WHERE id = ?1",
        params![mailbox, cursor],
    )?;
    Ok(())
}

pub fn set_state(w: &mut W<'_>, mailbox: i64, state: SyncState) -> Result<()> {
    w.conn.execute(
        "UPDATE mailbox SET sync_state = ?2 WHERE id = ?1",
        params![mailbox, state.as_str()],
    )?;
    Ok(())
}

/// The mailbox is in sync with the server as of `highestmodseq` /
/// `uidnext`: commit them, go live and count it as polled.
pub fn finish_sync(
    w: &mut W<'_>,
    mailbox: i64,
    highestmodseq: Option<u64>,
    uidnext: u32,
    nomodseq: bool,
    keyword_capable: bool,
) -> Result<()> {
    let state = mailbox_state(w.conn, mailbox)?;
    w.conn.execute(
        "UPDATE mailbox SET highestmodseq = ?2, uidnext = ?3, nomodseq = ?4, keyword_capable = ?5, \
         sync_state = 'live', sync_cursor_uid = NULL, snapshot_modseq = NULL, last_poll_at = ?6 \
         WHERE id = ?1",
        params![
            mailbox,
            modseq_sql(highestmodseq),
            uidnext,
            nomodseq,
            keyword_capable,
            w.now_ms
        ],
    )?;
    if state != SyncState::Live {
        w.emit(
            Entity::Mailbox,
            mailbox,
            ChangeOp::Upsert,
            Some(json!({ "syncState": "live" })),
        );
    }
    Ok(())
}

fn mailbox_state(conn: &Connection, mailbox: i64) -> Result<SyncState> {
    let s: String = conn.query_row(
        "SELECT sync_state FROM mailbox WHERE id = ?1",
        [mailbox],
        |r| r.get(0),
    )?;
    SyncState::parse(&s)
}

pub fn mark_polled(w: &mut W<'_>, mailbox: i64) -> Result<()> {
    w.conn.execute(
        "UPDATE mailbox SET last_poll_at = ?2 WHERE id = ?1",
        params![mailbox, w.now_ms],
    )?;
    Ok(())
}

fn json_addresses(list: &[AddressDto]) -> Result<Option<String>> {
    if list.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(list)
        .map(Some)
        .map_err(|e| corrupt("address json", e))
}

pub fn parse_addresses(json: Option<&str>) -> Vec<AddressDto> {
    json.and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default()
}

/// Inserts a new message row, threads it and indexes its headers.
fn insert_message(w: &mut W<'_>, m: &NewMessage) -> Result<i64> {
    w.conn.execute(
        "INSERT INTO message (account_id, email_id, msgid_hdr, in_reply_to, date, subject_norm, \
         from_json, to_json, cc_json, subject, snippet, size, has_attachment) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            w.account,
            m.email_id.as_str(),
            m.msgid,
            m.in_reply_to,
            m.date,
            m.subject_norm,
            json_addresses(&m.from)?,
            json_addresses(&m.to)?,
            json_addresses(&m.cc)?,
            m.subject,
            m.snippet,
            m.size,
            m.has_attachment,
        ],
    )?;
    let id = w.conn.last_insert_rowid();
    threading::assign(
        w,
        id,
        &ThreadInput {
            msgid: m.msgid.clone(),
            refs: m.refs.clone(),
            subject_norm: m.subject_norm.clone(),
            date: m.date,
            participants: m.participants(),
        },
    )?;
    fts::index(
        w.conn,
        MessageRowId(id),
        &FtsDocument {
            subject: m.subject.as_deref().unwrap_or_default(),
            from: &address_text(&[&m.from]),
            to: &address_text(&[&m.to, &m.cc]),
            body: "",
        },
    )?;
    Ok(id)
}

/// Adds or updates the locations of a window; messages are deduplicated
/// across mailboxes by `email_id`.
pub fn add_messages(w: &mut W<'_>, mailbox: i64, fetched: &[Fetched]) -> Result<()> {
    for f in fetched {
        let Some(m) = &f.message else {
            continue;
        };
        let existing: Option<(i64, Option<i64>, Option<String>)> = w
            .conn
            .query_row(
                "SELECT id, tombstoned_at, snippet FROM message \
                 WHERE account_id = ?1 AND email_id = ?2",
                params![w.account, m.email_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let id = match existing {
            Some((id, tombstoned, snippet)) => {
                if tombstoned.is_some() {
                    w.conn.execute(
                        "UPDATE message SET tombstoned_at = NULL WHERE id = ?1",
                        [id],
                    )?;
                }
                if snippet.is_none() && m.snippet.is_some() {
                    w.conn.execute(
                        "UPDATE message SET snippet = ?2 WHERE id = ?1",
                        params![id, m.snippet],
                    )?;
                }
                id
            }
            None => insert_message(w, m)?,
        };
        let previous: Option<i64> = w
            .conn
            .query_row(
                "SELECT message_id FROM location WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox, f.location.uid],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous.filter(|p| *p != id) {
            w.touch_message(previous);
        }
        upsert_location(w.conn, mailbox, id, &f.location)?;
        w.touch_message(id);
    }
    Ok(())
}

fn upsert_location(conn: &Connection, mailbox: i64, message: i64, l: &LocationState) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO location (mailbox_id, uid, message_id, flags, keywords, modseq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (mailbox_id, uid) DO UPDATE SET message_id = excluded.message_id, \
         flags = excluded.flags, keywords = excluded.keywords, modseq = excluded.modseq",
    )?;
    stmt.execute(params![
        mailbox,
        l.uid,
        message,
        l.flags.bits(),
        l.keywords.to_storage(),
        modseq_sql(l.modseq),
    ])?;
    Ok(())
}

/// Applies FETCHed flags and keywords to known locations. Unknown UIDs are
/// ignored (they are fetched as new messages). Returns how many changed.
pub fn update_flags(w: &mut W<'_>, mailbox: i64, states: &[LocationState]) -> Result<usize> {
    let mut changed = 0;
    for s in states {
        let row: Option<(i64, u8, String)> = w
            .conn
            .query_row(
                "SELECT message_id, flags, keywords FROM location WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox, s.uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((message, flags, keywords)) = row else {
            continue;
        };
        let new_keywords = s.keywords.to_storage();
        let differs = flags != s.flags.bits() || !keywords.eq_ignore_ascii_case(&new_keywords);
        w.conn.execute(
            "UPDATE location SET flags = ?3, keywords = ?4, modseq = coalesce(?5, modseq) \
             WHERE mailbox_id = ?1 AND uid = ?2",
            params![
                mailbox,
                s.uid,
                s.flags.bits(),
                new_keywords,
                modseq_sql(s.modseq)
            ],
        )?;
        if differs {
            changed += 1;
            w.touch_message(message);
        }
    }
    Ok(changed)
}

/// Removes expunged locations; messages left without one are tombstoned.
pub fn remove_uids(w: &mut W<'_>, mailbox: i64, uids: &UidSet) -> Result<usize> {
    if uids.is_empty() {
        return Ok(0);
    }
    let affected = delete_locations(w.conn, mailbox, Some(uids))?;
    let n = affected.len();
    for message in affected {
        w.touch_message(message);
    }
    Ok(n)
}

/// Deletes tombstoned messages once every mailbox of the account finished
/// a sync after the tombstone (orphan grace, plan §3.4). Returns the
/// purged `email_id`s so their blobs can be deleted.
pub fn purge_orphans(w: &mut W<'_>) -> Result<Vec<String>> {
    let floor: Option<i64> = w.conn.query_row(
        "SELECT min(coalesce(last_poll_at, 0)) FROM mailbox WHERE account_id = ?1",
        [w.account],
        |r| r.get(0),
    )?;
    let Some(floor) = floor else {
        return Ok(Vec::new());
    };
    let rows: Vec<(i64, String, Option<i64>)> = {
        let mut stmt = w.conn.prepare_cached(
            "SELECT id, email_id, thread_id FROM message \
             WHERE account_id = ?1 AND tombstoned_at IS NOT NULL AND tombstoned_at < ?2",
        )?;
        stmt.query_map(params![w.account, floor], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<tagmail_store::rusqlite::Result<_>>()?
    };
    let mut purged = Vec::with_capacity(rows.len());
    for (id, email_id, thread) in rows {
        fts::delete(w.conn, MessageRowId(id))?;
        w.conn.execute("DELETE FROM message WHERE id = ?1", [id])?;
        if let Some(thread) = thread {
            w.touch_thread(thread);
        }
        purged.push(email_id);
    }
    Ok(purged)
}

/// Records the text blob, fills a missing snippet and re-indexes the
/// message with its body.
pub fn set_text(w: &mut W<'_>, message: i64, blob: &str, snippet: &str, body: &str) -> Result<()> {
    let row: Option<[Option<String>; 4]> = w
        .conn
        .query_row(
            "SELECT subject, from_json, to_json, cc_json FROM message WHERE id = ?1",
            [message],
            |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]),
        )
        .optional()?;
    let Some([subject, from, to, cc]) = row else {
        return Ok(());
    };
    w.conn.execute(
        "UPDATE message SET text_blob = ?2, \
         snippet = CASE WHEN snippet IS NULL OR snippet = '' THEN nullif(?3, '') ELSE snippet END \
         WHERE id = ?1",
        params![message, blob, snippet],
    )?;
    let from = parse_addresses(from.as_deref());
    let to = parse_addresses(to.as_deref());
    let cc = parse_addresses(cc.as_deref());
    fts::index(
        w.conn,
        MessageRowId(message),
        &FtsDocument {
            subject: subject.as_deref().unwrap_or_default(),
            from: &address_text(&[&from]),
            to: &address_text(&[&to, &cc]),
            body,
        },
    )?;
    Ok(())
}

/// `message_label` = union of the keyword labels over the message's
/// locations (plan §3.3).
fn derive_labels(w: &mut W<'_>, message: i64) -> Result<()> {
    let mut union = KeywordSet::new();
    {
        let mut stmt = w
            .conn
            .prepare_cached("SELECT keywords FROM location WHERE message_id = ?1")?;
        for kw in stmt.query_map([message], |r| r.get::<_, String>(0))? {
            for k in KeywordSet::from_storage(&kw?)?.iter() {
                union.insert(k.clone());
            }
        }
    }
    let mut want = BTreeSet::new();
    for keyword in union.iter() {
        if let KeywordClass::Label { name, key } = classify_keyword(keyword) {
            let canonical =
                tagmail_core::canonical_label_keyword(keyword).unwrap_or_else(|| keyword.clone());
            want.insert(ensure_label(w, &name, key.as_str(), canonical.as_str())?);
        }
    }
    let have: BTreeSet<i64> = {
        let mut stmt = w
            .conn
            .prepare_cached("SELECT label_id FROM message_label WHERE message_id = ?1")?;
        stmt.query_map([message], |r| r.get(0))?
            .collect::<tagmail_store::rusqlite::Result<_>>()?
    };
    for label in have.difference(&want) {
        w.conn.execute(
            "DELETE FROM message_label WHERE message_id = ?1 AND label_id = ?2",
            params![message, label],
        )?;
    }
    for label in want.difference(&have) {
        w.conn.execute(
            "INSERT INTO message_label (message_id, label_id) VALUES (?1, ?2)",
            params![message, label],
        )?;
    }
    Ok(())
}

fn ensure_label(w: &mut W<'_>, name: &str, key: &str, keyword: &str) -> Result<i64> {
    let id: Option<i64> = w
        .conn
        .query_row(
            "SELECT id FROM label WHERE account_id = ?1 AND label_key = ?2 AND kind = 'keyword'",
            params![w.account, key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        return Ok(id);
    }
    w.conn.execute(
        "INSERT INTO label (account_id, keyword, label_key, kind) VALUES (?1, ?2, ?3, 'keyword')",
        params![w.account, keyword, key],
    )?;
    let id = w.conn.last_insert_rowid();
    w.emit(
        Entity::Label,
        id,
        ChangeOp::Upsert,
        Some(json!({ "key": key, "name": name, "keyword": keyword })),
    );
    Ok(id)
}

/// Recomputes a thread's counts and its `thread_mailbox` rows from its
/// visible messages; deletes it once it has no messages at all.
fn refresh_thread(w: &mut W<'_>, thread: i64, bulk: bool) -> Result<()> {
    let row: Option<Option<i64>> = w
        .conn
        .query_row(
            "SELECT merged_into FROM thread WHERE id = ?1",
            [thread],
            |r| r.get(0),
        )
        .optional()?;
    let Some(merged_into) = row else {
        return Ok(());
    };
    if merged_into.is_some() {
        return Ok(());
    }
    let total: i64 = w.conn.query_row(
        "SELECT count(*) FROM message WHERE thread_id = ?1",
        [thread],
        |r| r.get(0),
    )?;
    if total == 0 {
        w.conn
            .execute("DELETE FROM thread WHERE id = ?1", [thread])?;
        if !bulk {
            w.emit(Entity::Thread, thread, ChangeOp::Delete, None);
        }
        return Ok(());
    }
    let (count, last_date, unread, starred): (u32, Option<i64>, u32, bool) = w.conn.query_row(
        "SELECT count(*), max(m.date), \
           sum(EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & 1) = 0)), \
           coalesce(max(EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & 2) != 0)), 0) \
         FROM message m WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL",
        [thread],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<u32>>(2)?.unwrap_or(0),
                r.get(3)?,
            ))
        },
    )?;
    w.conn.execute(
        "UPDATE thread SET msg_count = ?2, unread_count = ?3, starred = ?4, \
         last_date = coalesce(?5, last_date) WHERE id = ?1",
        params![thread, count, unread, starred, last_date],
    )?;
    let before = thread_mailboxes(w.conn, thread)?;
    w.conn
        .execute("DELETE FROM thread_mailbox WHERE thread_id = ?1", [thread])?;
    w.conn.execute(
        "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) \
         SELECT ?1, l.mailbox_id, max(m.date) FROM message m \
         JOIN location l ON l.message_id = m.id \
         WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL GROUP BY l.mailbox_id",
        [thread],
    )?;
    if !bulk {
        let after = thread_mailboxes(w.conn, thread)?;
        let removed: Vec<i64> = before
            .keys()
            .filter(|m| !after.contains_key(m))
            .copied()
            .collect();
        let last_date: i64 = w.conn.query_row(
            "SELECT last_date FROM thread WHERE id = ?1",
            [thread],
            |r| r.get(0),
        )?;
        w.emit(
            Entity::Thread,
            thread,
            ChangeOp::Upsert,
            Some(json!({
                "lastDate": last_date,
                "mailboxes": after.keys().collect::<Vec<_>>(),
                "removedFrom": removed,
            })),
        );
    }
    Ok(())
}

fn thread_mailboxes(conn: &Connection, thread: i64) -> Result<BTreeMap<i64, i64>> {
    let mut stmt = conn
        .prepare_cached("SELECT mailbox_id, last_date FROM thread_mailbox WHERE thread_id = ?1")?;
    let rows = stmt
        .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<tagmail_store::rusqlite::Result<_>>()?;
    Ok(rows)
}
