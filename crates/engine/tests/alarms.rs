//! AC 21: the reminder timer with a fake clock and the change feed's
//! reminder channel as the push sink.
#![cfg(feature = "calendar")]

use std::path::Path;
use std::sync::Arc;

use tagmail_api_core::calendar_dto::ReminderDto;
use tagmail_api_core::{ChangeFeed, ReminderPush};
use tagmail_calendar::Window;
use tagmail_core::AccountId;
use tagmail_engine::Stores;
use tagmail_engine::alarms::{GRACE_SECS, fire_due};
use tagmail_store::alarms::{self, AlarmRow};
use tagmail_store::calendar::{self, EventRow};
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey, Vault};
use tempfile::TempDir;
use tokio::sync::broadcast;

const ACCOUNT: AccountId = AccountId(1);
const CALENDAR: &str = "/cal/home/";
/// 2026-12-01T09:00:00Z.
const START: i64 = 1_796_115_600;
const EARLY: i64 = START - 30 * 60;
const LATE: i64 = START - 10 * 60;
const SESSION_A: &str = "aa";
const SESSION_B: &str = "bb";
const CACHE: &str = "cache.db";

fn ics(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T120000Z\r\nDTSTART:20261201T090000Z\r\nDTEND:20261201T100000Z\r\n\
         SUMMARY:Standup\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT30M\r\nEND:VALARM\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\n\
         BEGIN:VALARM\r\nACTION:AUDIO\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

struct Env {
    dir: TempDir,
    stores: Stores,
    pushes: broadcast::Receiver<Arc<ReminderPush>>,
}

async fn stores(dir: &Path, cache: &str) -> Stores {
    let config = Arc::new(ConfigDb::open(&dir.join("config.db")).unwrap());
    let cache = Arc::new(CacheDb::open(&dir.join(cache)).unwrap());
    let vault = Arc::new(
        Vault::unlock(
            &config,
            &MasterKey::from_bytes(zeroize::Zeroizing::new([7; 32])),
        )
        .await
        .unwrap(),
    );
    let feed = Arc::new(ChangeFeed::new(cache.head().await.unwrap()));
    Stores {
        config,
        vault,
        cache,
        blobs: Arc::new(BlobStore::open(&dir.join("blobs")).unwrap()),
        feed,
        uploads: dir.join("uploads"),
        outbox: dir.join("outbox"),
    }
}

impl Env {
    /// An account whose calendar holds a 09:00 event with DISPLAY alarms at
    /// 08:30 and 08:50, and one paired device.
    async fn new() -> Env {
        let dir = TempDir::new().unwrap();
        let stores = stores(dir.path(), CACHE).await;
        stores
            .config
            .write(|tx| {
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                     VALUES (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO device (id, name, token_hash) VALUES (7, 'laptop', x'07')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let pushes = stores.feed.subscribe_reminders();
        let env = Env {
            dir,
            stores,
            pushes,
        };
        env.sync().await;
        env
    }

    /// Caches the event as a calendar sync does.
    async fn sync(&self) {
        let cal = tagmail_calendar::parse(ics("standup").as_bytes()).unwrap();
        let rows: Vec<AlarmRow> = cal
            .alarms(&Window::around_unix(START), None)
            .into_iter()
            .map(|a| AlarmRow {
                recurrence_id: a.recurrence_id,
                valarm: a.id,
                fire_at: a.fire_at,
                start: a.start,
                title: a.title,
            })
            .collect();
        self.stores
            .cache
            .write(move |tx| {
                let id = calendar::upsert_calendar(tx, ACCOUNT, CALENDAR, None, None, true)?;
                let row = EventRow {
                    href: format!("{CALENDAR}standup.ics"),
                    uid: "standup".into(),
                    ..EventRow::default()
                };
                let event = calendar::put_event(tx, id, &row, b"ICS", &[])?;
                alarms::replace_alarms(tx, event, &rows)
            })
            .await
            .unwrap();
    }

    async fn sql(&self, sql: &'static str, args: (String, i64)) {
        self.stores
            .config
            .write(move |tx| {
                tx.execute(sql, params![args.0, args.1])?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn subscribe(&self, session: &str, endpoint: &str, reminders: bool) {
        self.sql(
            "INSERT OR IGNORE INTO session (id_hash, expires_at) VALUES (unhex(?1), ?2)",
            (session.into(), i64::MAX),
        )
        .await;
        let endpoint = endpoint.to_owned();
        let session = session.to_owned();
        self.stores
            .config
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth, \
                     reminders) VALUES (unhex(?1), ?2, zeroblob(65), zeroblob(16), ?3)",
                    params![session, endpoint, reminders],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn fire(&self, now: i64) -> Option<i64> {
        fire_due(&self.stores, now).await.unwrap()
    }

    /// The sessions of each push sent since the last call.
    fn pushed(&mut self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        while let Ok(p) = self.pushes.try_recv() {
            assert_eq!(
                (p.account_id, p.title.as_deref(), p.start),
                (1, Some("Standup"), START)
            );
            let mut sessions = p.sessions.clone();
            sessions.sort();
            out.push(sessions);
        }
        out
    }

    async fn device_reminders(&self) -> Vec<ReminderDto> {
        self.stores
            .cache
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT payload FROM change_log WHERE entity = 'reminder' AND op = 'upsert' \
                     ORDER BY seq",
                )?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap()
            .iter()
            .map(|p| serde_json::from_str(p).unwrap())
            .collect()
    }

    /// A restart, on a new `cache.db` if `cache` names one.
    async fn reopen(&mut self, cache: &str) {
        self.stores = stores(self.dir.path(), cache).await;
        self.pushes = self.stores.feed.subscribe_reminders();
    }
}

fn sessions(names: &[&str]) -> Vec<Vec<String>> {
    vec![names.iter().map(|s| (*s).to_owned()).collect()]
}

#[tokio::test]
async fn fires_once_per_target() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    env.subscribe(SESSION_B, "https://push.example/b", true)
        .await;

    assert_eq!(env.fire(EARLY - 60).await, Some(EARLY));
    assert!(env.pushed().is_empty());

    assert_eq!(env.fire(EARLY).await, Some(LATE));
    assert_eq!(env.pushed(), sessions(&[SESSION_A, SESSION_B]));
    let shown = env.device_reminders().await;
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (
            shown[0].devices.as_slice(),
            shown[0].fire_at,
            shown[0].start
        ),
        (&[7][..], EARLY, START)
    );

    env.fire(EARLY + 60).await;
    env.fire(EARLY + 120).await;
    assert!(env.pushed().is_empty(), "sent again");
    assert_eq!(env.device_reminders().await.len(), 1);

    assert_eq!(env.fire(LATE).await, None, "AUDIO alarms are not reminders");
    assert_eq!(env.pushed(), sessions(&[SESSION_A, SESSION_B]));
    assert_eq!(env.device_reminders().await.len(), 2);
}

#[tokio::test]
async fn not_for_reminders_off() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    env.subscribe(SESSION_B, "https://push.example/b", false)
        .await;
    env.sql(
        "UPDATE device SET reminders = 0 WHERE name = ?1 AND id = ?2",
        ("laptop".into(), 7),
    )
    .await;

    env.fire(EARLY).await;
    assert_eq!(env.pushed(), sessions(&[SESSION_A]));
    assert!(env.device_reminders().await.is_empty());
}

#[tokio::test]
async fn not_after_ack() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    let alarm = env
        .stores
        .cache
        .read(|c| Ok(alarms::due_alarms(c, EARLY, EARLY)?.remove(0)))
        .await
        .unwrap();
    env.stores
        .config
        .write(move |tx| alarms::ack(tx, &alarm, EARLY - 300))
        .await
        .unwrap();

    env.fire(EARLY).await;
    assert!(env.pushed().is_empty());
    assert!(env.device_reminders().await.is_empty());
    env.fire(LATE).await;
    assert_eq!(env.pushed(), sessions(&[SESSION_A]), "only that alarm");
}

#[tokio::test]
async fn grace_window_suppresses_stale_after_cache_wipe() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    env.fire(LATE).await;
    assert_eq!(
        env.pushed(),
        sessions(&[SESSION_A]),
        "08:30 is past the grace"
    );
    assert_eq!(env.device_reminders().await.len(), 1);

    env.reopen("rebuilt.db").await;
    env.sync().await;
    env.fire(LATE + 60).await;
    env.fire(LATE + GRACE_SECS).await;
    assert!(env.pushed().is_empty(), "resent after the rebuild");
    assert!(env.device_reminders().await.is_empty());
}

#[tokio::test]
async fn restart_does_not_refire() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    env.fire(EARLY).await;
    assert_eq!(env.pushed(), sessions(&[SESSION_A]));

    env.reopen(CACHE).await;
    env.fire(EARLY + 30).await;
    assert!(env.pushed().is_empty());
    assert_eq!(env.device_reminders().await.len(), 1);
}

