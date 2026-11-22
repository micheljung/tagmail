//! One task per account (plan §3.4 connection budget): a worker connection
//! runs initial sync, polls and the text pass; a second connection IDLEs on
//! INBOX and wakes the worker. With a budget of one, the worker IDLEs
//! itself between polls. Failures back off and show in the runtime status.

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tagmail_api_core::{AccountRegistry, ChangeFeed, RuntimeStatus};
use tagmail_core::AccountId;
use tagmail_imap::{
    Capabilities, CapabilityOverride, Connector, IdleStop, ImapSession, Password, Pool, PoolConfig,
    Pooled, Security, SelectOptions, ServerConfig,
};
use tagmail_store::rusqlite::OptionalExtension;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, Sealed, Vault};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cache::{MailboxRow, SyncState};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::sync::{self, Progress};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// How often mailboxes other than INBOX (and INBOX without IDLE) are
    /// polled.
    pub poll_interval: Duration,
    /// Messages per text-pass batch.
    pub text_batch: usize,
    /// Reconnect backoff after a failure, doubled up to `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Authentication backoff and pool settings; `max_connections` comes
    /// from the account.
    pub pool: PoolConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            poll_interval: Duration::from_secs(120),
            text_batch: 100,
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            pool: PoolConfig::default(),
        }
    }
}

/// An account's IMAP settings with the decrypted password.
#[derive(Debug, Clone)]
pub struct AccountSettings {
    pub id: AccountId,
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: Password,
    pub max_connections: usize,
}

/// Builds the IMAP connector of an account.
pub trait ConnectorFactory: Send + Sync + 'static {
    type Connector: Connector;

    fn connector(&self, account: &AccountSettings) -> Self::Connector;
}

/// Connects with TLS or STARTTLS as configured, trusting the Mozilla roots.
#[derive(Debug, Clone, Default)]
pub struct ImapConnectorFactory {
    pub capability_override: CapabilityOverride,
    /// Connect without TLS whatever the account says; for local test
    /// servers only.
    pub insecure_plaintext: bool,
}

impl ConnectorFactory for ImapConnectorFactory {
    type Connector = ServerConfig;

    fn connector(&self, a: &AccountSettings) -> ServerConfig {
        let mut config = ServerConfig::new(
            a.host.clone(),
            a.port,
            if self.insecure_plaintext {
                Security::Plain
            } else {
                a.security
            },
            a.username.clone(),
            a.password.clone(),
        );
        config.capability_override = self.capability_override.clone();
        config
    }
}

/// The databases and feed the engine works on.
#[derive(Clone)]
pub struct Stores {
    pub config: Arc<ConfigDb>,
    pub vault: Arc<Vault>,
    pub cache: Arc<CacheDb>,
    pub blobs: Arc<BlobStore>,
    pub feed: Arc<ChangeFeed>,
}

/// Reads an account and decrypts its password.
pub async fn load_account(stores: &Stores, id: AccountId) -> Result<AccountSettings> {
    let row = stores
        .config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT imap_host, imap_port, imap_tls, username, secret_ct, secret_nonce, \
                 max_connections FROM account WHERE id = ?1",
                [id.0],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u16>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        Sealed {
                            ciphertext: r.get(4)?,
                            nonce: r.get(5)?,
                        },
                        r.get::<_, u8>(6)?,
                    ))
                },
            )
            .optional()?)
        })
        .await?;
    let (host, port, tls, username, sealed, max_connections) = row.ok_or(Error::NoAccount(id.0))?;
    let security = match tls.as_str() {
        "tls" => Security::Tls,
        "starttls" => Security::StartTls,
        other => return Err(Error::InvalidAccount(format!("imap_tls {other:?}"))),
    };
    let password = stores
        .vault
        .open(id, &sealed)
        .map_err(tagmail_store::Error::from)?;
    Ok(AccountSettings {
        id,
        host,
        port,
        security,
        username,
        password: Password::new(password.as_str()),
        max_connections: usize::from(max_connections),
    })
}

