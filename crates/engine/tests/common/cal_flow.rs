//! An engine account with a calendar and the API in front of it, for the
//! invitation tests: invitations arrive by IMAP APPEND, answers go out
//! through Mailpit.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tagmail_api_core::calendar_dto::CaldavStatusDto;
use tagmail_api_core::ops_dto::{OpDto, OpStatusDto};
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_api_core::{Api, CalendarService, Method, Request, WriteService};
use tagmail_core::AccountId;
use tagmail_dav::DiscoveryEnv;
use tagmail_imap::ImapSession;
use tagmail_store::calendar::{self, CaldavConfig, Scheduling};
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey};

use super::cal_fixture::{caldav_base, home_path};
use super::{
    DovecotFactory, Fixture, PASSWORD, Running, Sending, email_of, encode, eventually, mailpit,
    second,
};

/// Upper bound on server round trips; generous for slow test hosts.
pub const BUDGET: Duration = Duration::from_secs(60);
/// How long "nothing arrives" is watched for.
pub const QUIET: Duration = Duration::from_secs(4);
const POLL_STEP: Duration = Duration::from_millis(100);
pub const DEFAULT_CALENDAR: &str = "default/";
pub const TRUSTED: &str = "mx.tagmail.test";

pub struct CalAccount {
    pub fixture: Fixture,
    pub running: Running,
    pub api: Api<SqliteStore>,
    pub user: &'static str,
}

/// The calendar href the account's invitations go to.
pub fn default_href(user: &str) -> String {
    format!("{}{DEFAULT_CALENDAR}", home_path(user))
}

/// Points `account` at the CalDAV server over plain http.
pub async fn configure(running: &Running, account: AccountId, scheduling: Option<Scheduling>) {
    running
        .config
        .write(move |tx| {
            calendar::save_caldav(
                tx,
                account,
                &CaldavConfig {
                    url: Some(format!("{}/", caldav_base())),
                    allow_insecure: true,
                    scheduling,
                    ..CaldavConfig::default()
                },
            )
        })
        .await
        .unwrap();
}

pub async fn discover(running: &Running, account: AccountId) {
    let mut found = running.engine.discover(account).await.unwrap();
    if found.status == CaldavStatusDto::NeedsConfirmation {
        let host = found.host.clone().expect("a host to confirm");
        found = running.engine.confirm_host(account, host).await.unwrap();
    }
    assert_eq!(found.status, CaldavStatusDto::Ok, "{:?}", found.error);
}

/// Waits for a calendar poll that saw `account`, so a poll that started
/// before discovery can no longer drop its calendars.
pub async fn synced(running: &Running, account: AccountId) {
    eventually("calendar synced", BUDGET, || async {
        running
            .query(
                "SELECT account_id FROM calendar WHERE synced_at IS NOT NULL",
                |r| r.get::<_, i64>(0),
            )
            .await
            .contains(&account.0)
    })
    .await;
}

pub async fn set_default(running: &Running, account: AccountId, href: Option<String>) {
    running
        .config
        .write(move |tx| calendar::set_default_calendar(tx, account, href.as_deref()))
        .await
        .unwrap();
}

pub async fn set_trust(running: &Running, account: AccountId, authserv_id: Option<&str>) {
    let authserv_id = authserv_id.map(str::to_owned);
    running
        .config
        .write(move |tx| {
            let mut c = calendar::load_caldav(tx, account)?.unwrap_or_default();
            c.authserv_id = authserv_id;
            calendar::save_caldav(tx, account, &c)
        })
        .await
        .unwrap();
}

/// The API over the fixture's databases, with the engine behind it.
pub async fn api(fixture: &Fixture, running: &Running) -> Api<SqliteStore> {
    let dir = fixture.dir().to_owned();
    let key = std::fs::read(dir.join("master.key")).unwrap();
    let master = MasterKey::from_bytes(zeroize::Zeroizing::new(key.try_into().unwrap()));
    let (config, cache, blobs) = tokio::task::spawn_blocking(move || {
        (
            ConfigDb::open(&dir.join("config.db")).unwrap(),
            CacheDb::open(&dir.join("cache.db")).unwrap(),
            BlobStore::open(&dir.join("blobs")).unwrap(),
        )
    })
    .await
    .unwrap();
    let store = SqliteStore::open(config, cache, blobs, &master)
        .await
        .unwrap();
    let engine: Arc<tagmail_engine::Engine<DovecotFactory>> = Arc::new(running.engine.clone());
    Api::new(Arc::new(store), Some(engine.clone()))
        .with_calendar(engine.clone())
        .with_writes(engine)
}

