//! CalDAV settings in `config.db` and the calendar cache in `cache.db`
//! (appointments plan §3.2). Rows that leave `cache.db` are keyed by
//! (account, calendar href, UID), never by the cache's row ids.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use tagmail_core::AccountId;

use crate::blobs::ZSTD_LEVEL;
use crate::changes::{self, ChangeOp, Entity, NewChange};
use crate::credentials::Sealed;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheduling {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaldavStatus {
    #[default]
    Unconfigured,
    /// Discovery ended on a host the owner has to confirm before any
    /// authenticated request.
    NeedsConfirmation,
    Ok,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    WellKnown,
    Manual,
    Srv,
}

macro_rules! sql_enum {
    ($ty:ty, $($variant:path => $text:literal),+ $(,)?) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self {
                    $($variant => $text),+
                }
            }

            pub fn parse(text: &str) -> Result<$ty> {
                match text {
                    $($text => Ok($variant),)+
                    other => Err(Error::Corrupt(format!(
                        "{} {other:?}",
                        stringify!($ty)
                    ))),
                }
            }
        }
    };
}

sql_enum!(Scheduling, Scheduling::Server => "server", Scheduling::Client => "client");
sql_enum!(
    CaldavStatus,
    CaldavStatus::Unconfigured => "unconfigured",
    CaldavStatus::NeedsConfirmation => "needs_confirmation",
    CaldavStatus::Ok => "ok",
    CaldavStatus::Error => "error",
);
sql_enum!(
    Discovery,
    Discovery::WellKnown => "well_known",
    Discovery::Manual => "manual",
    Discovery::Srv => "srv",
);

fn parse_opt<T>(text: Option<String>, parse: fn(&str) -> Result<T>) -> Result<Option<T>> {
    text.as_deref().map(parse).transpose()
}

/// An account's `caldav` row with its `caldav_trust.authserv_id`.
/// `username`/`secret` `None`: the IMAP login is reused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaldavConfig {
    pub url: Option<String>,
    pub principal_url: Option<String>,
    pub home_url: Option<String>,
    pub confirmed_host: Option<String>,
    /// The host discovery asked the owner to confirm.
    pub pending_host: Option<String>,
    pub allow_insecure: bool,
    /// The owner's override; `None` follows `scheduling_detected`.
    pub scheduling: Option<Scheduling>,
    pub scheduling_detected: Option<Scheduling>,
    pub username: Option<String>,
    /// Sealed with [`crate::Vault::seal_caldav`].
    pub secret: Option<Sealed>,
    pub fallback_account: Option<AccountId>,
    pub default_calendar_href: Option<String>,
    pub discovery: Option<Discovery>,
    pub status: CaldavStatus,
    pub last_error: Option<String>,
    pub authserv_id: Option<String>,
    /// The authserv-id the runner offers; read only, see
    /// [`set_authserv_suggestion`].
    pub authserv_suggestion: Option<String>,
}

impl CaldavConfig {
    pub fn scheduling(&self) -> Option<Scheduling> {
        self.scheduling.or(self.scheduling_detected)
    }
}

/// `None` when neither the account has a `caldav` row nor a trust row.
pub fn load_caldav(conn: &Connection, account: AccountId) -> Result<Option<CaldavConfig>> {
    let row = conn
        .query_row(
            "SELECT c.url, c.principal_url, c.home_url, c.confirmed_host, c.allow_insecure, \
             c.scheduling, c.scheduling_detected, c.username, c.secret_ct, c.secret_nonce, \
             c.fallback_account_id, c.default_calendar_href, c.discovery, c.status, \
             c.last_error, t.authserv_id, c.pending_host, t.suggestion \
             FROM account a LEFT JOIN caldav c ON c.account_id = a.id \
             LEFT JOIN caldav_trust t ON t.account_id = a.id \
             WHERE a.id = ?1 AND (c.account_id IS NOT NULL OR t.account_id IS NOT NULL)",
            [account.0],
            |r| {
                let secret = match (r.get::<_, Option<Vec<u8>>>(8)?, r.get(9)?) {
                    (Some(ciphertext), Some(nonce)) => Some(Sealed { ciphertext, nonce }),
                    _ => None,
                };
                Ok((
                    CaldavConfig {
                        url: r.get(0)?,
                        principal_url: r.get(1)?,
                        home_url: r.get(2)?,
                        confirmed_host: r.get(3)?,
                        allow_insecure: r.get::<_, Option<bool>>(4)?.unwrap_or(false),
                        username: r.get(7)?,
                        secret,
                        fallback_account: r.get::<_, Option<i64>>(10)?.map(AccountId),
                        default_calendar_href: r.get(11)?,
                        last_error: r.get(14)?,
                        authserv_id: r.get(15)?,
                        pending_host: r.get(16)?,
                        authserv_suggestion: r.get(17)?,
                        ..CaldavConfig::default()
                    },
                    [
                        r.get::<_, Option<String>>(5)?,
                        r.get(6)?,
                        r.get(12)?,
                        r.get(13)?,
                    ],
                ))
            },
        )
        .optional()?;
    let Some((mut config, [scheduling, detected, discovery, status])) = row else {
        return Ok(None);
    };
    config.scheduling = parse_opt(scheduling, Scheduling::parse)?;
    config.scheduling_detected = parse_opt(detected, Scheduling::parse)?;
    config.discovery = parse_opt(discovery, Discovery::parse)?;
    config.status = parse_opt(status, CaldavStatus::parse)?.unwrap_or_default();
    Ok(Some(config))
}

