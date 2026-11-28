//! Discovery through the engine and the runner it starts, against the
//! in-memory CalDAV server.

use std::sync::Arc;
use std::time::Duration;

use tagmail_api_core::calendar_dto::{CaldavStatusDto, SchedulingDto};
use tagmail_api_core::{CalendarError, CalendarService};
use tagmail_core::AccountId;
use tagmail_dav::{DiscoveredAccount, Discovery, Scheduling};
use tagmail_store::calendar::{self, CaldavConfig, CaldavStatus};
use tagmail_store::rusqlite::params;
use tagmail_store::{ConfigDb, MasterKey, Vault};
use tempfile::TempDir;

use super::fake::{FakeConnector, HOME, ME, event, url};
use crate::fake::FakeAccounts;
use crate::supervisor::{Engine, EngineConfig, Stores};
use crate::testutil::{TestCache, open};

const ACCOUNT: AccountId = AccountId(1);
const IMAP_PASSWORD: &str = "imap-secret";
const POLL_STEP: Duration = Duration::from_millis(20);
const POLL_TRIES: usize = 250;
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const MANUAL_URL: &str = "https://cal.example.com/dav/";
const FOREIGN_HOST: &str = "dav.other.example";
const DAV_USER: &str = "dav-user";
const DAV_PASSWORD: &str = "dav-secret";

struct Fixture {
    engine: Engine<FakeAccounts>,
    connector: FakeConnector,
    stores: Stores,
    _cache: TestCache,
    _dir: TempDir,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let config = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let sealed = vault.seal(ACCOUNT, IMAP_PASSWORD).unwrap();
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) \
                 VALUES (?1, 'a', 'imap.example.com', 993, 'tls', 'smtp', 465, 'tls', ?2, ?3, \
                 ?4, ?2)",
                params![ACCOUNT.0, ME, sealed.ciphertext, sealed.nonce],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let cache = open();
    let stores = Stores {
        config,
        vault: Arc::new(vault),
        cache: cache.ctx.cache.clone(),
        blobs: cache.ctx.blobs.clone(),
        feed: cache.feed.clone(),
        uploads: dir.path().join("uploads"),
        outbox: dir.path().join("outbox"),
    };
    let engine = Engine::new(
        stores.clone(),
        FakeAccounts::default(),
        EngineConfig::default(),
    );
    let connector = FakeConnector::default();
    engine.start_calendar_on(Arc::new(connector.clone()));
    Fixture {
        engine,
        connector,
        stores,
        _cache: cache,
        _dir: dir,
    }
}

fn found() -> Discovery {
    Discovery::Found(Box::new(DiscoveredAccount {
        host: "dav.example.com".into(),
        context_url: url("/"),
        principal: url("/principals/me/"),
        home: url(HOME),
        scheduling: Scheduling::default(),
        calendars: Vec::new(),
    }))
}

impl Fixture {
    async fn caldav(&self) -> CaldavConfig {
        self.stores
            .config
            .read(|c| calendar::load_caldav(c, ACCOUNT))
            .await
            .unwrap()
            .unwrap_or_default()
    }

    async fn uids(&self) -> Vec<String> {
        self.stores
            .cache
            .read(|c| {
                let mut stmt = c.prepare("SELECT uid FROM cal_event ORDER BY uid")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?)
            })
            .await
            .unwrap()
    }

    async fn eventually_uids(&self, expected: &[&str]) {
        for _ in 0..POLL_TRIES {
            if self.uids().await == expected {
                return;
            }
            tokio::time::sleep(POLL_STEP).await;
        }
        assert_eq!(self.uids().await, expected);
    }
}

