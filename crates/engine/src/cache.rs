//! Writes to `cache.db` for one account inside one writer transaction:
//! mailboxes, messages and locations, derived labels and thread rows, and
//! the change-log entries describing them (plan §3.2–3.6).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tagmail_api_core::Arrival;
use tagmail_api_core::dto::AddressDto;
use tagmail_core::ops::{self, ArchiveMode, MailboxView, Projected, Role};
use tagmail_core::{
    EmailId, Flags, KeywordClass, KeywordSet, MessageRowId, SpecialUse, classify_keyword,
};
use tagmail_imap::UidSet;
use tagmail_store::rusqlite::{Connection, OptionalExtension, params};
use tagmail_store::{ChangeOp, Entity, Error, FtsDocument, NewChange, Result, changes, fts};

use crate::message::{Fetched, LocationState, NewMessage, address_text};
use crate::overlay::AccountOverlay;
use crate::threading::{self, ThreadInput};

/// Older mail is restored from elsewhere, not news (contract §5).
const ARRIVAL_MAX_AGE_SECS: i64 = 24 * 3600;
const MS_PER_SEC: i64 = 1000;

/// Recent messages that a live mailbox other than INBOX inserted. INBOX
/// may list them only later (All Mail syncs before INBOX), and their first
/// INBOX location is then still an arrival. They are dropped once INBOX
/// has completed a whole poll after the one in progress, so a later move
/// into INBOX is no arrival.
#[derive(Debug, Default)]
pub struct InboxCandidates {
    inbox_polls: u64,
    /// Message id → `inbox_polls` when inserted.
    ids: HashMap<i64, u64>,
}

/// What one transaction does to [`InboxCandidates`], applied on commit.
#[derive(Debug, Default)]
pub struct CandidateChanges {
    added: Vec<i64>,
    taken: Vec<i64>,
    inbox_polled: bool,
}