/// Writes the whole `caldav` and `caldav_trust` rows of `account`.
pub fn save_caldav(conn: &Connection, account: AccountId, c: &CaldavConfig) -> Result<()> {
    let (ciphertext, nonce) = match &c.secret {
        Some(s) => (Some(&s.ciphertext), Some(&s.nonce)),
        None => (None, None),
    };
    conn.execute(
        "INSERT INTO caldav (account_id, url, principal_url, home_url, confirmed_host, \
         allow_insecure, scheduling, scheduling_detected, username, secret_ct, secret_nonce, \
         fallback_account_id, default_calendar_href, discovery, status, last_error, \
         pending_host, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
         unixepoch()) \
         ON CONFLICT (account_id) DO UPDATE SET url = excluded.url, \
         principal_url = excluded.principal_url, home_url = excluded.home_url, \
         confirmed_host = excluded.confirmed_host, allow_insecure = excluded.allow_insecure, \
         scheduling = excluded.scheduling, scheduling_detected = excluded.scheduling_detected, \
         username = excluded.username, secret_ct = excluded.secret_ct, \
         secret_nonce = excluded.secret_nonce, fallback_account_id = excluded.fallback_account_id, \
         default_calendar_href = excluded.default_calendar_href, discovery = excluded.discovery, \
         status = excluded.status, last_error = excluded.last_error, \
         pending_host = excluded.pending_host, updated_at = unixepoch()",
        params![
            account.0,
            c.url,
            c.principal_url,
            c.home_url,
            c.confirmed_host,
            c.allow_insecure,
            c.scheduling.map(Scheduling::as_str),
            c.scheduling_detected.map(Scheduling::as_str),
            c.username,
            ciphertext,
            nonce,
            c.fallback_account.map(|a| a.0),
            c.default_calendar_href,
            c.discovery.map(Discovery::as_str),
            c.status.as_str(),
            c.last_error,
            c.pending_host,
        ],
    )?;
    conn.execute(
        "INSERT INTO caldav_trust (account_id, authserv_id) VALUES (?1, ?2) \
         ON CONFLICT (account_id) DO UPDATE SET authserv_id = excluded.authserv_id",
        params![account.0, c.authserv_id],
    )?;
    Ok(())
}

/// Records the authserv-id to offer the owner for `account`.
pub fn set_authserv_suggestion(
    conn: &Connection,
    account: AccountId,
    suggestion: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO caldav_trust (account_id, suggestion) \
         SELECT id, ?2 FROM account WHERE id = ?1 \
         ON CONFLICT (account_id) DO UPDATE SET suggestion = excluded.suggestion",
        params![account.0, suggestion],
    )?;
    Ok(())
}

/// What discovery found out, written over its own columns only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoveryResult {
    pub url: Option<String>,
    pub principal_url: Option<String>,
    pub home_url: Option<String>,
    pub confirmed_host: Option<String>,
    pub pending_host: Option<String>,
    pub scheduling_detected: Option<Scheduling>,
    pub discovery: Option<Discovery>,
    pub status: CaldavStatus,
    pub last_error: Option<String>,
}