impl CalAccount {
    /// `user`'s account syncing mail and its calendar, invitations going
    /// to its `default` calendar, with `scheduling` as the owner's choice.
    pub async fn start(user: &'static str, scheduling: Option<Scheduling>) -> CalAccount {
        let fixture = Fixture::existing(user, PASSWORD);
        let running = fixture.start_sending(Sending::default()).await;
        configure(&running, ACCOUNT, scheduling).await;
        running
            .engine
            .start_calendar_with(DiscoveryEnv::system())
            .unwrap();
        discover(&running, ACCOUNT).await;
        synced(&running, ACCOUNT).await;
        set_default(&running, ACCOUNT, Some(default_href(user))).await;
        let api = api(&fixture, &running).await;
        CalAccount {
            fixture,
            running,
            api,
            user,
        }
    }

    pub fn email(&self) -> String {
        email_of(self.user)
    }

    pub async fn call(&self, method: Method, path: &str, body: &Value) -> (u16, Value) {
        let body = serde_json::to_vec(body).unwrap();
        let resp = self
            .api
            .handle(Request {
                method,
                path,
                query: "",
                body: &body,
            })
            .await;
        let json = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        (resp.status, json)
    }

    /// Delivers `raw` to the INBOX and waits until it is synced; returns
    /// its message id.
    pub async fn deliver(&self, raw: &str, subject: &str) -> i64 {
        let mut s = second(self.user).await;
        s.append("INBOX", &[], raw.as_bytes()).await.unwrap();
        self.received(|s| s == subject).await
    }

    /// Waits until a message with a calendar part whose subject passes
    /// `subject` is synced; returns its message id.
    pub async fn received(&self, subject: impl Fn(&str) -> bool) -> i64 {
        let deadline = tokio::time::Instant::now() + BUDGET;
        loop {
            let rows = self
                .running
                .query(
                    "SELECT id, subject FROM message WHERE has_calendar = 1",
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .await;
            if let Some((id, _)) = rows
                .into_iter()
                .find(|(_, s)| s.as_deref().is_some_and(&subject))
            {
                return id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "invitation not synced within {BUDGET:?}"
            );
            tokio::time::sleep(POLL_STEP).await;
        }
    }

    pub async fn invitation(&self, message: i64) -> Value {
        let (status, card) = self
            .call(
                Method::Get,
                &format!("/api/messages/{message}/invitation"),
                &Value::Null,
            )
            .await;
        assert_eq!(status, 200, "{card}");
        card
    }

    pub async fn submit(&self, route: &str, body: Value) -> OpDto {
        let (status, op) = self
            .call(Method::Post, &format!("/api/calendar/{route}"), &body)
            .await;
        assert_eq!(status, 202, "{op}");
        serde_json::from_value(op).unwrap()
    }

    pub async fn op(&self, op_id: &str) -> OpDto {
        WriteService::op(&self.running.engine, op_id.into())
            .await
            .unwrap()
    }

    /// Waits until the op is no longer pending.
    pub async fn settled(&self, op_id: &str) -> OpDto {
        eventually("op settled", BUDGET, || async {
            self.op(op_id).await.status != OpStatusDto::Pending
        })
        .await;
        self.op(op_id).await
    }

    /// Syncs the calendar now and waits until it has the event `uid` in
    /// its cache.
    pub async fn cached(&self, uid: &str) {
        self.running.engine.settings_changed(ACCOUNT);
        let uid = uid.to_owned();
        eventually("event cached", BUDGET, || {
            let uid = uid.clone();
            async move {
                self.running
                    .query("SELECT uid FROM cal_event", |r| r.get::<_, String>(0))
                    .await
                    .contains(&uid)
            }
        })
        .await;
    }

    /// Waits until the runner went through every queued invitation.
    pub async fn inbox_drained(&self) {
        eventually("cal_inbox drained", BUDGET, || async {
            self.running
                .query("SELECT count(*) FROM cal_inbox", |r| r.get::<_, i64>(0))
                .await
                == [0]
        })
        .await;
    }

    pub async fn cal_ops(&self, kind: &'static str) -> Vec<String> {
        let ops: Vec<(String, String)> = self
            .running
            .config
            .read(|c| {
                let mut stmt = c.prepare("SELECT op_id, kind FROM cal_op ORDER BY op_id")?;
                Ok(stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?)
            })
            .await
            .unwrap();
        ops.into_iter()
            .filter(|(_, k)| k == kind)
            .map(|(id, _)| id)
            .collect()
    }

    pub async fn outbox_ops(&self) -> Vec<String> {
        self.running
            .config
            .read(|c| {
                let mut stmt = c.prepare("SELECT op_id FROM outbox ORDER BY id")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?)
            })
            .await
            .unwrap()
    }