impl InboxCandidates {
    pub fn apply(&mut self, changes: CandidateChanges) {
        for id in changes.taken {
            self.ids.remove(&id);
        }
        for id in changes.added {
            self.ids.insert(id, self.inbox_polls);
        }
        if changes.inbox_polled {
            self.inbox_polls += 1;
            let oldest = self.inbox_polls - 1;
            self.ids.retain(|_, polls| *polls >= oldest);
        }
    }
}

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
    /// per-message and per-thread entries, with the UID range they added
    /// unless the whole mailbox changed.
    bulk: BTreeMap<i64, Option<(u32, u32)>>,
    overlay: Arc<AccountOverlay>,
    /// The account's mailboxes `(id, name, special_use)`, read once per
    /// transaction while the overlay is not empty.
    views: Option<Vec<(i64, String, Option<SpecialUse>)>>,
    /// The account's INBOX if it is live, read once per transaction.
    live_inbox: Option<Option<i64>>,
    arrivals: Vec<Arrival>,
    candidates: Arc<Mutex<InboxCandidates>>,
    candidate_changes: CandidateChanges,
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
            bulk: BTreeMap::new(),
            overlay: Arc::default(),
            views: None,
            live_inbox: None,
            arrivals: Vec::new(),
            candidates: Arc::default(),
            candidate_changes: CandidateChanges::default(),
        }
    }

    pub fn with_candidates(mut self, candidates: Arc<Mutex<InboxCandidates>>) -> W<'a> {
        self.candidates = candidates;
        self
    }

    /// For [`InboxCandidates::apply`] once the transaction commits.
    pub fn take_candidate_changes(&mut self) -> CandidateChanges {
        std::mem::take(&mut self.candidate_changes)
    }

    fn is_candidate(&self, message: i64) -> bool {
        self.candidates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ids
            .contains_key(&message)
    }

    fn push_arrival(&mut self, message: i64, m: &NewMessage) -> Result<()> {
        let thread_id = self.conn.query_row(
            "SELECT thread_id FROM message WHERE id = ?1",
            [message],
            |r| r.get(0),
        )?;
        self.arrivals.push(Arrival {
            account_id: self.account,
            message_id: message,
            thread_id,
            from: m.from.first().cloned(),
            subject: m.subject.clone(),
            date: m.date,
        });
        Ok(())
    }

    pub fn with_overlay(mut self, overlay: Arc<AccountOverlay>) -> W<'a> {
        self.overlay = overlay;
        self
    }

    fn views(&mut self) -> Result<&[(i64, String, Option<SpecialUse>)]> {
        if self.views.is_none() {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, name, special_use FROM mailbox WHERE account_id = ?1",
            )?;
            let rows = stmt
                .query_map([self.account], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?;
            self.views = Some(
                rows.into_iter()
                    .map(|(id, name, u)| (id, name, u.and_then(|u| u.parse().ok())))
                    .collect(),
            );
        }
        Ok(self.views.as_deref().unwrap_or_default())
    }

    fn live_inbox(&mut self) -> Result<Option<i64>> {
        if let Some(id) = self.live_inbox {
            return Ok(id);
        }
        let id = self
            .conn
            .query_row(
                "SELECT id FROM mailbox WHERE account_id = ?1 AND name = 'INBOX' COLLATE NOCASE \
                 AND sync_state = 'live'",
                [self.account],
                |r| r.get(0),
            )
            .optional()?;
        self.live_inbox = Some(id);
        Ok(id)
    }

    /// Messages this transaction inserted that warrant a notification;
    /// published only once it commits.
    pub fn take_arrivals(&mut self) -> Vec<Arrival> {
        std::mem::take(&mut self.arrivals)
    }

    fn archive_mode(&mut self) -> Result<ArchiveMode> {
        let configured = self.overlay.archive_mode;
        let has_all = self
            .views()?
            .iter()
            .any(|(_, _, u)| *u == Some(SpecialUse::All));
        Ok(ArchiveMode::detect(configured, has_all))
    }

    /// The mailboxes of the account as the projection sees them.
    fn mailbox_views(&mut self) -> Result<Vec<(i64, String, Role, bool)>> {
        let mode = self.archive_mode()?;
        let views = self.views()?;
        let archive = ops::archive_mailbox(mode, views.iter().map(|(_, n, u)| (n.as_str(), *u)))
            .map(str::to_owned);
        Ok(views
            .iter()
            .map(|(id, name, u)| {
                (
                    *id,
                    name.clone(),
                    Role::of(name, *u),
                    archive.as_deref() == Some(name.as_str()),
                )
            })
            .collect())
    }

    fn email_id(&self, message: i64) -> Result<Option<EmailId>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT email_id FROM message WHERE id = ?1",
                [message],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id.and_then(|i| EmailId::parse(&i).ok()))
    }

    /// The server state of a location with the account's open ops on the
    /// message applied; `None` when no op touches the message.
    fn project(
        &mut self,
        mailbox: i64,
        message: i64,
        flags: Flags,
        keywords: &KeywordSet,
    ) -> Result<Option<Projected>> {
        if self.overlay.ops.is_empty() {
            return Ok(None);
        }
        let Some(email_id) = self.email_id(message)? else {
            return Ok(None);
        };
        if !self.overlay.ops.touches(&email_id) {
            return Ok(None);
        }
        let mode = self.archive_mode()?;
        let views = self.mailbox_views()?;
        let Some((_, name, role, is_archive)) = views.iter().find(|v| v.0 == mailbox) else {
            return Ok(None);
        };
        let view = MailboxView {
            name,
            role: *role,
            archive_mode: mode,
            is_archive: *is_archive,
        };
        let overlay = self.overlay.clone();
        Ok(Some(ops::project(
            overlay.ops.ops_for(&email_id),
            &view,
            flags,
            keywords.clone(),
        )))
    }

    /// Mailboxes the account's open moving ops put `message` into.
    fn pending_destinations(&mut self, message: i64) -> Result<Vec<i64>> {
        if self.overlay.ops.is_empty() {
            return Ok(Vec::new());
        }
        let Some(email_id) = self.email_id(message)? else {
            return Ok(Vec::new());
        };
        let overlay = self.overlay.clone();
        let destinations: Vec<_> = overlay
            .ops
            .ops_for(&email_id)
            .filter_map(|o| o.destination())
            .collect();
        if destinations.is_empty() {
            return Ok(Vec::new());
        }
        let mode = self.archive_mode()?;
        let views = self.mailbox_views()?;
        Ok(views
            .iter()
            .filter(|(_, name, role, is_archive)| {
                let view = MailboxView {
                    name,
                    role: *role,
                    archive_mode: mode,
                    is_archive: *is_archive,
                };
                destinations.iter().any(|d| d.matches(&view))
            })
            .map(|v| v.0)
            .collect())
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
        self.bulk.insert(mailbox, None);
    }

    /// [`W::bulk`] for a transaction that only added the locations of
    /// `mailbox` in `uids`.
    pub fn bulk_added(&mut self, mailbox: i64, uids: &UidSet) {
        let (Some(lo), Some(hi)) = (uids.min(), uids.max()) else {
            return self.bulk(mailbox);
        };
        self.bulk
            .entry(mailbox)
            .and_modify(|range| {
                *range = range.map(|(a, b)| (a.min(lo.0), b.max(hi.0)));
            })
            .or_insert(Some((lo.0, hi.0)));
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
        for (mailbox, uids) in std::mem::take(&mut self.bulk) {
            let payload = uids.map(|(lo, hi)| json!({ "uids": [lo, hi] }));
            self.emit(
                Entity::Mailbox,
                mailbox,
                ChangeOp::Bulk { mailbox },
                payload,
            );
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
    note_inbox_poll(w, mailbox)
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
    note_inbox_poll(w, mailbox)
}

fn note_inbox_poll(w: &mut W<'_>, mailbox: i64) -> Result<()> {
    let inbox: bool = w.conn.query_row(
        "SELECT name = 'INBOX' COLLATE NOCASE FROM mailbox WHERE id = ?1",
        [mailbox],
        |r| r.get(0),
    )?;
    w.candidate_changes.inbox_polled |= inbox;
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
        let unseen_recent = !f.location.flags.contains(Flags::SEEN)
            && m.date >= w.now_ms / MS_PER_SEC - ARRIVAL_MAX_AGE_SECS;
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
                if unseen_recent
                    && w.live_inbox()? == Some(mailbox)
                    && w.is_candidate(id)
                    && !has_location_in(w, mailbox, id)?
                {
                    w.candidate_changes.taken.push(id);
                    w.push_arrival(id, m)?;
                }
                id
            }
            None => {
                let id = insert_message(w, m)?;
                if unseen_recent {
                    if w.live_inbox()? == Some(mailbox) {
                        w.push_arrival(id, m)?;
                    } else if mailbox_state(w.conn, mailbox)? == SyncState::Live {
                        w.candidate_changes.added.push(id);
                    }
                }
                id
            }
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
        upsert_location(w, mailbox, id, &f.location)?;
        w.touch_message(id);
    }
    Ok(())
}

