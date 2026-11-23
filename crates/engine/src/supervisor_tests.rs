//! Several accounts in one engine against fake servers: shared budgets,
//! independent backoff, lifecycle, keyword probe and account tests.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tagmail_api_core::dto::TlsMode;
use tagmail_api_core::ops_dto::OpStatusDto;
use tagmail_api_core::store::Secret;
use tagmail_api_core::{AccountRegistry, BoxFuture, NewAccount, OpRequest};
use tagmail_core::ops::OpKind;
use tagmail_core::{AccountId, SpecialUse, ThreadId};
use tagmail_imap::{Flag, PoolConfig};
use tagmail_store::rusqlite::params;
use tagmail_store::{ChangeOp, Changes, ConfigDb, Entity, MasterKey, Vault};
use tempfile::TempDir;
use tokio::time::Instant;

use crate::fake::{FakeAccounts, FakeMsg, FetchFailure, Gauge, Shared, lock, server};
use crate::smtp::{SmtpError, SmtpSender, SmtpServer};
use crate::supervisor::{Engine, EngineConfig, Stores};
use crate::testutil::{TestCache, open};

const CAPS: &[&str] = &["IMAP4rev1", "ENABLE", "IDLE", "CONDSTORE", "QRESYNC"];
const PASSWORD: &str = "secret";
const FAR_POLL: Duration = Duration::from_secs(3600);

/// Answers SMTP checks; sends nothing.
struct SmtpCheck;

impl SmtpSender for SmtpCheck {
    fn send(
        &self,
        _: SmtpServer,
        _: String,
        _: Vec<String>,
        _: std::path::PathBuf,
    ) -> BoxFuture<Result<(), SmtpError>> {
        Box::pin(async { panic!("nothing is sent") })
    }

    fn check(&self, server: SmtpServer) -> BoxFuture<Result<(), SmtpError>> {
        Box::pin(async move {
            if server.password.expose() == PASSWORD {
                Ok(())
            } else {
                Err(SmtpError {
                    permanent: true,
                    message: "535 authentication failed".to_owned(),
                })
            }
        })
    }
}

struct Account {
    id: i64,
    max_connections: u8,
    server: Shared,
}

fn account(id: i64, max_connections: u8, server: &Shared) -> Account {
    Account {
        id,
        max_connections,
        server: server.clone(),
    }
}

struct Fixture {
    engine: Engine<FakeAccounts>,
    factory: FakeAccounts,
    stores: Stores,
    cache: TestCache,
    _dir: TempDir,
}

fn username(id: i64) -> String {
    format!("user{id}")
}

