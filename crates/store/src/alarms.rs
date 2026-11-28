//! Reminders (appointments plan §3.7): `cal_alarm` in `cache.db`, derived
//! from CalDAV data at sync, and what was sent or dismissed in `config.db`.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tagmail_core::AccountId;

use crate::error::Result;

const KEY_HEX: usize = 32;

/// One alarm of an event's instance; times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmRow {
    pub recurrence_id: Option<String>,
    /// The VALARM's UID, else its index and TRIGGER.
    pub valarm: String,
    pub fire_at: i64,
    pub start: i64,
    pub title: Option<String>,
}

/// A materialised alarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueAlarm {
    pub key: String,
    pub account: AccountId,
    pub fire_at: i64,
    pub start: i64,
    pub title: Option<String>,
}

/// Content-derived, so it survives a cache wipe.
pub fn alarm_key(
    account: AccountId,
    calendar_href: &str,
    uid: &str,
    recurrence_id: Option<&str>,
    valarm: &str,
    fire_at: i64,
) -> String {
    let mut hash = Sha256::new();
    for part in [
        &account.0.to_string(),
        calendar_href,
        uid,
        recurrence_id.unwrap_or("master"),
        valarm,
        &fire_at.to_string(),
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())[..KEY_HEX].to_owned()
}

/// Replaces the alarms of a `cache.db` event.
pub fn replace_alarms(conn: &Connection, event: i64, alarms: &[AlarmRow]) -> Result<()> {
    conn.execute("DELETE FROM cal_alarm WHERE event_id = ?1", [event])?;
    let Some((account, href, uid)) = conn
        .query_row(
            "SELECT c.account_id, c.href, e.uid FROM cal_event e \
             JOIN calendar c ON c.id = e.calendar_id WHERE e.id = ?1",
            [event],
            |r| {
                Ok((
                    AccountId(r.get(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(());
    };
    let mut insert = conn.prepare_cached(
        "INSERT INTO cal_alarm (event_id, alarm_key, recurrence_id, fire_at, start_utc, title) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for a in alarms {
        let key = alarm_key(
            account,
            &href,
            &uid,
            a.recurrence_id.as_deref(),
            &a.valarm,
            a.fire_at,
        );
        insert.execute(params![
            event,
            key,
            a.recurrence_id,
            a.fire_at,
            a.start,
            a.title
        ])?;
    }
    Ok(())
}

/// The earliest fire time after `after`.
pub fn next_alarm(conn: &Connection, after: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT min(fire_at) FROM cal_alarm WHERE fire_at > ?1",
        [after],
        |r| r.get(0),
    )?)
}

fn due(r: &rusqlite::Row<'_>) -> rusqlite::Result<DueAlarm> {
    Ok(DueAlarm {
        key: r.get(0)?,
        account: AccountId(r.get(1)?),
        fire_at: r.get(2)?,
        start: r.get(3)?,
        title: r.get(4)?,
    })
}

const DUE_SELECT: &str = "SELECT DISTINCT a.alarm_key, c.account_id, a.fire_at, a.start_utc, \
                          a.title FROM cal_alarm a JOIN cal_event e ON e.id = a.event_id \
                          JOIN calendar c ON c.id = e.calendar_id";

/// Alarms firing in `from..=to`.
pub fn due_alarms(conn: &Connection, from: i64, to: i64) -> Result<Vec<DueAlarm>> {
    let mut stmt = conn.prepare_cached(&format!(
        "{DUE_SELECT} WHERE a.fire_at BETWEEN ?1 AND ?2 ORDER BY a.fire_at"
    ))?;
    let rows = stmt.query_map([from, to], due)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn alarm_by_key(conn: &Connection, key: &str) -> Result<Option<DueAlarm>> {
    Ok(conn
        .query_row(&format!("{DUE_SELECT} WHERE a.alarm_key = ?1"), [key], due)
        .optional()?)
}

/// `config.db`: the reminder targets of `account`, `session:<hash hex>`
/// for push subscriptions and `device:<id>` for paired devices.
/// None for an account that is gone, whose cached alarms wait for the
/// cache purge.
pub fn targets(conn: &Connection, account: AccountId) -> Result<Vec<String>> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM account WHERE id = ?1")?
        .exists([account.0])?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT 'session:' || lower(hex(session_hash)) FROM push_subscription \
         WHERE reminders = 1 AND (account_ids IS NULL \
         OR EXISTS (SELECT 1 FROM json_each(account_ids) WHERE value = ?1)) \
         UNION ALL SELECT 'device:' || id FROM device \
         WHERE reminders = 1 AND revoked_at IS NULL",
    )?;
    let rows = stmt.query_map([account.0], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn acked(conn: &Connection, key: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM alarm_ack WHERE alarm_key = ?1")?
        .exists([key])?)
}

/// Records the alarm as sent to `target`; false if it already was.
pub fn mark_sent(conn: &Connection, alarm: &DueAlarm, target: &str, now: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "INSERT OR IGNORE INTO alarm_sent (alarm_key, target, account_id, fire_at, sent_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            alarm.key,
            target,
            alarm.account.0,
            alarm.fire_at,
            now
        ])?
        == 1)
}

/// Dismisses the alarm everywhere; false if it already was.
pub fn ack(conn: &Connection, alarm: &DueAlarm, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO alarm_ack (alarm_key, account_id, fire_at, acked_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![alarm.key, alarm.account.0, alarm.fire_at, now],
    )? == 1)
}

/// Drops sent and dismissed records of alarms that fired before `before`.
pub fn prune(conn: &Connection, before: i64) -> Result<usize> {
    Ok(
        conn.execute("DELETE FROM alarm_sent WHERE fire_at < ?1", [before])?
            + conn.execute("DELETE FROM alarm_ack WHERE fire_at < ?1", [before])?,
    )
}

const OWNER_ZONE_KEY: &str = "owner_time_zone";

/// `config.db`: the owner's IANA time zone, which floating and all-day
/// events start in for reminders; `None` until a client set it.
pub fn owner_zone(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [OWNER_ZONE_KEY],
            |r| r.get(0),
        )
        .optional()?)
}

/// False if `zone` already was the owner's time zone.
pub fn set_owner_zone(conn: &Connection, zone: &str) -> Result<bool> {
    if owner_zone(conn)?.as_deref() == Some(zone) {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [OWNER_ZONE_KEY, zone],
    )?;
    Ok(true)
}

/// `cache.db`: every cached event, whose alarms depend on the owner's
/// time zone. The ICS is compressed, so which have a VALARM is found when
/// reading it.
pub fn event_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM cal_event ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