type Status = Arc<Mutex<RuntimeStatus>>;

fn update(status: &Status, f: impl FnOnce(&mut RuntimeStatus)) {
    f(&mut status.lock().unwrap_or_else(PoisonError::into_inner));
}

struct Handle {
    status: Status,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct Inner<F> {
    config: EngineConfig,
    factory: F,
    stores: Stores,
    accounts: Mutex<HashMap<AccountId, Handle>>,
    runtime: tokio::runtime::Handle,
}

/// The sync engine: implements [`AccountRegistry`] for the API.
pub struct Engine<F: ConnectorFactory = ImapConnectorFactory> {
    inner: Arc<Inner<F>>,
}

impl<F: ConnectorFactory> Engine<F> {
    /// Must be called inside a tokio runtime, which runs the account tasks.
    pub fn new(stores: Stores, factory: F, config: EngineConfig) -> Engine<F> {
        Engine {
            inner: Arc::new(Inner {
                config,
                factory,
                stores,
                accounts: Mutex::new(HashMap::new()),
                runtime: tokio::runtime::Handle::current(),
            }),
        }
    }

    /// Starts every `active` account in `config.db`; returns how many.
    pub async fn start_all(&self) -> Result<usize> {
        let ids: Vec<i64> = self
            .inner
            .stores
            .config
            .read(|c| {
                let mut stmt =
                    c.prepare("SELECT id FROM account WHERE status = 'active' ORDER BY id")?;
                let ids = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?;
                Ok(ids)
            })
            .await?;
        for &id in &ids {
            self.start(AccountId(id));
        }
        Ok(ids.len())
    }

    /// Starts syncing an account unless it already runs.
    pub fn start(&self, id: AccountId) {
        let mut accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if accounts.get(&id).is_some_and(|h| !h.task.is_finished()) {
            return;
        }
        let status = Status::default();
        let (stop, stopped) = watch::channel(false);
        let task = self
            .inner
            .runtime
            .spawn(run(self.inner.clone(), id, status.clone(), stopped));
        accounts.insert(id, Handle { status, stop, task });
    }

    /// Stops all accounts and waits for their tasks.
    pub async fn shutdown(&self) {
        let handles: Vec<Handle> = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in &handles {
            h.stop.send_replace(true);
        }
        for h in handles {
            if let Err(e) = h.task.await
                && e.is_panic()
            {
                tracing::error!("account task panicked");
            }
        }
    }
}

impl<F: ConnectorFactory> AccountRegistry for Engine<F> {
    fn account_added(&self, id: AccountId) {
        self.start(id);
    }