#[tokio::test]
async fn renewal_same_session_does_not_refire() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    env.fire(EARLY).await;
    assert_eq!(env.pushed(), sessions(&[SESSION_A]));

    env.sql(
        "DELETE FROM push_subscription WHERE endpoint = ?1 AND ?2 > 0",
        ("https://push.example/a".into(), 1),
    )
    .await;
    env.subscribe(SESSION_A, "https://push.example/renewed", true)
        .await;
    env.fire(EARLY + 60).await;
    assert!(env.pushed().is_empty());

    env.subscribe(SESSION_B, "https://push.example/b", true)
        .await;
    env.fire(EARLY + 120).await;
    assert_eq!(
        env.pushed(),
        sessions(&[SESSION_B]),
        "a new session still gets it"
    );
}

#[tokio::test]
async fn an_alarm_of_a_removed_account_does_not_block_the_others() {
    let mut env = Env::new().await;
    env.subscribe(SESSION_A, "https://push.example/a", true)
        .await;
    let cal = tagmail_calendar::parse(ics("orphan").as_bytes()).unwrap();
    let rows: Vec<AlarmRow> = cal
        .alarms(&Window::around_unix(START), None)
        .into_iter()
        .map(|a| AlarmRow {
            recurrence_id: a.recurrence_id,
            valarm: a.id,
            fire_at: a.fire_at,
            start: a.start,
            title: a.title,
        })
        .collect();
    env.stores
        .cache
        .write(move |tx| {
            let id = calendar::upsert_calendar(tx, AccountId(2), CALENDAR, None, None, true)?;
            let row = EventRow {
                href: format!("{CALENDAR}orphan.ics"),
                uid: "orphan".into(),
                ..EventRow::default()
            };
            let event = calendar::put_event(tx, id, &row, b"ICS", &[])?;
            alarms::replace_alarms(tx, event, &rows)
        })
        .await
        .unwrap();

    assert_eq!(env.fire(EARLY).await, Some(LATE));
    assert_eq!(env.pushed(), sessions(&[SESSION_A]));
    assert_eq!(env.device_reminders().await.len(), 1);
}