    pub async fn stop(self) {
        self.running.stop().await;
    }
}

pub const ACCOUNT: AccountId = AccountId(1);

/// An invitation from `organizer` to `me`.
pub struct Invite<'a> {
    pub uid: &'a str,
    pub method: &'a str,
    pub sequence: u32,
    pub dtstamp: &'a str,
    pub summary: &'a str,
    pub organizer: &'a str,
    pub me: &'a str,
    pub partstat: &'a str,
    /// Header lines above `From`, e.g. `Authentication-Results`.
    pub headers: &'a str,
    pub from: &'a str,
}

impl Invite<'_> {
    pub fn vevent(&self) -> String {
        let status = if self.method == "CANCEL" {
            "STATUS:CANCELLED\r\n"
        } else {
            ""
        };
        format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:{}\r\nSEQUENCE:{}\r\n\
             DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:{}\r\n{status}\
             ORGANIZER;CN=Organizer:mailto:{}\r\n\
             ATTENDEE;PARTSTAT={};RSVP=TRUE:mailto:{}\r\nEND:VEVENT\r\n",
            self.uid,
            self.dtstamp,
            self.sequence,
            self.summary,
            self.organizer,
            self.partstat,
            self.me
        )
    }

    /// The object as a calendar stores it (no METHOD).
    pub fn stored(&self) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\n{}END:VCALENDAR\r\n",
            self.vevent()
        )
    }

    pub fn ics(&self) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\nMETHOD:{}\r\n{}\
             END:VCALENDAR\r\n",
            self.method,
            self.vevent()
        )
    }

    pub fn mail(&self, subject: &str) -> String {
        format!(
            "{}From: <{}>\r\nTo: <{}>\r\nSubject: {subject}\r\n\
             Message-ID: <{}-{subject}@tagmail.test>\r\nDate: Wed, 25 Nov 2026 10:00:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/calendar; method={}; charset=utf-8\r\n\r\n{}",
            self.headers,
            self.from,
            self.me,
            self.uid,
            self.method,
            self.ics()
        )
    }
}

/// A UID no earlier run used: Mailpit keeps the mail of every run.
pub fn unique_uid(test: &str) -> String {
    format!("{test}-{}", super::nanos())
}

/// The raw REPLYs in Mailpit to `organizer` about `uid`.
pub async fn replies(organizer: &str, uid: &str) -> Vec<String> {
    let query = encode(&format!("to:\"{organizer}\""));
    let found: Value =
        serde_json::from_slice(&mailpit(&format!("/api/v1/search?query={query}&limit=100")).await)
            .unwrap();
    let mut out = Vec::new();
    for m in found["messages"].as_array().cloned().unwrap_or_default() {
        let Some(id) = m["ID"].as_str() else {
            continue;
        };
        let raw = String::from_utf8_lossy(&mailpit(&format!("/api/v1/message/{id}/raw")).await)
            .into_owned();
        let unfolded = raw.replace("\r\n ", "").replace("\n ", "");
        if unfolded.contains(&format!("UID:{uid}")) && unfolded.contains("METHOD:REPLY") {
            out.push(unfolded);
        }
    }
    out
}

/// Waits for `n` REPLYs, then watches that no more arrive.
pub async fn exactly_replies(organizer: &str, uid: &str, n: usize) -> Vec<String> {
    eventually("replies arrived", BUDGET, || async {
        replies(organizer, uid).await.len() >= n
    })
    .await;
    tokio::time::sleep(QUIET).await;
    let found = replies(organizer, uid).await;
    assert_eq!(found.len(), n, "REPLYs to {organizer} for {uid}");
    found
}