    fn runtime_status(&self, id: AccountId) -> Option<RuntimeStatus> {
        let accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let handle = accounts.get(&id)?;
        let status = handle
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Some(status)
    }
}

/// `base` scaled by a random factor in [0.5, 1.5).
fn jitter(base: Duration) -> Duration {
    let r = RandomState::new().hash_one(Instant::now()) % 1000;
    base.mul_f64(0.5 + r as f64 / 1000.0)
}

async fn run<F: ConnectorFactory>(
    inner: Arc<Inner<F>>,
    id: AccountId,
    status: Status,
    mut stop: watch::Receiver<bool>,
) {
    let settings = match load_account(&inner.stores, id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(account = id.0, error = %e, "cannot load account");
            update(&status, |s| s.last_error = Some(e.to_string()));
            return;
        }
    };
    let pool = Pool::new(
        inner.factory.connector(&settings),
        PoolConfig {
            max_connections: settings.max_connections,
            ..inner.config.pool.clone()
        },
    );
    let stores = &inner.stores;
    let ctx = Ctx::new(
        stores.cache.clone(),
        stores.blobs.clone(),
        stores.feed.clone(),
        id,
    );
    let mut retry = inner.config.retry_min;
    loop {
        let result = tokio::select! {
            r = account_loop(&inner.config, &pool, &ctx, &status, &mut retry) => r,
            _ = stop.wait_for(|s| *s) => return,
        };
        let wait = match result {
            Ok(()) => continue,
            Err(Error::Imap(tagmail_imap::Error::AuthBackoff { retry_in })) => {
                let reason = pool
                    .auth_backoff()
                    .and_then(|(_, text)| text)
                    .unwrap_or_default();
                update(&status, |s| {
                    s.last_error = Some(format!(
                        "authentication failed ({reason}); retrying in {}s",
                        retry_in.as_secs()
                    ));
                });
                retry_in
            }
            Err(e) => {
                tracing::warn!(account = id.0, error = %e, "sync failed, reconnecting");
                update(&status, |s| s.last_error = Some(e.to_string()));
                let wait = jitter(retry);
                retry = (retry * 2).min(inner.config.retry_max);
                wait
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

fn capability_names(caps: &Capabilities) -> Vec<String> {
    let checks: [(&str, bool); 8] = [
        ("CONDSTORE", caps.condstore()),
        ("QRESYNC", caps.qresync()),
        ("IDLE", caps.idle()),
        ("MOVE", caps.r#move()),
        ("UIDPLUS", caps.uidplus()),
        ("SPECIAL-USE", caps.special_use()),
        ("OBJECTID", caps.objectid()),
        ("X-GM-EXT-1", caps.gmail()),
    ];
    checks
        .into_iter()
        .filter(|(_, has)| *has)
        .map(|(n, _)| n.to_owned())
        .collect()
}

fn degraded(caps: &Capabilities, rows: &[MailboxRow]) -> Vec<String> {
    let mut out = Vec::new();
    if !caps.condstore() {
        out.push("no_condstore".to_owned());
    } else if !caps.qresync() {
        out.push("no_qresync".to_owned());
    }
    if !caps.idle() {
        out.push("no_idle".to_owned());
    }
    for r in rows.iter().filter(|r| r.state == SyncState::Live) {
        if r.nomodseq && caps.condstore() {
            out.push(format!("no_modseq:{}", r.name));
        }
        if !r.keyword_capable {
            out.push(format!("no_keywords:{}", r.name));
        }
    }
    out
}

/// Aborts the task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// IDLEs on INBOX on its own connection and flags INBOX dirty on changes.
async fn idle_loop<C: Connector>(
    pool: Pool<C>,
    dirty: Arc<AtomicBool>,
    wake: Arc<Notify>,
    retry_min: Duration,
    retry_max: Duration,
) {
    let mut retry = retry_min;
    loop {
        let result: Result<()> = async {
            let mut s = pool.acquire().await?;
            let read_only = SelectOptions {
                read_only: true,
                qresync: None,
            };
            s.select("INBOX", read_only).await?;
            retry = retry_min;
            let never = Notify::new();
            loop {
                let r = s.idle(&never).await?;
                if r.stop == IdleStop::Changed {
                    dirty.store(true, Ordering::SeqCst);
                    wake.notify_one();
                }
            }
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "INBOX IDLE failed, reconnecting");
        }
        tokio::time::sleep(jitter(retry)).await;
        retry = (retry * 2).min(retry_max);
    }
}

async fn sync_one<C: Connector>(
    worker: &mut Pooled<C>,
    ctx: &Ctx,
    row: &MailboxRow,
    dirty: &Arc<AtomicBool>,
) -> Result<Progress> {
    let flag = dirty.clone();
    let inbox = row.is_inbox();
    let interrupt = move || !inbox && flag.load(Ordering::SeqCst);
    sync::sync_mailbox(&mut **worker, ctx, row, &interrupt).await
}

/// Runs until an error: initial sync, then INBOX on IDLE wake-ups, polls
/// every `poll_interval`, and the text pass when there is nothing else.
async fn account_loop<C: Connector>(
    config: &EngineConfig,
    pool: &Pool<C>,
    ctx: &Ctx,
    status: &Status,
    retry: &mut Duration,
) -> Result<()> {
    let mut worker = pool.acquire().await?;
    let caps = worker.capabilities().clone();
    update(status, |s| s.capabilities = capability_names(&caps));
    let dirty = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Notify::new());
    let idle_inline = caps.idle() && pool.max_connections() == 1;
    let _idle = (caps.idle() && pool.max_connections() >= 2).then(|| {
        AbortOnDrop(tokio::spawn(idle_loop(
            pool.clone(),
            dirty.clone(),
            wake.clone(),
            config.retry_min,
            config.retry_max,
        )))
    });

    let mut rows = sync::list(&mut *worker, ctx).await?;
    *retry = config.retry_min;
    let mut next_poll = Instant::now() + config.poll_interval;
    let mut text_skip = BTreeSet::new();
    let mut settled = false;
    loop {
        let inbox_live = rows
            .iter()
            .find(|r| r.is_inbox() && r.state == SyncState::Live)
            .cloned();
        if let Some(inbox) = inbox_live
            && dirty.swap(false, Ordering::SeqCst)
        {
            sync_one(&mut worker, ctx, &inbox, &dirty).await?;
            rows = sync::ordered(ctx).await?;
            continue;
        }
        if let Some(row) = rows.iter().find(|r| r.state != SyncState::Live).cloned() {
            settled = false;
            sync_one(&mut worker, ctx, &row, &dirty).await?;
            rows = sync::ordered(ctx).await?;
            continue;
        }
        if Instant::now() >= next_poll {
            rows = sync::list(&mut *worker, ctx).await?;
            for row in rows.clone() {
                sync_one(&mut worker, ctx, &row, &dirty).await?;
            }
            sync::purge(ctx).await?;
            rows = sync::ordered(ctx).await?;
            next_poll = Instant::now() + config.poll_interval;
            text_skip.clear();
            continue;
        }
        if sync::text_pass(&mut *worker, ctx, config.text_batch, &mut text_skip).await? > 0 {
            continue;
        }
        if !settled {
            settled = true;
            ctx.cache.shrink_memory().await?;
        }
        update(status, |s| {
            s.last_error = None;
            s.degraded = degraded(&caps, &rows);
        });

        if idle_inline {
            worker
                .select(
                    "INBOX",
                    SelectOptions {
                        read_only: true,
                        qresync: None,
                    },
                )
                .await?;
            let deadline = Arc::new(Notify::new());
            let timer = {
                let deadline = deadline.clone();
                AbortOnDrop(tokio::spawn(async move {
                    tokio::time::sleep_until(next_poll).await;
                    deadline.notify_one();
                }))
            };
            let r = worker.idle(&deadline).await?;
            drop(timer);
            worker.take_events();
            if r.stop == IdleStop::Changed {
                dirty.store(true, Ordering::SeqCst);
            }
        } else {
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep_until(next_poll) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use tagmail_imap::Flag;
    use tagmail_store::MasterKey;
    use tempfile::TempDir;

    use super::*;
    use crate::fake::{FakeConnector, FakeMsg, Shared, lock, server};
    use crate::testutil::{TestCache, open};

    const CAPS: &[&str] = &["IMAP4rev1", "ENABLE", "IDLE", "CONDSTORE", "QRESYNC"];

    struct Fixture {
        engine: Engine<FakeConnector>,
        cache: TestCache,
        _dir: TempDir,
    }

    async fn fixture(srv: &Shared, max_connections: u8, poll: Duration) -> Fixture {
        let dir = TempDir::new().unwrap();
        let config = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
        let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
        let sealed = vault.seal(AccountId(1), "secret").unwrap();
        config
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce, max_connections) \
                     VALUES (1, 'a', 'imap', 993, 'tls', 'smtp', 465, 'tls', 'u', ?1, ?2, ?3)",
                    tagmail_store::rusqlite::params![
                        sealed.ciphertext,
                        sealed.nonce,
                        max_connections
                    ],
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
        };
        let config = EngineConfig {
            poll_interval: poll,
            retry_min: Duration::from_millis(50),
            retry_max: Duration::from_millis(200),
            pool: PoolConfig {
                auth_backoff_base: Duration::from_secs(60),
                ..PoolConfig::default()
            },
            ..EngineConfig::default()
        };
        Fixture {
            engine: Engine::new(stores, FakeConnector(srv.clone()), config),
            cache,
            _dir: dir,
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

    async fn has(t: &TestCache, msgid: &'static str) -> bool {
        let rows = t
            .query(
                "SELECT m.msgid_hdr FROM message m JOIN location l ON l.message_id = m.id",
                |r| r.get::<_, String>(0),
            )
            .await;
        rows.iter().any(|m| m == msgid)
    }

    async fn all_live(t: &TestCache) -> bool {
        let states = t
            .query("SELECT sync_state FROM mailbox", |r| r.get::<_, String>(0))
            .await;
        !states.is_empty() && states.iter().all(|s| s == "live")
    }

    #[tokio::test]
    async fn new_account_syncs_and_follows_idle() {
        for max_connections in [2, 1] {
            let srv = server(CAPS);
            lock(&srv).append(
                "INBOX",
                FakeMsg::new("a1@x", "Hi", "ann@x", "me@x", 1_790_000_000),
            );
            let f = fixture(&srv, max_connections, Duration::from_secs(3600)).await;
            f.engine.account_added(AccountId(1));
            eventually("initial sync", || all_live(&f.cache)).await;
            assert!(has(&f.cache, "a1@x").await);

            let status = f.engine.runtime_status(AccountId(1)).unwrap();
            assert!(
                status.capabilities.contains(&"QRESYNC".to_owned()),
                "{status:?}"
            );
            assert_eq!(f.engine.runtime_status(AccountId(2)), None);

            lock(&srv).append(
                "INBOX",
                FakeMsg::new("n1@x", "New", "bob@x", "me@x", 1_790_000_100),
            );
            eventually("IDLE delivery", || has(&f.cache, "n1@x")).await;

            lock(&srv).set_flags("INBOX", 1, &[Flag::Flagged]);
            eventually("flag change", || async {
                f.cache
                    .query(
                        "SELECT starred FROM thread WHERE subject_norm = 'hi'",
                        |r| r.get::<_, bool>(0),
                    )
                    .await
                    == [true]
            })
            .await;
            assert!(
                lock(&srv).logins <= usize::from(max_connections),
                "connections reused"
            );
            f.engine.shutdown().await;
        }
    }

    #[tokio::test]
    async fn other_mailboxes_follow_polls() {
        let srv = server(CAPS);
        let f = fixture(&srv, 2, Duration::from_millis(200)).await;
        f.engine.start_all().await.unwrap();
        eventually("initial sync", || all_live(&f.cache)).await;
        let uid = lock(&srv).append(
            "Archive",
            FakeMsg::new("p1@x", "Old", "cat@x", "me@x", 1_790_000_200),
        );
        eventually("poll delivery", || has(&f.cache, "p1@x")).await;
        lock(&srv).move_msg("Archive", uid, "Sent");
        eventually("move", || async {
            f.cache
                .query(
                    "SELECT mb.name FROM location l JOIN mailbox mb ON mb.id = l.mailbox_id",
                    |r| r.get::<_, String>(0),
                )
                .await
                == ["Sent"]
        })
        .await;
        f.engine.shutdown().await;
    }

    #[tokio::test]
    async fn auth_failure_backs_off_and_shows_in_status() {
        let srv = server(CAPS);
        lock(&srv).reject_login = true;
        let f = fixture(&srv, 2, Duration::from_secs(3600)).await;
        assert_eq!(f.engine.start_all().await.unwrap(), 1);
        eventually("backoff status", || async {
            f.engine
                .runtime_status(AccountId(1))
                .and_then(|s| s.last_error)
                .is_some_and(|e| e.contains("authentication failed") && e.contains("retrying"))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lock(&srv).logins, 1, "no login retries during the backoff");
        f.engine.shutdown().await;
    }
}