fn has_location_in(w: &W<'_>, mailbox: i64, message: i64) -> Result<bool> {
    Ok(w.conn
        .prepare_cached("SELECT 1 FROM location WHERE mailbox_id = ?1 AND message_id = ?2")?
        .exists(params![mailbox, message])?)
}

/// What a location row stores for a server state: the projected state,
/// plus the server state itself when an op is projected over it.
struct Stored {
    flags: u8,
    keywords: String,
    srv_flags: Option<u8>,
    srv_keywords: Option<String>,
    hidden: bool,
}

fn stored(
    w: &mut W<'_>,
    mailbox: i64,
    message: i64,
    flags: Flags,
    keywords: &KeywordSet,
) -> Result<Stored> {
    Ok(match w.project(mailbox, message, flags, keywords)? {
        None => Stored {
            flags: flags.bits(),
            keywords: keywords.to_storage(),
            srv_flags: None,
            srv_keywords: None,
            hidden: false,
        },
        Some(p) => Stored {
            flags: p.flags.bits(),
            keywords: p.keywords.to_storage(),
            srv_flags: Some(flags.bits()),
            srv_keywords: Some(keywords.to_storage()),
            hidden: p.hidden,
        },
    })
}

fn upsert_location(w: &mut W<'_>, mailbox: i64, message: i64, l: &LocationState) -> Result<()> {
    let s = stored(w, mailbox, message, l.flags, &l.keywords)?;
    let mut stmt = w.conn.prepare_cached(
        "INSERT INTO location (mailbox_id, uid, message_id, flags, keywords, modseq, \
         srv_flags, srv_keywords, hidden) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT (mailbox_id, uid) DO UPDATE SET message_id = excluded.message_id, \
         flags = excluded.flags, keywords = excluded.keywords, modseq = excluded.modseq, \
         srv_flags = excluded.srv_flags, srv_keywords = excluded.srv_keywords, \
         hidden = excluded.hidden",
    )?;
    stmt.execute(params![
        mailbox,
        l.uid,
        message,
        s.flags,
        s.keywords,
        modseq_sql(l.modseq),
        s.srv_flags,
        s.srv_keywords,
        s.hidden,
    ])?;
    Ok(())
}