async fn insert_account(config: &ConfigDb, vault: &Vault, a: &Account, password: &str) {
    let sealed = vault.seal(AccountId(a.id), password).unwrap();
    let (id, cap, user) = (a.id, a.max_connections, username(a.id));
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, max_connections, email) \
                 VALUES (?1, 'a', 'imap', 993, 'tls', 'smtp', 465, 'tls', ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    user,
                    sealed.ciphertext,
                    sealed.nonce,
                    cap,
                    format!("{user}@x")
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn config(tweak: impl FnOnce(&mut EngineConfig)) -> EngineConfig {
    let mut config = EngineConfig {
        poll_interval: FAR_POLL,
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        pool: PoolConfig {
            auth_backoff_base: Duration::from_secs(60),
            ..PoolConfig::default()
        },
        start_stagger: Duration::ZERO,
        ..EngineConfig::default()
    };
    tweak(&mut config);
    config
}

async fn fixture(accounts: &[Account], config: EngineConfig) -> Fixture {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let factory = FakeAccounts::default();
    for a in accounts {
        insert_account(&db, &vault, a, PASSWORD).await;
        factory.add(&username(a.id), a.server.clone());
    }
    let cache = open();
    let stores = Stores {
        config: db,
        vault: Arc::new(vault),
        cache: cache.ctx.cache.clone(),
        blobs: cache.ctx.blobs.clone(),
        feed: cache.feed.clone(),
        uploads: dir.path().join("uploads"),
        outbox: dir.path().join("outbox"),
    };
    let engine = Engine::with_smtp(stores.clone(), factory.clone(), config, Arc::new(SmtpCheck));
    Fixture {
        engine,
        factory,
        stores,
        cache,
        _dir: dir,
    }
}

impl Fixture {
    async fn all_live(&self, account: i64) -> bool {
        let states = self
            .cache
            .query("SELECT account_id, sync_state FROM mailbox", |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .await;
        let mine: Vec<_> = states.iter().filter(|(a, _)| *a == account).collect();
        !mine.is_empty() && mine.iter().all(|(_, s)| s == "live")
    }

    async fn has(&self, msgid: &'static str) -> bool {
        self.cache
            .query("SELECT msgid_hdr FROM message", |r| {
                r.get::<_, Option<String>>(0)
            })
            .await
            .iter()
            .any(|m| m.as_deref() == Some(msgid))
    }

    async fn config_count(&self, sql: &'static str, account: i64) -> i64 {
        self.stores
            .config
            .read(move |c| Ok(c.query_row(sql, [account], |r| r.get(0))?))
            .await
            .unwrap()
    }

    async fn probe(&self, account: i64) -> Option<String> {
        self.stores
            .config
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT keyword_probe FROM account WHERE id = ?1",
                    [account],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }

    fn degraded(&self, account: i64) -> Vec<String> {
        self.engine
            .runtime_status(AccountId(account))
            .map(|s| s.degraded)
            .unwrap_or_default()
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn seeded(prefix: &str, count: u32) -> Shared {
    let srv = server(CAPS);
    for mailbox in ["INBOX", "Archive", "Sent"] {
        for i in 0..count {
            let id = format!("{prefix}-{mailbox}-{i}@x");
            lock(&srv).append(
                mailbox,
                FakeMsg::new(&id, "Hi", "ann@x", "me@x", 1_790_000_000 + i64::from(i)),
            );
        }
    }
    srv
}

/// The most full-window FETCHes in flight at once over both accounts'
/// initial sync, with `bulk_permits`.
async fn peak_bulk_fetches(bulk_permits: usize) -> usize {
    let gauge = Arc::new(Gauge::default());
    let (a, b) = (seeded("a", 3), seeded("b", 3));
    for srv in [&a, &b] {
        lock(srv).full_fetch_gauge = Some((gauge.clone(), Duration::from_millis(30)));
    }
    let f = fixture(
        &[account(1, 2, &a), account(2, 2, &b)],
        config(|c| c.bulk_permits = bulk_permits),
    )
    .await;
    assert_eq!(f.engine.start_all().await.unwrap(), 2);
    eventually("both accounts live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    f.engine.shutdown().await;
    gauge.peak()
}

#[tokio::test]
async fn initial_syncs_of_two_accounts_share_the_bulk_permit() {
    assert_eq!(peak_bulk_fetches(1).await, 1);
    assert_eq!(peak_bulk_fetches(2).await, 2, "the gauge sees overlap");
}

#[tokio::test]
async fn inbox_stays_live_while_another_account_syncs_its_backlog() {
    let a = seeded("a", 1);
    let b = seeded("b", 1);
    for i in 0..20 {
        lock(&b).add_mailbox(&format!("Folder{i}"), None, 1000 + i);
        lock(&b).append(
            &format!("Folder{i}"),
            FakeMsg::new(&format!("f{i}@x"), "Old", "ann@x", "me@x", 1_700_000_000),
        );
    }
    lock(&b).full_fetch_gauge = Some((Arc::new(Gauge::default()), Duration::from_millis(150)));
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("account 1 live", || f.all_live(1)).await;
    assert!(!f.all_live(2).await, "account 2 still syncs its folders");

    lock(&a).append(
        "INBOX",
        FakeMsg::new("new@x", "New", "bob@x", "me@x", 1_790_000_500),
    );
    eventually("new mail of account 1", || f.has("new@x")).await;
    assert!(!f.all_live(2).await, "delivered before account 2 finished");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn polls_run_while_waiting_for_the_bulk_permit() {
    const POLL: Duration = Duration::from_millis(200);
    const MAX_TURNS: usize = 50;
    let srv = server(&["IMAP4rev1", "ENABLE", "CONDSTORE", "QRESYNC"]);
    lock(&srv).append("INBOX", FakeMsg::new("i@x", "Hi", "ann@x", "me@x", 1));
    for i in 0..10 {
        lock(&srv).add_mailbox(&format!("Folder{i}"), None, 1000 + i);
        lock(&srv).append(
            &format!("Folder{i}"),
            FakeMsg::new(&format!("f{i}@x"), "Old", "ann@x", "me@x", 1),
        );
    }
    lock(&srv).full_fetch_gauge = Some((Arc::new(Gauge::default()), Duration::from_millis(100)));
    let f = fixture(&[account(1, 2, &srv)], config(|c| c.poll_interval = POLL)).await;
    f.engine.start_all().await.unwrap();
    eventually("INBOX live", || async {
        f.cache
            .query("SELECT name, sync_state FROM mailbox", |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .await
            .iter()
            .any(|(n, s)| n == "INBOX" && s == "live")
    })
    .await;
    let held = f.engine.inner.bulk.acquire().await.unwrap();
    assert!(!f.all_live(1).await, "folders wait for the permit");
    let turns = f.engine.inner.loop_turns.load(Ordering::SeqCst);

    lock(&srv).append("INBOX", FakeMsg::new("new@x", "New", "bob@x", "me@x", 2));
    eventually("new INBOX mail polled", || f.has("new@x")).await;
    tokio::time::sleep(POLL * 3).await;
    let spent = f.engine.inner.loop_turns.load(Ordering::SeqCst) - turns;
    assert!(spent < MAX_TURNS, "{spent} loop turns while waiting");
    assert!(!f.all_live(1).await);
    drop(held);
    eventually("live", || f.all_live(1)).await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn auth_backoff_of_one_account_does_not_hold_back_another() {
    let (a, b) = (seeded("a", 1), seeded("b", 1));
    lock(&a).password = Some("other".to_owned());
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("account 2 live", || f.all_live(2)).await;
    eventually("account 1 backs off", || async {
        f.engine
            .runtime_status(AccountId(1))
            .and_then(|s| s.last_error)
            .is_some_and(|e| e.contains("authentication failed"))
    })
    .await;
    let until = "SELECT count(*) FROM account WHERE id = ?1 AND auth_backoff_until > unixepoch()";
    eventually("backoff persisted", || async {
        f.config_count(until, 1).await == 1
    })
    .await;
    assert_eq!(f.config_count(until, 2).await, 0);
    f.engine.shutdown().await;

    let logins = lock(&a).logins;
    let again = Engine::with_smtp(
        f.stores.clone(),
        f.factory.clone(),
        config(|_| {}),
        Arc::new(SmtpCheck),
    );
    again.start_all().await.unwrap();
    eventually("backoff shown after the restart", || async {
        again
            .runtime_status(AccountId(1))
            .and_then(|s| s.last_error)
            .is_some_and(|e| e.contains("retrying in"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(lock(&a).logins, logins, "no login before the backoff ends");

    let sealed = f.stores.vault.seal(AccountId(1), "changed").unwrap();
    f.stores
        .config
        .write(move |tx| {
            tx.execute(
                "UPDATE account SET secret_ct = ?1, secret_nonce = ?2 WHERE id = 1",
                params![sealed.ciphertext, sealed.nonce],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    lock(&a).password = Some("changed".to_owned());
    again.account_updated(AccountId(1), true);
    eventually("restart logs in with the new password", || f.all_live(1)).await;
    eventually("backoff cleared", || async {
        f.config_count(
            "SELECT count(*) FROM account WHERE id = ?1 AND auth_backoff_until IS NULL",
            1,
        )
        .await
            == 1
    })
    .await;
    again.shutdown().await;
}

#[tokio::test]
async fn a_restart_without_login_changes_keeps_the_auth_backoff() {
    let a = seeded("a", 1);
    lock(&a).password = Some("other".to_owned());
    let f = fixture(&[account(1, 2, &a)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    let backing_off =
        "SELECT count(*) FROM account WHERE id = ?1 AND auth_backoff_until > unixepoch()";
    eventually("backoff persisted", || async {
        f.config_count(backing_off, 1).await == 1
    })
    .await;
    let logins = lock(&a).logins;
    f.stores
        .config
        .write(|tx| {
            tx.execute("UPDATE account SET max_connections = 1 WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    f.engine.restart_account(AccountId(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.config_count(backing_off, 1).await, 1);
    assert_eq!(lock(&a).logins, logins, "no login before the backoff ends");

    f.stores
        .config
        .write(|tx| {
            tx.execute("UPDATE account SET imap_port = 994 WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    f.engine.restart_account(AccountId(1)).await.unwrap();
    eventually("logs in again at once", || async {
        lock(&a).logins > logins
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn connection_caps_are_clamped_to_the_budget() {
    let servers: Vec<Shared> = (0..3).map(|_| server(CAPS)).collect();
    let f = fixture(
        &[
            account(1, 2, &servers[0]),
            account(2, 4, &servers[1]),
            account(3, 1, &servers[2]),
        ],
        config(|c| {
            c.max_total_connections = 3;
            c.start_stagger = Duration::from_millis(100);
        }),
    )
    .await;
    f.engine.start_all().await.unwrap();
    for id in 1..=3 {
        eventually("live", || f.all_live(id)).await;
    }
    let mut caps: Vec<(i64, usize)> = f.engine.connection_caps();
    caps.sort_unstable();
    assert_eq!(caps, [(1, 2), (2, 1), (3, 1)]);
    for srv in &servers[1..] {
        assert!(lock(srv).logins <= 2, "one connection, reconnected once");
    }
    f.engine.stop_account(AccountId(1)).await;
    assert_eq!(f.engine.connection_caps().len(), 2, "released on stop");
    f.engine.shutdown().await;
}

async fn cache_rows(f: &Fixture, account: i64) -> Vec<(&'static str, i64)> {
    let mut out = Vec::new();
    for (table, sql) in [
        (
            "mailbox",
            "SELECT count(*) FROM mailbox WHERE account_id = ?1",
        ),
        (
            "message",
            "SELECT count(*) FROM message WHERE account_id = ?1",
        ),
        (
            "thread",
            "SELECT count(*) FROM thread WHERE account_id = ?1",
        ),
        ("label", "SELECT count(*) FROM label WHERE account_id = ?1"),
        (
            "thread_ref",
            "SELECT count(*) FROM thread_ref WHERE account_id = ?1",
        ),
    ] {
        let n = f
            .stores
            .cache
            .read(move |c| Ok(c.query_row(sql, [account], |r| r.get(0))?))
            .await
            .unwrap();
        out.push((table, n));
    }
    out
}

#[tokio::test]
async fn removing_an_account_purges_its_rows_and_reports_them() {
    let (a, b) = (seeded("a", 2), seeded("b", 3));
    lock(&b).set_flags("INBOX", 1, &[Flag::Keyword("$label_Work".into())]);
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("both live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    f.stores
        .config
        .write(|tx| {
            tx.execute(
                "INSERT INTO label_registry (account_id, label_key, display_name) \
                 VALUES (2, 'work', 'Work')",
                [],
            )?;
            tx.execute(
                "INSERT INTO identity (account_id, address, is_default) VALUES (2, 'b@x', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let thread = f
        .cache
        .query(
            "SELECT thread_id FROM message WHERE account_id = 2 LIMIT 1",
            |r| r.get::<_, i64>(0),
        )
        .await[0];
    f.engine.stop_account(AccountId(2)).await;
    f.engine
        .queue_op(OpRequest {
            op_id: "gone-1".into(),
            kind: OpKind::Star,
            thread_ids: vec![ThreadId(thread)],
            email_ids: Vec::new(),
            label: None,
            mailbox: None,
        })
        .await
        .unwrap();
    let mut mailboxes = f
        .cache
        .query("SELECT id FROM mailbox WHERE account_id = 2", |r| {
            r.get::<_, i64>(0)
        })
        .await;
    mailboxes.sort_unstable();
    let before_account_1 = cache_rows(&f, 1).await;
    let head = f.stores.cache.head().await.unwrap();

    f.engine.account_removed(AccountId(2)).await;
    assert!(
        cache_rows(&f, 2).await.iter().all(|(_, n)| *n == 0),
        "{:?}",
        cache_rows(&f, 2).await
    );
    assert_eq!(cache_rows(&f, 1).await, before_account_1);
    let orphans = f
        .cache
        .query(
            "SELECT count(*) FROM location l LEFT JOIN mailbox mb ON mb.id = l.mailbox_id \
             WHERE mb.id IS NULL",
            |r| r.get::<_, i64>(0),
        )
        .await;
    assert_eq!(orphans, [0]);
    for sql in [
        "SELECT count(*) FROM account WHERE id = ?1",
        "SELECT count(*) FROM identity WHERE account_id = ?1",
        "SELECT count(*) FROM label_registry WHERE account_id = ?1",
        "SELECT count(*) FROM pending_op WHERE account_id = ?1",
        "SELECT count(*) FROM op_seen WHERE account_id = ?1",
    ] {
        assert_eq!(f.config_count(sql, 2).await, 0, "{sql}");
    }
    assert_eq!(
        f.config_count("SELECT count(*) FROM account WHERE id = ?1", 1)
            .await,
        1
    );
    let Changes::Page { changes, .. } = f.stores.cache.changes_since(head, 1000).await.unwrap()
    else {
        panic!("gone");
    };
    let mut bulk: Vec<i64> = changes
        .iter()
        .filter_map(|c| match c.op {
            ChangeOp::Bulk { mailbox } => Some(mailbox),
            _ => None,
        })
        .collect();
    bulk.sort_unstable();
    assert_eq!(bulk, mailboxes);
    let last = changes.last().unwrap();
    assert_eq!(
        (last.entity, last.entity_id, last.op),
        (Entity::Account, 2, ChangeOp::Delete)
    );
    assert!(f.engine.runtime_status(AccountId(2)).is_none());
    assert!(f.engine.runtime_status(AccountId(1)).is_some());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn stopping_an_account_does_not_wait_for_another_accounts_blob_writes() {
    let (a, b) = (seeded("a", 1), seeded("b", 1));
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("both live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    let writing = f.engine.blob_writes(AccountId(2));
    let in_flight = writing.acquire().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), f.engine.stop_account(AccountId(1)))
        .await
        .expect("stopped without waiting for account 2");
    drop(in_flight);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn start_all_resumes_an_interrupted_removal() {
    let (a, b) = (seeded("a", 1), seeded("b", 2));
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("both live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    f.engine.shutdown().await;
    f.stores
        .config
        .write(|tx| {
            tx.execute(
                "UPDATE account SET status = 'deleting', secret_ct = x'', secret_nonce = x'' \
                 WHERE id = 2",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let again = Engine::with_smtp(
        f.stores.clone(),
        f.factory.clone(),
        config(|_| {}),
        Arc::new(SmtpCheck),
    );
    assert_eq!(again.start_all().await.unwrap(), 1);
    eventually("removal finished", || async {
        f.config_count("SELECT count(*) FROM account WHERE id = ?1", 2)
            .await
            == 0
    })
    .await;
    assert!(cache_rows(&f, 2).await.iter().all(|(_, n)| *n == 0));
    assert!(again.runtime_status(AccountId(2)).is_none());
    eventually("account 1 live", || f.all_live(1)).await;
    again.shutdown().await;
}

#[tokio::test]
async fn a_restart_racing_a_removal_does_not_resurrect_the_account() {
    let (a, b) = (seeded("a", 1), seeded("b", 3));
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("both live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    let (restarted, removed) = tokio::join!(
        f.engine.restart_account(AccountId(2)),
        f.engine.remove_account(AccountId(2)),
    );
    restarted.unwrap();
    removed.unwrap();
    assert!(f.engine.runtime_status(AccountId(2)).is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        cache_rows(&f, 2).await.iter().all(|(_, n)| *n == 0),
        "{:?}",
        cache_rows(&f, 2).await
    );
    assert_eq!(
        f.config_count("SELECT count(*) FROM account WHERE id = ?1", 2)
            .await,
        0
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn keyword_probe_result_is_persisted_and_shown() {
    for max_connections in [2, 1] {
        let ok = seeded("ok", 1);
        let dropped = seeded("dropped", 1);
        lock(&dropped).drop_keywords = true;
        let unsupported = seeded("unsupported", 1);
        lock(&unsupported).keyword_capable = false;
        let empty = server(CAPS);
        let f = fixture(
            &[
                account(1, max_connections, &ok),
                account(2, max_connections, &dropped),
                account(3, max_connections, &unsupported),
                account(4, max_connections, &empty),
            ],
            config(|_| {}),
        )
        .await;
        f.engine.start_all().await.unwrap();
        for id in 1..=4 {
            eventually("live", || f.all_live(id)).await;
        }
        assert_eq!(f.probe(1).await.as_deref(), Some("ok"));
        assert_eq!(f.probe(2).await.as_deref(), Some("dropped"));
        assert_eq!(f.probe(3).await.as_deref(), Some("unsupported"));
        assert_eq!(f.probe(4).await.as_deref(), Some("skipped"));
        for srv in [&ok, &dropped] {
            let newest = lock(srv).mailbox("INBOX").uidnext - 1;
            assert!(
                lock(srv).flags_of("INBOX", newest).is_empty(),
                "probe keyword cleared"
            );
        }
        assert!(f.degraded(2).contains(&"labels_degraded".to_owned()));
        assert!(!f.degraded(1).contains(&"labels_degraded".to_owned()));
        let probed_at = "SELECT count(*) FROM account WHERE id = ?1 AND keyword_probe_at > 0";
        assert_eq!(f.config_count(probed_at, 1).await, 1);
        f.engine.shutdown().await;

        lock(&empty).append("INBOX", FakeMsg::new("e@x", "Hi", "a@x", "b@x", 1));
        let logins = lock(&ok).logins;
        let again = Engine::with_smtp(
            f.stores.clone(),
            f.factory.clone(),
            config(|_| {}),
            Arc::new(SmtpCheck),
        );
        again.start_all().await.unwrap();
        eventually("skipped account probed again", || async {
            f.probe(4).await.as_deref() == Some("ok")
        })
        .await;
        eventually("account 1 live", || f.all_live(1)).await;
        assert!(
            lock(&ok).logins - logins <= usize::from(max_connections),
            "account 1 not probed again"
        );
        again.shutdown().await;
    }
}

#[tokio::test]
async fn an_inconclusive_probe_is_not_repeated_on_reconnects() {
    const PROBE_STORES: usize = 2;
    let srv = seeded("a", 1);
    lock(&srv).fail_flag_fetches = Some(FetchFailure::No);
    lock(&srv).fail_after_full_fetches = Some(0);
    let f = fixture(&[account(1, 2, &srv)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("several reconnects", || async { lock(&srv).logins > 4 }).await;
    assert_eq!(lock(&srv).stores.len(), PROBE_STORES, "mark and clear once");
    assert_eq!(f.probe(1).await, None);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn sender_address_missing_is_degraded() {
    let a = seeded("a", 1);
    let f = fixture(&[account(1, 2, &a)], config(|_| {})).await;
    f.stores
        .config
        .write(|tx| {
            tx.execute("UPDATE account SET email = NULL WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    f.engine.start_all().await.unwrap();
    eventually("no sender address shown", || async {
        f.degraded(1).contains(&"no_sender_address".to_owned())
    })
    .await;
    f.stores
        .config
        .write(|tx| {
            tx.execute("UPDATE account SET email = 'a@x' WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    f.engine.account_updated(AccountId(1), false);
    eventually("cleared after the update", || async {
        !f.degraded(1).contains(&"no_sender_address".to_owned())
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn label_added_in_another_account_is_registered_there() {
    let (a, b) = (seeded("a", 1), seeded("b", 1));
    let f = fixture(&[account(1, 2, &a), account(2, 2, &b)], config(|_| {})).await;
    f.engine.start_all().await.unwrap();
    eventually("both live", || async {
        f.all_live(1).await && f.all_live(2).await
    })
    .await;
    f.stores
        .config
        .write(|tx| {
            tx.execute(
                "INSERT INTO label_registry (account_id, label_key, display_name, color, sort) \
                 VALUES (1, 'shared', 'Shared', '#00aa00', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let thread = f
        .cache
        .query(
            "SELECT thread_id FROM message WHERE account_id = 2 LIMIT 1",
            |r| r.get::<_, i64>(0),
        )
        .await[0];
    let head = f.stores.cache.head().await.unwrap();
    f.engine
        .queue_op(OpRequest {
            op_id: "shared-1".into(),
            kind: OpKind::LabelAdd,
            thread_ids: vec![ThreadId(thread)],
            email_ids: Vec::new(),
            label: Some("Shared".into()),
            mailbox: None,
        })
        .await
        .unwrap();
    eventually("op done", || async {
        f.engine.op_dto("shared-1".into()).await.unwrap().status == OpStatusDto::Done
    })
    .await;
    let uid = lock(&b).uid_of("INBOX", "b-INBOX-0@x").unwrap();
    assert!(
        lock(&b)
            .flags_of("INBOX", uid)
            .contains(&"$label_Shared".to_owned())
    );
    let row = f
        .stores
        .config
        .read(|c| {
            Ok(c.query_row(
                "SELECT display_name, color FROM label_registry \
                 WHERE account_id = 2 AND label_key = 'shared'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(row, ("Shared".to_owned(), Some("#00aa00".to_owned())));
    let Changes::Page { changes, .. } = f.stores.cache.changes_since(head, 1000).await.unwrap()
    else {
        panic!("gone");
    };
    let label: serde_json::Value = changes
        .iter()
        .filter(|c| c.entity == Entity::Label && c.entity_id == 0)
        .find_map(|c| serde_json::from_str(c.payload.as_deref()?).ok())
        .expect("label upsert");
    assert_eq!(label["key"], "shared");
    assert_eq!(label["accountIds"], serde_json::json!([1, 2]));
    f.engine.shutdown().await;
}

fn new_account(password: &str) -> NewAccount {
    NewAccount {
        name: "Test".into(),
        imap_host: "imap".into(),
        imap_port: 993,
        imap_tls: TlsMode::Tls,
        smtp_host: "smtp".into(),
        smtp_port: 465,
        smtp_tls: TlsMode::Tls,
        username: username(7),
        password: Secret::new(password.to_owned()),
        max_connections: 2,
        email: "user7@x".into(),
        display_name: None,
    }
}

#[tokio::test]
async fn account_test_logs_in_without_saving() {
    let srv = server(CAPS);
    lock(&srv).password = Some(PASSWORD.to_owned());
    lock(&srv).add_mailbox("Drafts", Some(SpecialUse::Drafts), 400);
    let f = fixture(&[], config(|_| {})).await;
    f.factory.add(&username(7), srv.clone());

    let ok = f.engine.test_account(new_account(PASSWORD)).await;
    assert!(ok.imap.ok && ok.smtp.ok, "{ok:?}");
    assert_eq!(ok.mailboxes, Some(4));
    assert!(ok.capabilities.contains(&"IDLE".to_owned()), "{ok:?}");

    let wrong = f.engine.test_account(new_account("nope")).await;
    assert!(!wrong.imap.ok && !wrong.smtp.ok, "{wrong:?}");
    assert!(
        wrong
            .imap
            .error
            .as_deref()
            .unwrap()
            .contains("wrong password"),
        "{wrong:?}"
    );
    assert!(!wrong.imap.error.as_deref().unwrap().contains("nope"));
    assert_eq!(wrong.mailboxes, None);
    assert_eq!(
        f.config_count("SELECT count(*) FROM account WHERE ?1 = ?1", 0)
            .await,
        0,
        "nothing saved"
    );
    f.engine.shutdown().await;
}