/// Records `found` unless the owner changed the URL since discovery read
/// `read_url`, or the account is being deleted; returns whether it did.
pub fn record_discovery(
    conn: &Connection,
    account: AccountId,
    read_url: Option<&str>,
    found: &DiscoveryResult,
) -> Result<bool> {
    conn.execute(
        "INSERT OR IGNORE INTO caldav (account_id) \
         SELECT id FROM account WHERE id = ?1 AND status != 'deleting'",
        [account.0],
    )?;
    let updated = conn.execute(
        "UPDATE caldav SET url = ?3, principal_url = ?4, home_url = ?5, confirmed_host = ?6, \
         pending_host = ?7, scheduling_detected = ?8, discovery = ?9, status = ?10, \
         last_error = ?11, updated_at = unixepoch() \
         WHERE account_id = ?1 AND url IS ?2 \
         AND EXISTS (SELECT 1 FROM account WHERE id = ?1 AND status != 'deleting')",
        params![
            account.0,
            read_url,
            found.url,
            found.principal_url,
            found.home_url,
            found.confirmed_host,
            found.pending_host,
            found.scheduling_detected.map(Scheduling::as_str),
            found.discovery.map(Discovery::as_str),
            found.status.as_str(),
            found.last_error,
        ],
    )?;
    Ok(updated > 0)
}

/// Sets or clears the default calendar of `account`, touching nothing else.
pub fn set_default_calendar(
    conn: &Connection,
    account: AccountId,
    href: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO caldav (account_id, default_calendar_href) VALUES (?1, ?2) \
         ON CONFLICT (account_id) DO UPDATE SET \
         default_calendar_href = excluded.default_calendar_href, updated_at = unixepoch()",
        params![account.0, href],
    )?;
    Ok(())
}