#[tokio::test]
async fn imap_credentials_reused() {
    let f = fixture().await;
    f.connector.dav.calendar(None, None);
    f.connector
        .dav
        .put("one.ics", "\"1\"", &event("one", 0, None));
    let mut discovered = found();
    if let Discovery::Found(account) = &mut discovered {
        account.calendars = f.connector.dav.calendars();
    }
    f.connector.discovers(Ok(discovered));

    let dto = f.engine.discover(ACCOUNT).await.unwrap();
    assert_eq!(dto.status, CaldavStatusDto::Ok);
    assert_eq!(dto.scheduling_detected, Some(SchedulingDto::Client));
    assert_eq!(dto.calendars.len(), 1);
    assert_eq!(dto.calendars[0].href, "/home/work/");
    let asked = &f.connector.asked()[0];
    assert_eq!(
        (asked.username.as_str(), asked.password.as_str()),
        (ME, IMAP_PASSWORD)
    );
    assert_eq!(
        (asked.email.as_str(), asked.manual_url.as_deref()),
        (ME, None)
    );

    let stored = f.caldav().await;
    assert_eq!(
        stored.home_url.as_deref(),
        Some("https://dav.example.com/home/")
    );
    assert_eq!(stored.confirmed_host.as_deref(), Some("dav.example.com"));
    assert_eq!(stored.discovery, Some(calendar::Discovery::WellKnown));
    f.eventually_uids(&["one"]).await;
    let connected = &f.connector.connected()[0];
    assert_eq!(
        (connected.username.as_str(), connected.password.as_str()),
        (ME, IMAP_PASSWORD)
    );
    assert_eq!(connected.confirmed_host, "dav.example.com");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_foreign_host_waits_for_confirmation() {
    let f = fixture().await;
    f.connector.discovers(Ok(Discovery::NeedsConfirmation {
        host: "dav.other.example".into(),
        url: url::Url::parse("https://dav.other.example/dav/").unwrap(),
    }));
    let dto = f.engine.discover(ACCOUNT).await.unwrap();
    assert_eq!(dto.status, CaldavStatusDto::NeedsConfirmation);
    assert_eq!(dto.host.as_deref(), Some("dav.other.example"));
    assert_eq!(f.caldav().await.status, CaldavStatus::NeedsConfirmation);

    assert_eq!(
        f.engine
            .confirm_host(ACCOUNT, "evil.example".into())
            .await
            .unwrap_err(),
        CalendarError::HostMismatch
    );
    f.connector.discovers(Ok(found()));
    let dto = f
        .engine
        .confirm_host(ACCOUNT, "dav.other.example".into())
        .await
        .unwrap();
    assert_eq!(dto.status, CaldavStatusDto::Ok);
    let asked = f.connector.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[1].confirmed_host.as_deref(),
        Some("dav.other.example")
    );
    f.engine.shutdown().await;
}

