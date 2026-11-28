//! AC 16 server side: `GET /api/calendar/events` lists the expanded
//! instances of a range with their summaries, in the cached window and
//! beyond it.
#![cfg(all(feature = "calendar", feature = "sqlite"))]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_api_core::{Api, Method, Request};
use tagmail_calendar::Window;
use tagmail_core::AccountId;
use tagmail_store::calendar::{self, EventRow, InstanceRow};
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey};
use tempfile::TempDir;
use time::OffsetDateTime;

const ACCOUNT: AccountId = AccountId(1);
const WORK: &str = "/cal/work/";
const HOME: &str = "/cal/home/";
const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

fn today() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    now - now.rem_euclid(DAY)
}

fn stamp(t: i64) -> String {
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
}

fn vcalendar(vevents: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\n{vevents}END:VCALENDAR\r\n")
}

fn vevent(uid: &str, start: i64, extra: &str) -> String {
    format!(
        "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:{}\r\nDTEND:{}\r\n\
         {extra}END:VEVENT\r\n",
        stamp(start),
        stamp(start + HOUR),
    )
}

/// A weekly standup from a week ago whose instance next week moved an hour
/// and was renamed, and a dentist appointment in two days.
fn resources(today: i64) -> [(&'static str, String, String); 2] {
    let first = today - 7 * DAY + 9 * HOUR;
    let moved = first + 14 * DAY;
    let standup = vcalendar(&format!(
        "{}{}",
        vevent("standup", first, "RRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n"),
        vevent(
            "standup",
            moved + HOUR,
            &format!(
                "RECURRENCE-ID:{}\r\nSUMMARY:Moved standup\r\n",
                stamp(moved)
            ),
        ),
    ));
    let dentist = vcalendar(&vevent(
        "dentist",
        today + 2 * DAY + 14 * HOUR,
        "SUMMARY:Dentist\r\nLOCATION:Main St 1\r\n",
    ));
    [
        (WORK, "standup.ics".into(), standup),
        (HOME, "dentist.ics".into(), dentist),
    ]
}

async fn api(dir: &TempDir) -> (Api<SqliteStore>, Arc<SqliteStore>) {
    let config = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    let cache = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let blobs = BlobStore::open(&dir.path().join("blobs")).unwrap();
    let store = SqliteStore::open(config, cache, blobs, &MasterKey::generate())
        .await
        .unwrap();
    let today = today();
    store
        .cache()
        .write(move |tx| {
            for (calendar_href, name, ics) in resources(today) {
                let id = calendar::upsert_calendar(tx, ACCOUNT, calendar_href, None, None, true)?;
                let cal = tagmail_calendar::parse(ics.as_bytes()).unwrap();
                let (instances, _, edge) = cal.instances(&Window::around_unix(today));
                let s = cal.summary(&[]).unwrap();
                let row = EventRow {
                    href: format!("{calendar_href}{name}"),
                    uid: s.uid,
                    is_recurring: s.is_recurring,
                    first_start: s.first_start,
                    last_end: s.last_end,
                    window_edge: edge,
                    ..EventRow::default()
                };
                let instances: Vec<InstanceRow> = instances
                    .into_iter()
                    .map(|i| InstanceRow {
                        start: i.start,
                        end: i.end,
                        recurrence_id: i.recurrence_id,
                        all_day: i.all_day,
                    })
                    .collect();
                calendar::put_event(tx, id, &row, ics.as_bytes(), &instances)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let store = Arc::new(store);
    (Api::new(Arc::clone(&store), None), store)
}

async fn get(api: &Api<SqliteStore>, query: &str) -> (u16, Value) {
    let resp = api
        .handle(Request {
            method: Method::Get,
            path: "/api/calendar/events",
            query,
            body: b"",
        })
        .await;
    (resp.status, serde_json::from_slice(&resp.body).unwrap())
}

fn titles(body: &Value) -> Vec<(i64, String)> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["start"].as_i64().unwrap(),
                e["summary"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_range_lists_instances_with_their_own_summaries() {
    let dir = TempDir::new().unwrap();
    let (api, _) = api(&dir).await;
    let today = today();
    let next_week = today + 7 * DAY;

    let (status, body) = get(&api, &format!("from={today}&to={}", today + 14 * DAY)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        titles(&body),
        [
            (today + 9 * HOUR, "Standup".to_owned()),
            (today + 2 * DAY + 14 * HOUR, "Dentist".to_owned()),
            (next_week + 10 * HOUR, "Moved standup".to_owned()),
        ]
    );
    let dentist = &body["events"][1];
    assert_eq!(dentist["location"], "Main St 1");
    assert_eq!(dentist["calendarHref"], HOME);
    assert_eq!(dentist["recurrenceId"], Value::Null);
    assert_eq!(
        body["events"][2]["recurrenceId"],
        stamp(next_week + 9 * HOUR)
    );

    let work = format!(
        "from={today}&to={}&calendars=1%3A%2Fcal%2Fwork%2F",
        today + 14 * DAY
    );
    let (_, body) = get(&api, &work).await;
    assert_eq!(titles(&body).len(), 2);
    assert!(titles(&body).iter().all(|(_, t)| t.contains("tandup")));
}

#[tokio::test]
async fn a_range_past_the_window_is_expanded_on_demand() {
    let dir = TempDir::new().unwrap();
    let (api, _) = api(&dir).await;
    let far = Window::around_unix(today()).end_ts() + 30 * DAY;
    let (status, body) = get(&api, &format!("from={far}&to={}", far + 7 * DAY)).await;
    assert_eq!(status, 200, "{body}");
    let found = titles(&body);
    assert_eq!(found.len(), 1, "{body}");
    assert_eq!(found[0].1, "Standup");
    assert_eq!((found[0].0 - (today() + 9 * HOUR)).rem_euclid(7 * DAY), 0);
}

#[tokio::test]
async fn bad_ranges_and_calendars_are_refused() {
    let dir = TempDir::new().unwrap();
    let (api, _) = api(&dir).await;
    for (query, status) in [
        ("", 400),
        ("from=1&to=x", 400),
        ("from=10&to=10", 422),
        ("from=0&to=99999999", 422),
        ("from=0&to=5356800", 200),
        ("from=0&to=5356801", 422),
        ("from=-9223372036854775808&to=9223372036854775807", 422),
        ("from=9223372036854775000&to=9223372036854775807", 422),
        ("from=253402300800&to=253402300900", 422),
        ("from=%2B5&to=10", 200),
        ("from=0&to=10&calendars=%2Fcal%2Fwork%2F", 422),
    ] {
        assert_eq!(get(&api, query).await.0, status, "{query}");
    }
}

/// A resource put as the replica's sync left it: `instances` cached up to
/// `edge`.
async fn put(store: &SqliteStore, name: &str, ics: &str, instances: Vec<InstanceRow>, edge: i64) {
    let (name, ics) = (name.to_owned(), ics.to_owned());
    store
        .cache()
        .write(move |tx| {
            let id = calendar::upsert_calendar(tx, ACCOUNT, WORK, None, None, true)?;
            let row = EventRow {
                href: format!("{WORK}{name}"),
                uid: name.clone(),
                status: Some("CONFIRMED".into()),
                is_recurring: true,
                first_start: instances.first().map(|i| i.start),
                window_edge: Some(edge),
                ..EventRow::default()
            };
            calendar::put_event(tx, id, &row, ics.as_bytes(), &instances)?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unparsable_resource_keeps_its_cached_instances() {
    let dir = TempDir::new().unwrap();
    let (api, store) = api(&dir).await;
    let at = today() + 3 * DAY;
    let cached = InstanceRow {
        start: at,
        end: at + HOUR,
        recurrence_id: None,
        all_day: false,
    };
    put(&store, "broken", "not a calendar", vec![cached], at + DAY).await;
    let (_, body) = get(&api, &format!("from={at}&to={}", at + DAY)).await;
    let broken = &body["events"][0];
    assert_eq!(broken["uid"], "broken", "{body}");
    assert_eq!(broken["start"], at);
    assert_eq!(broken["summary"], Value::Null);
    assert_eq!(broken["status"], "confirmed");
}

#[tokio::test]
async fn a_series_past_its_expanded_edge_is_expanded_on_demand() {
    let dir = TempDir::new().unwrap();
    let (api, store) = api(&dir).await;
    let first = today() + 10 * HOUR;
    let daily = vcalendar(&vevent(
        "daily",
        first,
        "RRULE:FREQ=DAILY\r\nSUMMARY:Daily\r\n",
    ));
    let only_first = InstanceRow {
        start: first,
        end: first + HOUR,
        recurrence_id: Some(stamp(first)),
        all_day: false,
    };
    put(&store, "daily", &daily, vec![only_first], first + DAY).await;
    let later = today() + 20 * DAY;
    let (_, body) = get(&api, &format!("from={later}&to={}", later + DAY)).await;
    assert_eq!(titles(&body), [(later + 10 * HOUR, "Daily".to_owned())]);
}