/// The owner's calendar addresses for `account`, lower case: its
/// identities and its login email. The one definition of "me" for
/// invitations, replies and edits.
pub fn my_addresses(conn: &Connection, account: AccountId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT lower(address) FROM identity WHERE account_id = ?1 \
         UNION SELECT lower(email) FROM account WHERE id = ?1 AND email IS NOT NULL",
    )?;
    Ok(stmt
        .query_map([account.0], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The newest REPLY DTSTAMP applied or being applied for each
/// (lower-case attendee, RECURRENCE-ID key) of `uid`, so an older REPLY that
/// arrives later is not applied over it. Done ops go after 120 days, while
/// a REPLY is only applied within 30 days of receipt.
pub fn reply_stamps(
    conn: &Connection,
    targets: &[AccountId],
    uid: &str,
) -> Result<Vec<(String, Option<String>, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT lower(attendee), recurrence_id, max(dtstamp) FROM cal_op \
         WHERE kind = 'apply' AND uid = ?1 AND target_account_id = ?2 AND dtstamp IS NOT NULL \
         AND state != 'failed' GROUP BY lower(attendee), recurrence_id",
    )?;
    let mut out = Vec::new();
    for target in targets {
        out.extend(
            stmt.query_map(rusqlite::params![uid, target.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(out)
}

/// Active accounts with a discovered calendar home, the ones the calendar
/// runner syncs.
pub fn caldav_accounts(conn: &Connection) -> Result<Vec<AccountId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.account_id FROM caldav c JOIN account a ON a.id = c.account_id \
         WHERE c.home_url IS NOT NULL AND c.status != 'needs_confirmation' \
         AND a.status = 'active' ORDER BY c.account_id",
    )?;
    Ok(stmt
        .query_map([], |r| r.get(0).map(AccountId))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Records the outcome of a sync; an account awaiting host confirmation
/// keeps that status.
pub fn set_caldav_status(
    conn: &Connection,
    account: AccountId,
    status: CaldavStatus,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE caldav SET status = ?2, last_error = ?3, updated_at = unixepoch() \
         WHERE account_id = ?1 AND status != 'needs_confirmation'",
        params![account.0, status.as_str(), error],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarPref {
    pub href: String,
    pub color: Option<String>,
    pub visible: bool,
    pub sort: Option<i64>,
}

impl CalendarPref {
    pub fn new(href: String) -> CalendarPref {
        CalendarPref {
            href,
            color: None,
            visible: true,
            sort: None,
        }
    }
}

pub fn calendar_prefs(conn: &Connection) -> Result<HashMap<(AccountId, String), CalendarPref>> {
    let mut stmt =
        conn.prepare_cached("SELECT account_id, href, color, visible, sort FROM calendar_pref")?;
    let rows = stmt.query_map([], |r| {
        let href: String = r.get(1)?;
        Ok((
            (AccountId(r.get(0)?), href.clone()),
            CalendarPref {
                href,
                color: r.get(2)?,
                visible: r.get(3)?,
                sort: r.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Changes only the given preferences of a calendar; `Some(None)` resets
/// one.
pub fn update_calendar_pref(
    conn: &Connection,
    account: AccountId,
    href: &str,
    color: Option<Option<&str>>,
    visible: Option<bool>,
    sort: Option<Option<i64>>,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO calendar_pref (account_id, href) VALUES (?1, ?2)",
        params![account.0, href],
    )?;
    conn.execute(
        "UPDATE calendar_pref SET \
         color = CASE WHEN ?3 THEN ?4 ELSE color END, \
         visible = coalesce(?5, visible), \
         sort = CASE WHEN ?6 THEN ?7 ELSE sort END \
         WHERE account_id = ?1 AND href = ?2",
        params![
            account.0,
            href,
            color.is_some(),
            color.flatten(),
            visible,
            sort.is_some(),
            sort.flatten(),
        ],
    )?;
    Ok(())
}

/// Logs one calendar or event change in the caller's transaction.
fn emit(conn: &Connection, entity: Entity, id: i64, op: ChangeOp) -> Result<()> {
    changes::append(
        conn,
        &NewChange {
            entity,
            entity_id: id,
            op,
            payload: None,
        },
    )?;
    Ok(())
}

/// Logs the deletion of each id.
fn emit_deleted(conn: &Connection, entity: Entity, ids: &[i64]) -> Result<()> {
    ids.iter()
        .try_for_each(|id| emit(conn, entity, *id, ChangeOp::Delete))
}

/// A `cache.db` calendar collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarRow {
    pub id: i64,
    pub account: AccountId,
    pub href: String,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub writable: bool,
    /// Unix seconds of the last complete sync; `None` before the first, when
    /// "no stored event" means nothing yet.
    pub synced_at: Option<i64>,
}

/// Calendars of `account`, or of every account, by account and href.
pub fn calendars(conn: &Connection, account: Option<AccountId>) -> Result<Vec<CalendarRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, account_id, href, display_name, color_srv, ctag, sync_token, writable, \
         synced_at FROM calendar WHERE ?1 IS NULL OR account_id = ?1 ORDER BY account_id, href",
    )?;
    let rows = stmt.query_map([account.map(|a| a.0)], |r| {
        Ok(CalendarRow {
            id: r.get(0)?,
            account: AccountId(r.get(1)?),
            href: r.get(2)?,
            display_name: r.get(3)?,
            color: r.get(4)?,
            ctag: r.get(5)?,
            sync_token: r.get(6)?,
            writable: r.get(7)?,
            synced_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Inserts or renames a calendar, keeping its sync state; returns its id.
/// Only an actual change is logged.
pub fn upsert_calendar(
    conn: &Connection,
    account: AccountId,
    href: &str,
    display_name: Option<&str>,
    color: Option<&str>,
    writable: bool,
) -> Result<i64> {
    let unchanged: Option<i64> = conn
        .query_row(
            "SELECT id FROM calendar WHERE account_id = ?1 AND href = ?2 \
             AND display_name IS ?3 AND color_srv IS ?4 AND writable = ?5",
            params![account.0, href, display_name, color, writable],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = unchanged {
        return Ok(id);
    }
    let id = conn.query_row(
        "INSERT INTO calendar (account_id, href, display_name, color_srv, writable) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (account_id, href) DO UPDATE SET \
         display_name = excluded.display_name, color_srv = excluded.color_srv, \
         writable = excluded.writable RETURNING id",
        params![account.0, href, display_name, color, writable],
        |r| r.get(0),
    )?;
    emit(conn, Entity::Calendar, id, ChangeOp::Upsert)?;
    Ok(id)
}

/// Records a complete sync at `now` (Unix seconds).
pub fn set_sync_state(
    conn: &Connection,
    calendar: i64,
    ctag: Option<&str>,
    sync_token: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET ctag = ?2, sync_token = ?3, synced_at = ?4 WHERE id = ?1",
        params![calendar, ctag, sync_token, now],
    )?;
    Ok(())
}

/// Deletes the calendars of `account` that are not in `keep`, with their
/// events through the cascade.
pub fn retain_calendars(conn: &Connection, account: AccountId, keep: &[String]) -> Result<usize> {
    let mut delete = conn.prepare_cached("DELETE FROM calendar WHERE id = ?1")?;
    let mut deleted = 0;
    for calendar in calendars(conn, Some(account))? {
        if !keep.contains(&calendar.href) && delete.execute([calendar.id])? > 0 {
            emit(conn, Entity::Calendar, calendar.id, ChangeOp::Delete)?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// The cached copy of one calendar object resource.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventRow {
    pub href: String,
    pub etag: Option<String>,
    pub uid: String,
    /// The master's SEQUENCE; 0 when the resource holds only overrides.
    pub sequence: u32,
    /// The master's DTSTAMP, Unix seconds.
    pub dtstamp: Option<i64>,
    /// The iCalendar STATUS value, e.g. `CONFIRMED`.
    pub status: Option<String>,
    /// The owner's PARTSTAT value, e.g. `ACCEPTED`.
    pub my_partstat: Option<String>,
    pub organizer: Option<String>,
    pub is_recurring: bool,
    pub first_start: Option<i64>,
    pub last_end: Option<i64>,
    pub window_edge: Option<i64>,
}

/// One instance in the cached window; times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow {
    pub start: i64,
    pub end: i64,
    pub recurrence_id: Option<String>,
    pub all_day: bool,
}

/// Hrefs and etags of a calendar's cached events and skipped resources.
pub fn event_etags(conn: &Connection, calendar: i64) -> Result<HashMap<String, Option<String>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT href, etag FROM cal_event WHERE calendar_id = ?1 \
         UNION ALL SELECT href, etag FROM cal_skip WHERE calendar_id = ?1",
    )?;
    let rows = stmt.query_map([calendar], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records resources without a usable event at their etag, dropping any
/// event cached under the same href.
pub fn skip_resources(
    conn: &Connection,
    calendar: i64,
    skipped: &[(String, Option<String>)],
) -> Result<()> {
    let mut drop_event = conn.prepare_cached(
        "DELETE FROM cal_event WHERE calendar_id = ?1 AND href = ?2 RETURNING id",
    )?;
    let mut skip = conn.prepare_cached(
        "INSERT INTO cal_skip (calendar_id, href, etag) VALUES (?1, ?2, ?3) \
         ON CONFLICT (calendar_id, href) DO UPDATE SET etag = excluded.etag",
    )?;
    for (href, etag) in skipped {
        let dropped: Option<i64> = drop_event
            .query_row(params![calendar, href], |r| r.get(0))
            .optional()?;
        emit_deleted(conn, Entity::Event, dropped.as_slice())?;
        skip.execute(params![calendar, href, etag])?;
    }
    Ok(())
}

/// Moves a recurring event's window edge without touching its instances.
pub fn set_window_edge(conn: &Connection, event: i64, window_edge: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE cal_event SET window_edge = ?2 WHERE id = ?1",
        params![event, window_edge],
    )?;
    Ok(())
}

/// Stores `event` with its iCalendar text and replaces its instances;
/// returns the event's id.
pub fn put_event(
    conn: &Connection,
    calendar: i64,
    event: &EventRow,
    ics: &[u8],
    instances: &[InstanceRow],
) -> Result<i64> {
    let compressed = zstd::bulk::compress(ics, ZSTD_LEVEL)?;
    conn.execute(
        "DELETE FROM cal_skip WHERE calendar_id = ?1 AND href = ?2",
        params![calendar, event.href],
    )?;
    let id: i64 = conn.query_row(
        "INSERT INTO cal_event (calendar_id, href, etag, uid, ics, sequence, dtstamp, status, \
         my_partstat, organizer, is_recurring, first_start, last_end, window_edge) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT (calendar_id, href) DO UPDATE SET etag = excluded.etag, \
         uid = excluded.uid, ics = excluded.ics, sequence = excluded.sequence, \
         dtstamp = excluded.dtstamp, status = excluded.status, \
         my_partstat = excluded.my_partstat, organizer = excluded.organizer, \
         is_recurring = excluded.is_recurring, first_start = excluded.first_start, \
         last_end = excluded.last_end, window_edge = excluded.window_edge RETURNING id",
        params![
            calendar,
            event.href,
            event.etag,
            event.uid,
            compressed,
            event.sequence,
            event.dtstamp,
            event.status,
            event.my_partstat,
            event.organizer,
            event.is_recurring,
            event.first_start,
            event.last_end,
            event.window_edge,
        ],
        |r| r.get(0),
    )?;
    replace_instances(conn, id, instances, event.window_edge)?;
    emit(conn, Entity::Event, id, ChangeOp::Upsert)?;
    Ok(id)
}

/// Replaces an event's instances and records the window edge they reach.
pub fn replace_instances(
    conn: &Connection,
    event: i64,
    instances: &[InstanceRow],
    window_edge: Option<i64>,
) -> Result<()> {
    conn.execute("DELETE FROM cal_instance WHERE event_id = ?1", [event])?;
    conn.execute(
        "UPDATE cal_event SET window_edge = ?2 WHERE id = ?1",
        params![event, window_edge],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO cal_instance (event_id, start_utc, end_utc, recurrence_id, all_day) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for i in instances {
        insert.execute(params![event, i.start, i.end, i.recurrence_id, i.all_day])?;
    }
    Ok(())
}

/// Deletes the events and skip records of `hrefs`.
pub fn delete_events(conn: &Connection, calendar: i64, hrefs: &[String]) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM cal_event WHERE calendar_id = ?1 AND href = ?2 RETURNING id",
    )?;
    let mut unskip =
        conn.prepare_cached("DELETE FROM cal_skip WHERE calendar_id = ?1 AND href = ?2")?;
    let mut deleted = 0;
    for href in hrefs {
        let id: Option<i64> = stmt
            .query_row(params![calendar, href], |r| r.get(0))
            .optional()?;
        emit_deleted(conn, Entity::Event, id.as_slice())?;
        deleted += id.map_or(0, |_| 1);
        unskip.execute(params![calendar, href])?;
    }
    Ok(deleted)
}

/// The decompressed iCalendar text of an event.
pub fn event_ics(conn: &Connection, event: i64) -> Result<Option<Vec<u8>>> {
    let compressed: Option<Vec<u8>> = conn
        .query_row("SELECT ics FROM cal_event WHERE id = ?1", [event], |r| {
            r.get(0)
        })
        .optional()?;
    compressed
        .map(|c| zstd::stream::decode_all(c.as_slice()).map_err(Error::from))
        .transpose()
}

/// Recurring events expanded only up to before `window_end`, and their
/// calendars' accounts.
pub fn events_behind(conn: &Connection, window_end: i64) -> Result<Vec<(i64, AccountId)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, c.account_id FROM cal_event e JOIN calendar c ON c.id = e.calendar_id \
         WHERE e.window_edge IS NOT NULL AND e.window_edge < ?1",
    )?;
    let rows = stmt.query_map([window_end], |r| Ok((r.get(0)?, AccountId(r.get(1)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Drops instances that ended, and alarms that fired, before `window_start`.
pub fn trim_instances(conn: &Connection, window_start: i64) -> Result<usize> {
    conn.execute("DELETE FROM cal_alarm WHERE fire_at < ?1", [window_start])?;
    Ok(conn.execute(
        "DELETE FROM cal_instance WHERE end_utc < ?1",
        [window_start],
    )?)
}

/// A cached event found by UID, with where it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub account: AccountId,
    pub calendar_href: String,
    pub event: EventRow,
}

/// Every cached copy of `uid` in `accounts`' calendars.
pub fn events_by_uid(
    conn: &Connection,
    accounts: &[AccountId],
    uid: &str,
) -> Result<Vec<StoredEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.account_id, c.href, e.href, e.etag, e.uid, e.sequence, e.dtstamp, e.status, \
         e.my_partstat, e.organizer, e.is_recurring, e.first_start, e.last_end, e.window_edge \
         FROM cal_event e JOIN calendar c ON c.id = e.calendar_id \
         WHERE e.uid = ?1 ORDER BY c.account_id, c.href, e.href",
    )?;
    let rows = stmt.query_map([uid], |r| {
        Ok(StoredEvent {
            account: AccountId(r.get(0)?),
            calendar_href: r.get(1)?,
            event: EventRow {
                href: r.get(2)?,
                etag: r.get(3)?,
                uid: r.get(4)?,
                sequence: r.get(5)?,
                dtstamp: r.get(6)?,
                status: r.get(7)?,
                my_partstat: r.get(8)?,
                organizer: r.get(9)?,
                is_recurring: r.get(10)?,
                first_start: r.get(11)?,
                last_end: r.get(12)?,
                window_edge: r.get(13)?,
            },
        })
    })?;
    let mut found = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    found.retain(|s| accounts.contains(&s.account));
    Ok(found)
}

/// Purges the calendars of every account not in `keep`: removed,
/// disabled or no longer configured.
pub fn retain_accounts(conn: &Connection, keep: &[AccountId]) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT account_id FROM calendar")?;
    let cached: Vec<AccountId> = stmt
        .query_map([], |r| r.get(0).map(AccountId))?
        .collect::<rusqlite::Result<_>>()?;
    for account in cached.into_iter().filter(|a| !keep.contains(a)) {
        delete_account_calendars(conn, account)?;
    }
    Ok(())
}

fn delete_account_calendars(conn: &Connection, account: AccountId) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("DELETE FROM calendar WHERE account_id = ?1 RETURNING id")?;
    let ids: Vec<i64> = stmt
        .query_map([account.0], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    emit_deleted(conn, Entity::Calendar, &ids)
}

/// Deletes an account's calendars (events, instances and alarms cascade)
/// and its queued invitations.
pub fn purge_account(conn: &Connection, account: AccountId) -> Result<()> {
    delete_account_calendars(conn, account)?;
    conn.execute("DELETE FROM cal_inbox WHERE account_id = ?1", [account.0])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations;

    fn db(migrations: &[&str]) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrations::apply(&mut conn, migrations).unwrap();
        conn
    }

    fn account(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
             VALUES (?1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00')",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn caldav_config_round_trips() {
        let conn = db(migrations::CONFIG);
        account(&conn, 1);
        account(&conn, 2);
        assert_eq!(load_caldav(&conn, AccountId(1)).unwrap(), None);
        let config = CaldavConfig {
            url: Some("https://dav.example.com/".into()),
            home_url: Some("https://dav.example.com/home/".into()),
            allow_insecure: true,
            scheduling: Some(Scheduling::Client),
            scheduling_detected: Some(Scheduling::Server),
            username: Some("me".into()),
            secret: Some(Sealed {
                ciphertext: vec![1],
                nonce: vec![2],
            }),
            fallback_account: Some(AccountId(2)),
            default_calendar_href: Some("/cal/".into()),
            discovery: Some(Discovery::Manual),
            status: CaldavStatus::Ok,
            authserv_id: Some("mx.example.com".into()),
            ..CaldavConfig::default()
        };
        save_caldav(&conn, AccountId(1), &config).unwrap();
        assert_eq!(load_caldav(&conn, AccountId(1)).unwrap(), Some(config));
        assert_eq!(caldav_accounts(&conn).unwrap(), [AccountId(1)]);
    }

    fn event(href: &str, uid: &str) -> EventRow {
        EventRow {
            href: href.into(),
            etag: Some("\"1\"".into()),
            uid: uid.into(),
            sequence: 2,
            ..EventRow::default()
        }
    }

    #[test]
    fn events_are_found_by_uid_and_purged_with_their_account() {
        let conn = db(migrations::CACHE);
        let work =
            upsert_calendar(&conn, AccountId(1), "/a/work/", Some("Work"), None, true).unwrap();
        let other = upsert_calendar(&conn, AccountId(2), "/b/home/", None, None, true).unwrap();
        let instance = InstanceRow {
            start: 10,
            end: 20,
            recurrence_id: None,
            all_day: false,
        };
        let id = put_event(
            &conn,
            work,
            &event("/a/work/1.ics", "u1"),
            b"ICS",
            &[instance],
        )
        .unwrap();
        put_event(&conn, other, &event("/b/home/1.ics", "u1"), b"ICS", &[]).unwrap();
        assert_eq!(event_ics(&conn, id).unwrap().as_deref(), Some(&b"ICS"[..]));
        assert_eq!(
            upsert_calendar(&conn, AccountId(1), "/a/work/", Some("Job"), None, true).unwrap(),
            work
        );

        let found = events_by_uid(&conn, &[AccountId(1)], "u1").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].calendar_href, "/a/work/");
        assert_eq!(found[0].event.sequence, 2);
        assert_eq!(
            event_etags(&conn, work).unwrap(),
            HashMap::from([("/a/work/1.ics".to_owned(), Some("\"1\"".to_owned()))])
        );

        purge_account(&conn, AccountId(1)).unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            (count("calendar"), count("cal_event"), count("cal_instance")),
            (1, 1, 0)
        );
    }

    #[test]
    fn retain_drops_calendars_gone_from_the_server() {
        let conn = db(migrations::CACHE);
        for href in ["/a/", "/b/"] {
            upsert_calendar(&conn, AccountId(1), href, None, None, true).unwrap();
        }
        assert_eq!(
            retain_calendars(&conn, AccountId(1), &["/b/".to_owned()]).unwrap(),
            1
        );
        let left: Vec<String> = calendars(&conn, None)
            .unwrap()
            .into_iter()
            .map(|c| c.href)
            .collect();
        assert_eq!(left, ["/b/"]);
    }

    #[test]
    fn discovery_writes_only_its_columns_and_only_over_the_url_it_read() {
        let conn = db(migrations::CONFIG);
        account(&conn, 1);
        let found = DiscoveryResult {
            url: Some("https://dav.example.com/".into()),
            home_url: Some("https://dav.example.com/home/".into()),
            status: CaldavStatus::Ok,
            ..DiscoveryResult::default()
        };
        assert!(record_discovery(&conn, AccountId(1), None, &found).unwrap());
        let owner = CaldavConfig {
            url: Some("https://new.example.com/".into()),
            username: Some("me".into()),
            secret: Some(Sealed {
                ciphertext: vec![1],
                nonce: vec![2],
            }),
            default_calendar_href: Some("/home/work/".into()),
            ..load_caldav(&conn, AccountId(1)).unwrap().unwrap()
        };
        save_caldav(&conn, AccountId(1), &owner).unwrap();
        let stale = Some("https://dav.example.com/");
        assert!(!record_discovery(&conn, AccountId(1), stale, &found).unwrap());
        assert_eq!(load_caldav(&conn, AccountId(1)).unwrap().unwrap(), owner);

        let fresh = Some("https://new.example.com/");
        let moved = DiscoveryResult {
            url: Some("https://new.example.com/".into()),
            ..found.clone()
        };
        assert!(record_discovery(&conn, AccountId(1), fresh, &moved).unwrap());
        let after = load_caldav(&conn, AccountId(1)).unwrap().unwrap();
        assert_eq!(after.secret, owner.secret, "discovery never writes secrets");
        assert_eq!(after.default_calendar_href, owner.default_calendar_href);

        conn.execute("UPDATE account SET status = 'deleting'", [])
            .unwrap();
        conn.execute(
            "UPDATE caldav SET username = NULL, secret_ct = NULL, secret_nonce = NULL",
            [],
        )
        .unwrap();
        assert!(!record_discovery(&conn, AccountId(1), fresh, &moved).unwrap());
        assert_eq!(
            load_caldav(&conn, AccountId(1)).unwrap().unwrap().secret,
            None
        );
    }

    #[test]
    fn a_calendar_pref_update_keeps_the_fields_not_given() {
        let conn = db(migrations::CONFIG);
        account(&conn, 1);
        update_calendar_pref(
            &conn,
            AccountId(1),
            "/c/",
            Some(Some("#fff")),
            None,
            Some(Some(3)),
        )
        .unwrap();
        update_calendar_pref(&conn, AccountId(1), "/c/", None, Some(false), None).unwrap();
        let pref = calendar_prefs(&conn).unwrap()[&(AccountId(1), "/c/".to_owned())].clone();
        assert_eq!(
            (pref.color.as_deref(), pref.visible, pref.sort),
            (Some("#fff"), false, Some(3))
        );
        update_calendar_pref(&conn, AccountId(1), "/c/", Some(None), None, None).unwrap();
        let pref = calendar_prefs(&conn).unwrap()[&(AccountId(1), "/c/".to_owned())].clone();
        assert_eq!(pref.color, None);
    }

    #[test]
    fn skipped_resources_count_as_cached_until_they_change() {
        let conn = db(migrations::CACHE);
        let cal = upsert_calendar(&conn, AccountId(1), "/a/", None, None, true).unwrap();
        let skipped = vec![("/a/task.ics".to_owned(), Some("\"1\"".to_owned()))];
        skip_resources(&conn, cal, &skipped).unwrap();
        assert_eq!(event_etags(&conn, cal).unwrap().len(), 1);
        put_event(&conn, cal, &event("/a/task.ics", "u"), b"ICS", &[]).unwrap();
        let skips: i64 = conn
            .query_row("SELECT count(*) FROM cal_skip", [], |r| r.get(0))
            .unwrap();
        assert_eq!(skips, 0, "an event replaces the skip record");
        delete_events(&conn, cal, &["/a/task.ics".to_owned()]).unwrap();
        assert!(event_etags(&conn, cal).unwrap().is_empty());
    }
}