/// `(mailbox_id, uid, flags, keywords, srv_flags, srv_keywords, hidden)`.
type StoredRow = (i64, u32, u8, String, Option<u8>, Option<String>, bool);

/// Re-projects the open ops over the server state of every location of
/// `messages`: after an op is added, completes, fails or is discarded.
pub fn reproject(w: &mut W<'_>, messages: &[i64]) -> Result<()> {
    for &message in messages {
        let rows: Vec<StoredRow> = {
            let mut stmt = w.conn.prepare_cached(
                "SELECT mailbox_id, uid, flags, keywords, srv_flags, srv_keywords, hidden \
                 FROM location WHERE message_id = ?1",
            )?;
            stmt.query_map([message], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<tagmail_store::rusqlite::Result<_>>()?
        };
        for (mailbox, uid, flags, keywords, srv_flags, srv_keywords, hidden) in rows {
            let server_flags = Flags::from_bits(srv_flags.unwrap_or(flags));
            let server_keywords =
                KeywordSet::from_storage(srv_keywords.as_deref().unwrap_or(&keywords))?;
            let s = stored(w, mailbox, message, server_flags, &server_keywords)?;
            if s.flags != flags
                || s.keywords != keywords
                || s.srv_flags != srv_flags
                || s.srv_keywords != srv_keywords
                || s.hidden != hidden
            {
                w.conn.execute(
                    "UPDATE location SET flags = ?3, keywords = ?4, srv_flags = ?5, \
                     srv_keywords = ?6, hidden = ?7 WHERE mailbox_id = ?1 AND uid = ?2",
                    params![
                        mailbox,
                        uid,
                        s.flags,
                        s.keywords,
                        s.srv_flags,
                        s.srv_keywords,
                        s.hidden
                    ],
                )?;
            }
        }
        w.touch_message(message);
    }
    Ok(())
}

/// Messages of the account with an overlay stored on any location.
pub fn overlaid_messages(conn: &Connection, account: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT l.message_id FROM location l JOIN message m ON m.id = l.message_id \
         WHERE m.account_id = ?1 AND (l.srv_flags IS NOT NULL OR l.hidden = 1)",
    )?;
    let rows = stmt
        .query_map([account], |r| r.get(0))?
        .collect::<tagmail_store::rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Message rows of the account for `email_ids` that exist.
pub fn messages_by_email_id(
    conn: &Connection,
    account: i64,
    email_ids: &[String],
) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM message WHERE account_id = ?1 AND email_id = ?2")?;
    let mut out = Vec::new();
    for e in email_ids {
        if let Some(id) = stmt
            .query_row(params![account, e], |r| r.get::<_, i64>(0))
            .optional()?
        {
            out.push(id);
        }
    }
    Ok(out)
}

