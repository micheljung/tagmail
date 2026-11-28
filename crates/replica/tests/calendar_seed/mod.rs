//! Calendar rows on the server as its CalDAV sync writes them, from real
//! provider exports (Google, Outlook, Apple: time zones across DST,
//! EXDATE, overrides, all-day), each logged in the change log.

#![allow(dead_code)]

use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_calendar::{Calendar, Window};
use tagmail_core::AccountId;
use tagmail_store::calendar::{self, EventRow, InstanceRow};
use time::OffsetDateTime;

pub const DAY: i64 = 86_400;
/// The year the fixtures are dated in.
const FIXTURE_YEAR: i32 = 2026;
/// The attendee the fixtures invite.
const ME: &str = "bob@example.org";
const FIXTURES: [&str; 7] = [
    include_str!("../../../calendar/tests/fixtures/google_recurring_override.ics"),
    include_str!("../../../calendar/tests/fixtures/google_request.ics"),
    include_str!("../../../calendar/tests/fixtures/google_cancel_instance.ics"),
    include_str!("../../../calendar/tests/fixtures/outlook_custom_tz.ics"),
    include_str!("../../../calendar/tests/fixtures/outlook_teams.ics"),
    include_str!("../../../calendar/tests/fixtures/apple_all_day.ics"),
    include_str!("../../../calendar/tests/fixtures/apple_recurring.ics"),
];
/// Year shifts that spread the copies over the window around today.
const YEAR_SHIFTS: [i32; 3] = [-1, 0, 1];

/// A single event at `start` for one hour, in UTC.
pub fn ics(uid: &str, start: i64, rule: Option<&str>) -> String {
    let stamp = |t: i64| {
        let d = OffsetDateTime::from_unix_timestamp(t).unwrap();
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            d.year(),
            u8::from(d.month()),
            d.day(),
            d.hour(),
            d.minute(),
            d.second()
        )
    };
    let rule = rule.map(|r| format!("RRULE:{r}\r\n")).unwrap_or_default();
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:{}\r\nDTEND:{}\r\n{rule}\
         SUMMARY:Event {uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp(start),
        stamp(start + 3_600),
    )
}

/// Fixture `i` with its own UIDs, moved by whole years so that it lands in
/// the window around `now`.
fn copy(i: usize, now: i64) -> String {
    let year = OffsetDateTime::from_unix_timestamp(now).unwrap().year();
    let to = year + YEAR_SHIFTS[i % YEAR_SHIFTS.len()];
    let text = FIXTURES[(i / YEAR_SHIFTS.len()) % FIXTURES.len()];
    let mut out = text.replace("\nUID:", &format!("\nUID:copy{i}-"));
    for sep in [":", ",", "="] {
        out = out.replace(&format!("{sep}{FIXTURE_YEAR}"), &format!("{sep}{to}"));
    }
    out
}

/// The cache rows of one resource, derived as the server's sync does.
pub fn parsed(href: &str, ics: &str, now: i64) -> (EventRow, Vec<InstanceRow>) {
    let cal: Calendar = tagmail_calendar::parse(ics.as_bytes()).unwrap();
    let (instances, _, edge) = cal.instances(&Window::around_unix(now));
    let s = cal.summary(&[ME.to_owned()]).unwrap();
    let row = EventRow {
        href: href.to_owned(),
        etag: Some("\"1\"".into()),
        uid: s.uid,
        sequence: s.sequence,
        dtstamp: s.dtstamp,
        status: s.status,
        my_partstat: s.my_partstat,
        organizer: s.organizer,
        is_recurring: s.is_recurring,
        first_start: s.first_start,
        last_end: s.last_end,
        window_edge: edge,
    };
    let instances = instances
        .into_iter()
        .map(|i| InstanceRow {
            start: i.start,
            end: i.end,
            recurrence_id: i.recurrence_id,
            all_day: i.all_day,
        })
        .collect();
    (row, instances)
}

/// `calendars` calendars of `account` with `events` fixture copies each;
/// the first is the account's default.
pub async fn seed(store: &SqliteStore, account: i64, calendars: &[&str], events: usize, now: i64) {
    let hrefs: Vec<String> = calendars.iter().map(|c| (*c).to_owned()).collect();
    let default = hrefs[0].clone();
    store
        .config()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO caldav (account_id, default_calendar_href, scheduling_detected) \
                 VALUES (?1, ?2, 'client') ON CONFLICT (account_id) DO UPDATE SET \
                 default_calendar_href = excluded.default_calendar_href",
                tagmail_store::rusqlite::params![account, default],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (c, href) in hrefs.into_iter().enumerate() {
        let rows: Vec<(String, String)> = (0..events)
            .map(|i| (format!("{href}{i}.ics"), copy(c * events + i, now)))
            .collect();
        store
            .cache()
            .write(move |tx| {
                let id = calendar::upsert_calendar(
                    tx,
                    AccountId(account),
                    &href,
                    Some("Cal"),
                    None,
                    true,
                )?;
                for (event_href, text) in &rows {
                    let (row, instances) = parsed(event_href, text, now);
                    calendar::put_event(tx, id, &row, text.as_bytes(), &instances)?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }
}

/// Counts of `calendar`, `cal_event` and `cal_instance` rows.
pub async fn counts(store: &SqliteStore) -> [i64; 3] {
    store
        .cache()
        .read(|c| {
            let n = |t: &str| -> tagmail_store::Result<i64> {
                Ok(c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?)
            };
            Ok([n("calendar")?, n("cal_event")?, n("cal_instance")?])
        })
        .await
        .unwrap()
}
