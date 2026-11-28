//! Calendar records in the replica (appointments plan Decision e, AC 14):
//! every cached event of the server, with instances expanded locally over
//! the same window the server keeps.

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use tagmail_api_core::replica_dto::{CaldavRecord, CalendarRecord, EventRecord};
use tagmail_store::{Error, Result};

/// The account's calendar settings, without URL or credentials.
pub fn put_caldav(conn: &Connection, r: &CaldavRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO caldav (account_id, scheduling, scheduling_detected, fallback_account_id, \
         default_calendar_href) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (account_id) DO UPDATE \
         SET scheduling = excluded.scheduling, scheduling_detected = excluded.scheduling_detected, \
         fallback_account_id = excluded.fallback_account_id, \
         default_calendar_href = excluded.default_calendar_href",
        params![
            r.account_id,
            r.scheduling,
            r.scheduling_detected,
            r.fallback_account_id,
            r.default_calendar_href
        ],
    )?;
    Ok(())
}

/// The owner's preferences of a calendar, kept in `config.db`.
pub fn put_calendar_pref(conn: &Connection, r: &CalendarRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO calendar_pref (account_id, href, color, visible, sort) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (account_id, href) DO UPDATE SET \
         color = excluded.color, visible = excluded.visible, sort = excluded.sort",
        params![r.account_id, r.href, r.color, r.visible, r.sort],
    )?;
    Ok(())
}

pub fn put_calendar(conn: &Connection, r: &CalendarRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO calendar (id, account_id, href, display_name, color_srv, writable) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (id) DO UPDATE SET \
         account_id = excluded.account_id, href = excluded.href, \
         display_name = excluded.display_name, color_srv = excluded.color_srv, \
         writable = excluded.writable",
        params![
            r.id,
            r.account_id,
            r.href,
            r.display_name,
            r.color_srv,
            r.writable
        ],
    )?;
    Ok(())
}

/// Stores the event and expands it over the window around `now`. An event
/// of a calendar the replica does not have is skipped.
pub fn put_event(conn: &Connection, r: &EventRecord, now: i64) -> Result<()> {
    let known = conn
        .query_row(
            "SELECT 1 FROM calendar WHERE id = ?1",
            [r.calendar_id],
            |_| Ok(()),
        )
        .optional()?;
    if known.is_none() {
        return Ok(());
    }
    let ics = base64::engine::general_purpose::STANDARD
        .decode(&r.ics_zstd)
        .map_err(|e| Error::Corrupt(format!("event {} ics: {e}", r.id)))?;
    conn.execute(
        "INSERT INTO cal_event (id, calendar_id, href, etag, uid, ics, sequence, dtstamp, status, \
         my_partstat, organizer, is_recurring, first_start, last_end) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT (id) DO UPDATE SET calendar_id = excluded.calendar_id, \
         href = excluded.href, etag = excluded.etag, uid = excluded.uid, ics = excluded.ics, \
         sequence = excluded.sequence, dtstamp = excluded.dtstamp, status = excluded.status, \
         my_partstat = excluded.my_partstat, organizer = excluded.organizer, \
         is_recurring = excluded.is_recurring, first_start = excluded.first_start, \
         last_end = excluded.last_end",
        params![
            r.id,
            r.calendar_id,
            r.href,
            r.etag,
            r.uid,
            ics,
            r.sequence,
            r.dtstamp,
            r.status,
            r.my_partstat,
            r.organizer,
            r.is_recurring,
            r.first_start,
            r.last_end
        ],
    )?;
    expand_event(conn, r.id, now)
}

pub fn delete_calendar(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM cal_event WHERE calendar_id = ?1", [id])?;
    conn.execute("DELETE FROM calendar WHERE id = ?1", [id])?;
    Ok(())
}

pub fn delete_event(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM cal_instance WHERE event_id = ?1", [id])?;
    conn.execute("DELETE FROM cal_alarm WHERE event_id = ?1", [id])?;
    conn.execute("DELETE FROM cal_event WHERE id = ?1", [id])?;
    Ok(())
}

/// Drops every calendar record, for a server that no longer offers them.
pub fn purge_cache(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM cal_instance; DELETE FROM cal_alarm; DELETE FROM cal_event; \
         DELETE FROM calendar;",
    )?;
    Ok(())
}

pub fn purge_config(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM calendar_pref; DELETE FROM caldav;")?;
    Ok(())
}

/// Recurring events expanded only up to before the window around `now`,
/// re-expanded, and instances before it dropped. Returns how many moved.
#[cfg(feature = "calendar")]
pub fn slide(conn: &Connection, now: i64) -> Result<usize> {
    let window = tagmail_calendar::Window::around_unix(now);
    let behind: Vec<i64> = tagmail_store::calendar::events_behind(conn, window.end_ts())?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for id in &behind {
        expand_event(conn, *id, now)?;
    }
    tagmail_store::calendar::trim_instances(conn, window.start_ts())?;
    Ok(behind.len())
}

#[cfg(not(feature = "calendar"))]
pub fn slide(conn: &Connection, now: i64) -> Result<usize> {
    let _ = (conn, now);
    Ok(0)
}

/// Expands the event over the window around `now` as the server does
/// (`tagmail_calendar::Calendar::instances`).
#[cfg(feature = "calendar")]
fn expand_event(conn: &Connection, id: i64, now: i64) -> Result<()> {
    use tagmail_store::calendar::InstanceRow;

    let window = tagmail_calendar::Window::around_unix(now);
    let Some(ics) = tagmail_store::calendar::event_ics(conn, id)? else {
        return Ok(());
    };
    let cal = match tagmail_calendar::parse(&ics) {
        Ok(cal) => cal,
        Err(e) => {
            tracing::debug!(event = id, error = %e, "replica event does not parse");
            return tagmail_store::calendar::replace_instances(
                conn,
                id,
                &[],
                Some(window.end_ts()),
            );
        }
    };
    let (instances, _, edge) = cal.instances(&window);
    let rows: Vec<InstanceRow> = instances
        .into_iter()
        .map(|i| InstanceRow {
            start: i.start,
            end: i.end,
            recurrence_id: i.recurrence_id,
            all_day: i.all_day,
        })
        .collect();
    tagmail_store::calendar::replace_instances(conn, id, &rows, edge)
}

#[cfg(not(feature = "calendar"))]
fn expand_event(conn: &Connection, id: i64, now: i64) -> Result<()> {
    let _ = (conn, id, now);
    Ok(())
}