/// Applies FETCHed flags and keywords to known locations. Unknown UIDs are
/// ignored (they are fetched as new messages). Returns how many changed.
pub fn update_flags(w: &mut W<'_>, mailbox: i64, states: &[LocationState]) -> Result<usize> {
    let mut changed = 0;
    for s in states {
        let row: Option<(i64, u8, String, bool)> = w
            .conn
            .query_row(
                "SELECT message_id, flags, keywords, hidden FROM location \
                 WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox, s.uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((message, flags, keywords, hidden)) = row else {
            continue;
        };
        let new = stored(w, mailbox, message, s.flags, &s.keywords)?;
        let differs = flags != new.flags
            || !keywords.eq_ignore_ascii_case(&new.keywords)
            || hidden != new.hidden;
        w.conn.execute(
            "UPDATE location SET flags = ?3, keywords = ?4, modseq = coalesce(?5, modseq), \
             srv_flags = ?6, srv_keywords = ?7, hidden = ?8 WHERE mailbox_id = ?1 AND uid = ?2",
            params![
                mailbox,
                s.uid,
                new.flags,
                new.keywords,
                modseq_sql(s.modseq),
                new.srv_flags,
                new.srv_keywords,
                new.hidden
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

/// Deletes up to `limit` of the account's messages with their index rows,
/// locations and labels; once none are left, its threads, labels and
/// mailboxes, each mailbox reported as one bulk change. Returns the
/// deleted `email_id`s, for their blobs, and whether the account is gone.
pub fn purge_account(w: &mut W<'_>, limit: usize) -> Result<(Vec<String>, bool)> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = w
            .conn
            .prepare_cached("SELECT id, email_id FROM message WHERE account_id = ?1 LIMIT ?2")?;
        stmt.query_map(
            params![w.account, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<tagmail_store::rusqlite::Result<_>>()?
    };
    let mut purged = Vec::with_capacity(rows.len());
    for (id, email_id) in rows {
        fts::delete(w.conn, MessageRowId(id))?;
        w.conn.execute("DELETE FROM message WHERE id = ?1", [id])?;
        purged.push(email_id);
    }
    if purged.len() == limit {
        return Ok((purged, false));
    }
    for table in ["thread_ref", "label", "thread"] {
        w.conn.execute(
            &format!("DELETE FROM {table} WHERE account_id = ?1"),
            [w.account],
        )?;
    }
    let mailboxes: Vec<i64> = {
        let mut stmt = w
            .conn
            .prepare_cached("SELECT id FROM mailbox WHERE account_id = ?1")?;
        stmt.query_map([w.account], |r| r.get(0))?
            .collect::<tagmail_store::rusqlite::Result<_>>()?
    };
    w.conn
        .execute("DELETE FROM mailbox WHERE account_id = ?1", [w.account])?;
    for mailbox in mailboxes {
        w.bulk(mailbox);
    }
    Ok((purged, true))
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
         WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL AND l.hidden = 0 \
         GROUP BY l.mailbox_id",
        [thread],
    )?;
    if !w.overlay.ops.is_empty() {
        let members: Vec<(i64, i64)> = {
            let mut stmt = w.conn.prepare_cached(
                "SELECT id, date FROM message WHERE thread_id = ?1 AND tombstoned_at IS NULL",
            )?;
            stmt.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<tagmail_store::rusqlite::Result<_>>()?
        };
        for (message, date) in members {
            for mailbox in w.pending_destinations(message)? {
                w.conn.execute(
                    "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) \
                     VALUES (?1, ?2, ?3) ON CONFLICT (thread_id, mailbox_id) \
                     DO UPDATE SET last_date = max(last_date, excluded.last_date)",
                    params![thread, mailbox, date],
                )?;
            }
        }
    }
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
        let labels: Vec<String> = {
            let mut stmt = w.conn.prepare_cached(
                "SELECT DISTINCT l.label_key FROM message m \
                 JOIN message_label ml ON ml.message_id = m.id \
                 JOIN label l ON l.id = ml.label_id \
                 WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL ORDER BY l.label_key",
            )?;
            stmt.query_map([thread], |r| r.get(0))?
                .collect::<tagmail_store::rusqlite::Result<_>>()?
        };
        w.emit(
            Entity::Thread,
            thread,
            ChangeOp::Upsert,
            Some(json!({
                "accountId": w.account,
                "lastDate": last_date,
                "mailboxes": after.keys().collect::<Vec<_>>(),
                "removedFrom": removed,
                "labels": labels,
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
