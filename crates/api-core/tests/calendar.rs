//! AC 3 server side: `/invitation` over the server's databases reports the
//! stored copy of the event and whether the invitation is older than it.
#![cfg(all(feature = "calendar", feature = "sqlite"))]

use std::sync::Arc;

use serde_json::{Value, json};
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_api_core::{Api, Method, Request};
use tagmail_core::{AccountId, EmailId};
use tagmail_store::calendar::{self, CaldavConfig, EventRow, Scheduling};
use tagmail_store::{BlobKey, BlobStore, CacheDb, ConfigDb, DEFAULT_RAW_CAP, MasterKey};
use tempfile::TempDir;

const ACCOUNT: AccountId = AccountId(1);
const WORK: &str = "/cal/work/";
const UID: &str = "u1";
const STORED_SEQUENCE: u32 = 2;
const STORED_DTSTAMP: &str = "20261101T120000Z";
const OLDER_DTSTAMP: &str = "20261020T120000Z";
const NEWER_DTSTAMP: &str = "20261110T120000Z";
/// 2026-11-01 12:00:00 UTC.
const STORED_DTSTAMP_UNIX: i64 = 1_793_534_400;

fn vevent(sequence: u32, dtstamp: &str, recurrence_id: Option<&str>) -> String {
    let rid = recurrence_id
        .map(|r| format!("RECURRENCE-ID:{r}\r\n"))
        .unwrap_or_default();
    let rule = if recurrence_id.is_none() {
        "RRULE:FREQ=WEEKLY;COUNT=4\r\n"
    } else {
        ""
    };
    format!(
        "BEGIN:VEVENT\r\nUID:{UID}\r\n{rid}DTSTAMP:{dtstamp}\r\nSEQUENCE:{sequence}\r\n\
         DTSTART:20261201T090000Z\r\nDTEND:20261201T100000Z\r\n{rule}SUMMARY:Review\r\n\
         ORGANIZER:mailto:boss@example.com\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\n"
    )
}

fn mail(vevents: &[String]) -> String {
    format!(
        "From: <boss@example.com>\r\nSubject: Invitation\r\n\
         Content-Type: text/calendar; method=REQUEST\r\n\r\n\
         BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n{}END:VCALENDAR\r\n",
        vevents.concat()
    )
}

async fn store(dir: &TempDir, mails: &[(i64, String)]) -> SqliteStore {
    let config = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    let cache = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let blobs = BlobStore::open(&dir.path().join("blobs")).unwrap();
    let store = SqliteStore::open(config, cache, blobs, &MasterKey::generate())
        .await
        .unwrap();
    store
        .config()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                 VALUES (1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00')",
            )?;
            calendar::save_caldav(
                tx,
                ACCOUNT,
                &CaldavConfig {
                    default_calendar_href: Some(WORK.into()),
                    scheduling_detected: Some(Scheduling::Client),
                    ..CaldavConfig::default()
                },
            )
        })
        .await
        .unwrap();
    let ids: Vec<i64> = mails.iter().map(|(id, _)| *id).collect();
    store
        .cache()
        .write(move |tx| {
            tx.execute_batch(
                "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count) \
                 VALUES (1, 1, 'invitation', 1, 1)",
            )?;
            for id in &ids {
                tx.execute(
                    "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, \
                     has_calendar) VALUES (?1, 1, ?2, 1, 1, 'Invitation', 1)",
                    tagmail_store::rusqlite::params![id, format!("g:{id}")],
                )?;
            }
            let calendar_id = calendar::upsert_calendar(tx, ACCOUNT, WORK, None, None, true)?;
            let stored = EventRow {
                href: format!("{WORK}{UID}.ics"),
                uid: UID.into(),
                sequence: STORED_SEQUENCE,
                dtstamp: Some(STORED_DTSTAMP_UNIX),
                status: Some("CONFIRMED".into()),
                my_partstat: Some("ACCEPTED".into()),
                ..EventRow::default()
            };
            calendar::put_event(tx, calendar_id, &stored, b"ICS", &[])?;
            Ok(())
        })
        .await
        .unwrap();
    let blobs = store.blobs().clone();
    let mails = mails.to_vec();
    tokio::task::spawn_blocking(move || {
        for (id, raw) in mails {
            let key = BlobKey::for_message(ACCOUNT, &EmailId::parse(&format!("g:{id}")).unwrap());
            let mut writer = blobs.raw_writer(key, DEFAULT_RAW_CAP).unwrap();
            writer.write_chunk(raw.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
    })
    .await
    .unwrap();
    store
}

async fn card(api: &Api<SqliteStore>, id: i64) -> Value {
    let path = format!("/api/messages/{id}/invitation");
    let resp = api
        .handle(Request {
            method: Method::Get,
            path: &path,
            query: "",
            body: b"",
        })
        .await;
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

#[tokio::test]
async fn invitation_reports_stored_partstat_and_outdated_sequence() {
    let rid = Some("20261208T090000Z");
    let cases = [
        (
            10,
            vec![
                vevent(STORED_SEQUENCE, STORED_DTSTAMP, None),
                vevent(0, STORED_DTSTAMP, rid),
            ],
        ),
        (11, vec![vevent(STORED_SEQUENCE - 1, NEWER_DTSTAMP, None)]),
        (12, vec![vevent(STORED_SEQUENCE, OLDER_DTSTAMP, None)]),
        (13, vec![vevent(STORED_SEQUENCE, NEWER_DTSTAMP, None)]),
        (14, vec![vevent(0, OLDER_DTSTAMP, rid)]),
    ];
    let mails: Vec<(i64, String)> = cases.iter().map(|(id, v)| (*id, mail(v))).collect();
    let dir = TempDir::new().unwrap();
    let api = Api::new(Arc::new(store(&dir, &mails).await), None);

    let current = card(&api, 10).await;
    assert_eq!(
        current["stored"],
        json!({
            "accountId": ACCOUNT.0,
            "calendarHref": WORK,
            "partstat": "accepted",
            "sequence": STORED_SEQUENCE,
            "status": "confirmed",
        })
    );
    assert_eq!(
        current["target"],
        json!({ "accountId": ACCOUNT.0, "calendarHref": WORK })
    );
    assert_eq!(current["scheduling"], "client");
    assert_eq!(
        current["outdated"], false,
        "an override's lower SEQUENCE does not count"
    );

    for (id, outdated, why) in [
        (11, true, "lower SEQUENCE"),
        (12, true, "same SEQUENCE, older DTSTAMP"),
        (13, false, "same SEQUENCE, newer DTSTAMP"),
        (14, false, "overrides only"),
    ] {
        assert_eq!(card(&api, id).await["outdated"], outdated, "{why}");
    }
}
