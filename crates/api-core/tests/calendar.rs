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
                     has_calendar, received) VALUES (?1, 1, ?2, 1, 1, 'Invitation', 1, unixepoch())",
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
                organizer: Some("boss@example.com".into()),
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
            "replyState": null,
            "revertPartstat": null,
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

const TRUSTED: &str = "mx.example.com";

fn authenticated(results: &str, vevents: &[String]) -> String {
    format!("Authentication-Results: {results}\r\n{}", mail(vevents))
}

async fn trust(store: &SqliteStore, authserv_id: Option<&str>, default: Option<&str>) {
    let (authserv_id, default) = (authserv_id.map(str::to_owned), default.map(str::to_owned));
    store
        .config()
        .write(move |tx| {
            let mut c = calendar::load_caldav(tx, ACCOUNT)?.unwrap();
            c.authserv_id = authserv_id;
            c.default_calendar_href = default;
            calendar::save_caldav(tx, ACCOUNT, &c)
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn default_trust_is_manual() {
    let newer = vec![vevent(STORED_SEQUENCE + 1, NEWER_DTSTAMP, None)];
    let mails = [
        (
            20,
            authenticated(
                "mx.example.com; dkim=pass header.d=example.com; dmarc=pass header.from=example.com",
                &newer,
            ),
        ),
        (
            21,
            authenticated("mx.example.com; dmarc=fail header.from=example.com", &newer),
        ),
        (
            22,
            authenticated("evil.example; dmarc=pass header.from=example.com", &newer),
        ),
    ];
    let dir = TempDir::new().unwrap();
    let store = Arc::new(store(&dir, &mails).await);
    let api = Api::new(store.clone(), None);

    let untrusted = card(&api, 20).await;
    assert_eq!(untrusted["senderAuth"], "untrusted_config");
    assert_eq!(untrusted["autoApply"], "manual");

    trust(&store, Some(TRUSTED), Some(WORK)).await;
    let verified = card(&api, 20).await;
    assert_eq!(verified["senderAuth"], "verified");
    assert_eq!(verified["autoApply"], "pending");
    for id in [21, 22] {
        let card = card(&api, id).await;
        assert_eq!(card["senderAuth"], "unauthenticated", "{id}");
        assert_eq!(card["autoApply"], "manual", "{id}");
    }

    trust(&store, Some(TRUSTED), Some("/cal/gone/")).await;
    let gone = card(&api, 20).await;
    assert_eq!(
        gone["target"],
        Value::Null,
        "an uncached default is no calendar"
    );
    assert_eq!(gone["autoApply"], "manual");
}

#[tokio::test]
async fn organizer_change_and_cancelled_reply_are_reported() {
    let newer = vec![vevent(STORED_SEQUENCE + 1, NEWER_DTSTAMP, None)];
    let takeover: Vec<String> = newer
        .iter()
        .map(|v| {
            v.replace(
                "ORGANIZER:mailto:boss@example.com",
                "ORGANIZER:mailto:thief@example.com",
            )
        })
        .collect();
    let mails = [
        (
            30,
            authenticated("mx.example.com; dmarc=pass header.from=example.com", &newer),
        ),
        (
            31,
            authenticated(
                "mx.example.com; dmarc=pass header.from=example.com",
                &takeover,
            )
            .replace("From: <boss@example.com>", "From: <thief@example.com>"),
        ),
    ];
    let dir = TempDir::new().unwrap();
    let store = Arc::new(store(&dir, &mails).await);
    trust(&store, Some(TRUSTED), Some(WORK)).await;
    store
        .config()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO cal_op (op_id, account_id, target_account_id, uid, kind, partstat, \
                 reply_op_id, state, reply_cancelled, prior_partstat) \
                 VALUES ('r1', 1, 1, 'u1', 'rsvp', 'ACCEPTED', 'r1#reply', 'done', 1, \
                 'NEEDS-ACTION')",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let api = Api::new(store.clone(), None);

    let same = card(&api, 30).await;
    assert_eq!(same["organizerChanged"], false);
    assert_eq!(same["autoApply"], "pending");
    assert_eq!(same["stored"]["replyState"], "cancelled");
    assert_eq!(same["stored"]["revertPartstat"], "needs_action");
    let takeover = card(&api, 31).await;
    assert_eq!(takeover["senderAuth"], "verified");
    assert_eq!(takeover["organizerChanged"], true);
    assert_eq!(takeover["autoApply"], "manual");

    store
        .config()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO cal_op (op_id, account_id, target_account_id, uid, kind, partstat, \
                 state, op_seq) VALUES ('r2', 1, 1, 'u1', 'rsvp', 'ACCEPTED', 'done', 1)",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let resent = card(&api, 30).await;
    assert_eq!(resent["stored"]["replyState"], Value::Null);
    assert_eq!(resent["stored"]["revertPartstat"], Value::Null);
}