async fn set_manual_url(f: &Fixture, override_login: bool) {
    let sealed = f.stores.vault.seal_caldav(ACCOUNT, DAV_PASSWORD).unwrap();
    f.stores
        .config
        .write(move |tx| {
            calendar::save_caldav(
                tx,
                ACCOUNT,
                &CaldavConfig {
                    url: Some(MANUAL_URL.into()),
                    username: override_login.then(|| DAV_USER.to_owned()),
                    secret: override_login.then_some(sealed),
                    ..CaldavConfig::default()
                },
            )
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn credential_override() {
    let f = fixture().await;
    set_manual_url(&f, true).await;
    f.connector.dav.calendar(None, None);
    f.connector
        .dav
        .put("one.ics", "\"1\"", &event("one", 0, None));
    f.connector
        .discovers(Err(tagmail_dav::DavError::Unauthorized));
    let dto = f.engine.discover(ACCOUNT).await.unwrap();
    assert_eq!(dto.status, CaldavStatusDto::Error);
    assert!(dto.error.unwrap().contains("credentials"));
    f.connector.discovers(Ok(found()));
    assert_eq!(
        f.engine.discover(ACCOUNT).await.unwrap().status,
        CaldavStatusDto::Ok
    );
    for asked in f.connector.asked() {
        assert_eq!(
            (asked.username.as_str(), asked.password.as_str()),
            (DAV_USER, DAV_PASSWORD)
        );
        assert_eq!(asked.manual_url.as_deref(), Some(MANUAL_URL));
    }
    f.eventually_uids(&["one"]).await;
    let connected = &f.connector.connected()[0];
    assert_eq!(
        (connected.username.as_str(), connected.password.as_str()),
        (DAV_USER, DAV_PASSWORD)
    );
    let stored = f.caldav().await;
    assert_eq!(stored.discovery, Some(calendar::Discovery::Manual));
    assert_eq!(stored.url.as_deref(), Some(MANUAL_URL));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_manual_url_redirected_to_another_domain_is_confirmed() {
    let f = fixture().await;
    set_manual_url(&f, false).await;
    f.connector.discovers(Ok(Discovery::NeedsConfirmation {
        host: FOREIGN_HOST.into(),
        url: url::Url::parse("https://dav.other.example/dav/").unwrap(),
    }));
    let dto = f.engine.discover(ACCOUNT).await.unwrap();
    assert_eq!(dto.status, CaldavStatusDto::NeedsConfirmation);
    assert_eq!(dto.host.as_deref(), Some(FOREIGN_HOST));
    let pending = f.caldav().await;
    assert_eq!(pending.url.as_deref(), Some(MANUAL_URL));
    assert_eq!(pending.pending_host.as_deref(), Some(FOREIGN_HOST));

    f.connector.discovers(Ok(found()));
    let dto = f
        .engine
        .confirm_host(ACCOUNT, FOREIGN_HOST.into())
        .await
        .unwrap();
    assert_eq!(dto.status, CaldavStatusDto::Ok);
    let asked = &f.connector.asked()[1];
    assert_eq!(asked.manual_url.as_deref(), Some(MANUAL_URL));
    assert_eq!(asked.confirmed_host.as_deref(), Some(FOREIGN_HOST));
    let stored = f.caldav().await;
    assert_eq!(stored.pending_host, None);
    assert_eq!(stored.url.as_deref(), Some(MANUAL_URL));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_hanging_server() {
    let f = fixture().await;
    f.connector.dav.calendar(None, None);
    f.connector.dav.block();
    f.connector.discovers(Ok(found()));
    f.engine.discover(ACCOUNT).await.unwrap();
    for _ in 0..POLL_TRIES {
        if f.connector.dav.took().contains(&"list_calendars") {
            break;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
    tokio::time::timeout(SHUTDOWN_BUDGET, f.engine.shutdown())
        .await
        .expect("shutdown cancels the poll in progress");
}

#[tokio::test]
async fn removing_the_account_purges_its_calendars() {
    let f = fixture().await;
    f.connector.dav.calendar(None, None);
    f.connector
        .dav
        .put("one.ics", "\"1\"", &event("one", 0, None));
    f.connector.discovers(Ok(found()));
    f.engine.discover(ACCOUNT).await.unwrap();
    f.eventually_uids(&["one"]).await;
    f.engine.remove_account(ACCOUNT).await.unwrap();
    assert!(f.uids().await.is_empty());
    let calendars = f
        .stores
        .cache
        .read(|c| calendar::calendars(c, None))
        .await
        .unwrap();
    assert!(calendars.is_empty());
    assert_eq!(f.caldav().await, CaldavConfig::default());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn confirm_host_compares_idn_and_ipv6_spellings() {
    for (pending, typed) in [
        ("dav.xn--bcher-kva.example", "Dav.B\u{fc}cher.example."),
        ("[::1]", "::1"),
    ] {
        let f = fixture().await;
        f.connector.discovers(Ok(Discovery::NeedsConfirmation {
            host: pending.into(),
            url: url::Url::parse(&format!("https://{pending}/dav/")).unwrap(),
        }));
        f.engine.discover(ACCOUNT).await.unwrap();
        f.connector.discovers(Ok(found()));
        let dto = f.engine.confirm_host(ACCOUNT, typed.into()).await.unwrap();
        assert_eq!(dto.status, CaldavStatusDto::Ok, "{typed}");
        assert_eq!(
            f.connector.asked()[1].confirmed_host.as_deref(),
            Some(pending)
        );
        f.engine.shutdown().await;
    }
}
