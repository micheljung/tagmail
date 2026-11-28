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

/// Caches `ics` at `href` of its calendar as the sync derives it.
async fn add(store: &SqliteStore, calendar_href: &'static str, name: &str, ics: &str) {
    let (name, ics) = (name.to_owned(), ics.to_owned());
    let today = today();
    store
        .cache()
        .write(move |tx| {
            let id = calendar::upsert_calendar(tx, ACCOUNT, calendar_href, None, None, true)?;
            let cal = tagmail_calendar::parse(ics.as_bytes()).unwrap();
            let (instances, _, edge) = cal.instances(&Window::around_unix(today));
            let s = cal.summary(&[]).unwrap();
            let row = EventRow {
                href: format!("{calendar_href}{name}"),
                etag: Some("\"e1\"".into()),
                uid: s.uid,
                status: s.status,
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
            Ok(())
        })
        .await
        .unwrap();
}

fn date(t: i64) -> String {
    stamp(t)[..8].to_owned()
}

#[tokio::test]
async fn exdates_cancelled_all_day_and_floating_events_are_listed_as_stored() {
    let dir = TempDir::new().unwrap();
    let (api, store) = api(&dir).await;
    let today = today();
    let first = today + 30 * DAY + 8 * HOUR;
    let skipped = vcalendar(&vevent(
        "skipped",
        first,
        &format!(
            "RRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:{}\r\nSUMMARY:Skipped\r\n",
            stamp(first + DAY)
        ),
    ));
    add(&store, WORK, "skipped.ics", &skipped).await;
    let off = vcalendar(&vevent(
        "off",
        first + 2 * HOUR,
        "STATUS:CANCELLED\r\nSUMMARY:Off\r\n",
    ));
    add(&store, WORK, "off.ics", &off).await;
    let holiday = vcalendar(&format!(
        "BEGIN:VEVENT\r\nUID:holiday\r\nDTSTAMP:20261101T000000Z\r\n\
         DTSTART;VALUE=DATE:{}\r\nDTEND;VALUE=DATE:{}\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\n",
        date(today + 31 * DAY),
        date(today + 33 * DAY)
    ));
    add(&store, HOME, "holiday.ics", &holiday).await;
    let floating = vcalendar(&format!(
        "BEGIN:VEVENT\r\nUID:wall\r\nDTSTAMP:20261101T000000Z\r\n\
         DTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Wall clock\r\nEND:VEVENT\r\n",
        stamp(first + 3 * HOUR).trim_end_matches('Z'),
        stamp(first + 4 * HOUR).trim_end_matches('Z'),
    ));
    add(&store, HOME, "wall.ics", &floating).await;

    let (status, body) = get(
        &api,
        &format!("from={}&to={}", today + 30 * DAY, today + 34 * DAY),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let of = |uid: &str| -> Vec<&Value> {
        body["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["uid"] == uid)
            .collect()
    };
    let starts: Vec<i64> = of("skipped")
        .iter()
        .map(|e| e["start"].as_i64().unwrap())
        .collect();
    assert_eq!(starts, [first, first + 2 * DAY], "the EXDATE is left out");
    assert_eq!(of("off")[0]["status"], "cancelled");
    let holiday = of("holiday");
    assert_eq!(holiday.len(), 1);
    assert_eq!(holiday[0]["allDay"], true);
    assert_eq!(holiday[0]["start"], today + 31 * DAY);
    assert_eq!(holiday[0]["end"], today + 33 * DAY);
    assert_eq!(holiday[0]["floating"], false);
    let wall = of("wall");
    assert_eq!(wall[0]["floating"], true);
    assert_eq!(wall[0]["start"], first + 3 * HOUR, "wall clock read as UTC");
    assert_eq!(of("skipped")[0]["floating"], false);
}

#[tokio::test]
async fn an_event_opens_with_its_editable_fields() {
    let dir = TempDir::new().unwrap();
    let (api, store) = api(&dir).await;
    let ics = vcalendar(
        "BEGIN:VEVENT\r\nUID:lunch\r\nDTSTAMP:20261101T000000Z\r\nSEQUENCE:2\r\n\
         DTSTART;TZID=Europe/Zurich:20261201T120000\r\nDURATION:PT90M\r\nSUMMARY:Lunch\r\n\
         ORGANIZER;CN=Me:mailto:me@example.org\r\n\
         ATTENDEE;CN=Ann;PARTSTAT=ACCEPTED:mailto:ann@example.com\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Lunch\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\n\
         END:VEVENT\r\n",
    );
    add(&store, WORK, "lunch.ics", &ics).await;
    let open = |path: String| {
        let api = &api;
        async move {
            let resp = api
                .handle(Request {
                    method: Method::Get,
                    path: &path,
                    query: "",
                    body: b"",
                })
                .await;
            (
                resp.status,
                serde_json::from_slice::<Value>(&resp.body).unwrap(),
            )
        }
    };
    let (status, detail) = open("/api/calendar/events/1/%2Fcal%2Fwork%2Flunch.ics".into()).await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["etag"], "\"e1\"");
    assert_eq!(detail["sequence"], 2);
    assert_eq!(detail["organizer"]["email"], "me@example.org");
    assert_eq!(
        detail["organizedByMe"], false,
        "no identity of mine has that address"
    );
    assert_eq!(detail["fixedTimes"], false);
    let fields = &detail["fields"];
    assert_eq!(fields["start"], "2026-12-01T12:00:00");
    assert_eq!(fields["end"], "2026-12-01T13:30:00");
    assert_eq!(fields["tzid"], "Europe/Zurich");
    assert_eq!(fields["allDay"], false);
    assert_eq!(fields["alarms"], serde_json::json!([10]));
    assert_eq!(fields["attendees"][0]["partstat"], "accepted");
    let (status, _) = open("/api/calendar/events/1/%2Fcal%2Fwork%2Fnone.ics".into()).await;
    assert_eq!(status, 404);
}
