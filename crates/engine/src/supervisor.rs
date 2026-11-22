//! One task per account (plan §3.4 connection budget): a worker connection
//! runs initial sync, polls and the text pass; a second connection IDLEs on
//! INBOX and wakes the worker. With a budget of one, the worker IDLEs
//! itself between polls. Failures back off and show in the runtime status.

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tagmail_api_core::{AccountRegistry, BoxFuture, ChangeFeed, FetchRawError, RuntimeStatus};
use tagmail_core::{AccountId, MessageRowId};
use tagmail_imap::{
    Capabilities, CapabilityOverride, Connector, IdleStop, ImapSession, Password, Pool, PoolConfig,
    Pooled, Security, SelectOptions, ServerConfig,
};
use tagmail_store::rusqlite::OptionalExtension;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, DEFAULT_RAW_CAP, Sealed, Vault};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cache::{MailboxRow, SyncState};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::jobs::{JobFuture, Jobs, Priority, erase};
use crate::raw;
use crate::sync::{self, Progress};

/// Compressed size of the raw message cache before LRU eviction.
pub const DEFAULT_RAW_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

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
    /// Largest raw message fetched on open.
    pub raw_cap: u64,
    /// The raw cache is trimmed to this many compressed bytes after each
    /// fetch.
    pub raw_cache_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            poll_interval: Duration::from_secs(120),
            text_batch: 100,
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            pool: PoolConfig::default(),
            raw_cap: DEFAULT_RAW_CAP,
            raw_cache_bytes: DEFAULT_RAW_CACHE_BYTES,
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

/// The IMAP session type of a connector factory.
pub(crate) type Session<F> = <<F as ConnectorFactory>::Connector as Connector>::Session;

const NOT_SYNCING: &str = "the account is not syncing";

struct Handle<S> {
    status: Status,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    jobs: Arc<Jobs<S>>,
}

struct Inner<F: ConnectorFactory> {
    config: EngineConfig,
    factory: F,
    stores: Stores,
    accounts: Mutex<HashMap<AccountId, Handle<Session<F>>>>,
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
        let jobs = Arc::new(Jobs::new());
        let task = self.inner.runtime.spawn(run(
            self.inner.clone(),
            id,
            status.clone(),
            stopped,
            jobs.clone(),
        ));
        accounts.insert(
            id,
            Handle {
                status,
                stop,
                task,
                jobs,
            },
        );
    }

    /// Runs `job` on the account's worker connection between sync steps
    /// and returns its result. [`Error::Unavailable`] when the account is
    /// not syncing, is reconnecting or backing off, or its queue is full.
    ///
    /// The job owns the session while it runs: it may select any mailbox,
    /// and must leave the session usable unless it fails with a fatal
    /// error, after which the worker reconnects.
    pub(crate) async fn run_job<T, J>(
        &self,
        account: AccountId,
        priority: Priority,
        job: J,
    ) -> Result<T>
    where
        T: Send + 'static,
        J: for<'a> FnOnce(&'a mut Session<F>, &'a Ctx) -> JobFuture<'a, T> + Send + 'static,
    {
        let jobs = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&account)
            .filter(|h| !h.task.is_finished())
            .map(|h| h.jobs.clone());
        let jobs = jobs.ok_or_else(|| Error::Unavailable(NOT_SYNCING.to_owned()))?;
        let (erased, answer) = erase(job);
        if jobs.submit(priority, erased).is_err() {
            return Err(Error::Unavailable("too much queued IMAP work".to_owned()));
        }
        answer
            .await
            .unwrap_or_else(|_| Err(Error::Unavailable(NOT_SYNCING.to_owned())))
    }

    /// Stops all accounts and waits for their tasks.
    pub async fn shutdown(&self) {
        let handles: Vec<Handle<Session<F>>> = self
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

    fn fetch_raw(
        &self,
        account: AccountId,
        message: MessageRowId,
    ) -> BoxFuture<Result<(), FetchRawError>> {
        let engine = Engine {
            inner: self.inner.clone(),
        };
        let (cap, cache_max) = (self.inner.config.raw_cap, self.inner.config.raw_cache_bytes);
        Box::pin(async move {
            let job = engine.run_job(account, Priority::Interactive, move |s, ctx| {
                Box::pin(raw::fetch_raw(s, ctx, message.0, cap, cache_max))
            });
            match job.await {
                Ok(answer) => answer,
                Err(e) => Err(FetchRawError::Unavailable(e.to_string())),
            }
        })
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
    stop: watch::Receiver<bool>,
    jobs: Arc<Jobs<Session<F>>>,
) {
    run_account(inner, id, status, stop, &jobs).await;
    jobs.refuse_all(NOT_SYNCING).await;
}

async fn run_account<F: ConnectorFactory>(
    inner: Arc<Inner<F>>,
    id: AccountId,
    status: Status,
    mut stop: watch::Receiver<bool>,
    jobs: &Jobs<Session<F>>,
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
            r = account_loop(&inner.config, &pool, &ctx, &status, &mut retry, jobs) => r,
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
        let until = Instant::now() + wait;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(until) => break,
                _ = stop.wait_for(|s| *s) => return,
                () = jobs.wake.notified() => {}
            }
            let why = status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .last_error
                .clone()
                .unwrap_or_else(|| "reconnecting".to_owned());
            jobs.refuse_all(&why).await;
        }
    }
}

/// Serves the queued jobs; fails when one left the session unusable.
async fn serve_jobs<C: Connector>(
    worker: &mut Pooled<C>,
    ctx: &Ctx,
    jobs: &Jobs<C::Session>,
) -> Result<()> {
    jobs.pending.store(false, Ordering::SeqCst);
    while let Some(job) = jobs.pop() {
        job(Ok((&mut **worker, ctx))).await;
        if !worker.is_usable() {
            return Err(Error::Imap(tagmail_imap::Error::Closed { bye: None }));
        }
    }
    Ok(())
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
    pending: &Arc<AtomicBool>,
) -> Result<Progress> {
    let flag = dirty.clone();
    let pending = pending.clone();
    let inbox = row.is_inbox();
    let interrupt =
        move || !inbox && (flag.load(Ordering::SeqCst) || pending.load(Ordering::SeqCst));
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
    jobs: &Jobs<C::Session>,
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
        serve_jobs(&mut worker, ctx, jobs).await?;
        let inbox_live = rows
            .iter()
            .find(|r| r.is_inbox() && r.state == SyncState::Live)
            .cloned();
        if let Some(inbox) = inbox_live
            && dirty.swap(false, Ordering::SeqCst)
        {
            sync_one(&mut worker, ctx, &inbox, &dirty, &jobs.pending).await?;
            rows = sync::ordered(ctx).await?;
            continue;
        }
        if let Some(row) = rows.iter().find(|r| r.state != SyncState::Live).cloned() {
            settled = false;
            sync_one(&mut worker, ctx, &row, &dirty, &jobs.pending).await?;
            rows = sync::ordered(ctx).await?;
            continue;
        }
        if Instant::now() >= next_poll {
            rows = sync::list(&mut *worker, ctx).await?;
            for row in rows.clone() {
                sync_one(&mut worker, ctx, &row, &dirty, &jobs.pending).await?;
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
                let open = jobs.wake.clone();
                AbortOnDrop(tokio::spawn(async move {
                    tokio::select! {
                        () = tokio::time::sleep_until(next_poll) => {}
                        () = open.notified() => {}
                    }
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
                () = jobs.wake.notified() => {}
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
    use crate::fake::{FakeConnector, FakeMsg, FakeSession, Shared, lock, server};
    use crate::testutil::{TestCache, open};

    const CAPS: &[&str] = &["IMAP4rev1", "ENABLE", "IDLE", "CONDSTORE", "QRESYNC"];
    const RAW_CAP: u64 = 4096;

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
            raw_cap: RAW_CAP,
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

    #[tokio::test]
    async fn jobs_run_on_the_worker_connection() {
        let srv = server(CAPS);
        lock(&srv).add_mailbox("Drafts", Some(tagmail_core::SpecialUse::Drafts), 400);
        let f = fixture(&srv, 1, Duration::from_secs(3600)).await;
        fn job<'a>(s: &'a mut FakeSession, _: &'a Ctx) -> JobFuture<'a, u32> {
            Box::pin(async move {
                let raw = b"Message-ID: <d1@x>\r\nSubject: Draft\r\n\r\nHi";
                let uid = s.append("Drafts", &[Flag::Draft], raw).await?.uid.unwrap();
                Ok(uid.0)
            })
        }
        let refused = f.engine.run_job(AccountId(1), Priority::Write, job).await;
        assert!(
            matches!(&refused, Err(Error::Unavailable(why)) if why == NOT_SYNCING),
            "{refused:?}"
        );

        f.engine.start_all().await.unwrap();
        eventually("initial sync", || all_live(&f.cache)).await;
        let uid = tokio::time::timeout(
            Duration::from_secs(10),
            f.engine.run_job(AccountId(1), Priority::Write, job),
        )
        .await
        .expect("served while the worker IDLEs")
        .unwrap();
        assert_eq!(lock(&srv).mailbox("Drafts").msgs[&uid].id, "d1@x");

        let expunged = f
            .engine
            .run_job(AccountId(1), Priority::Write, move |s, _| {
                Box::pin(async move {
                    s.select("Drafts", SelectOptions::default()).await?;
                    let found = s
                        .uid_search(tagmail_imap::SearchCriteria::Header {
                            name: "Message-ID".into(),
                            value: "<d1@x>".into(),
                        })
                        .await?;
                    s.expunge_uids(&found).await?;
                    Ok(found.len())
                })
            })
            .await
            .unwrap();
        assert_eq!(expunged, 1);
        assert!(lock(&srv).mailbox("Drafts").msgs.is_empty());
        f.engine.shutdown().await;
    }

    async fn message_id(t: &TestCache, msgid: &'static str) -> i64 {
        t.query("SELECT id, msgid_hdr FROM message", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .await
        .into_iter()
        .find(|(_, m)| m == msgid)
        .unwrap()
        .0
    }

    #[tokio::test]
    async fn opening_a_message_fetches_its_raw_body() {
        for max_connections in [2, 1] {
            let srv = server(CAPS);
            lock(&srv).append(
                "INBOX",
                FakeMsg::new("o1@x", "Open me", "ann@x", "me@x", 1_790_000_000),
            );
            let mut big = FakeMsg::new("big@x", "Big", "ann@x", "me@x", 1_790_000_001);
            big.body = "x".repeat(RAW_CAP as usize);
            lock(&srv).append("Archive", big);
            let f = fixture(&srv, max_connections, Duration::from_secs(3600)).await;

            assert_eq!(
                f.engine.fetch_raw(AccountId(1), MessageRowId(1)).await,
                Err(FetchRawError::Unavailable(
                    "the account is not syncing".to_owned()
                ))
            );
            f.engine.start_all().await.unwrap();
            eventually("initial sync", || all_live(&f.cache)).await;

            let id = message_id(&f.cache, "o1@x").await;
            let answer = tokio::time::timeout(
                Duration::from_secs(10),
                f.engine.fetch_raw(AccountId(1), MessageRowId(id)),
            )
            .await
            .expect("served while the worker waits or IDLEs");
            assert_eq!(answer, Ok(()));
            let email_id = f
                .cache
                .query("SELECT id, email_id FROM message", |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .await
                .into_iter()
                .find(|(m, _)| *m == id)
                .unwrap()
                .1;
            let key = tagmail_store::BlobKey::for_message(
                AccountId(1),
                &tagmail_core::EmailId::parse(&email_id).unwrap(),
            );
            let mut raw = String::new();
            std::io::Read::read_to_string(
                &mut f.cache.ctx.blobs.open_raw(&key).unwrap().unwrap(),
                &mut raw,
            )
            .unwrap();
            assert!(raw.contains("Subject: Open me\r\n"), "{raw}");
            assert!(raw.ends_with("Body of o1@x"), "{raw}");

            let big = message_id(&f.cache, "big@x").await;
            assert_eq!(
                f.engine.fetch_raw(AccountId(1), MessageRowId(big)).await,
                Err(FetchRawError::TooLarge)
            );
            assert_eq!(
                f.engine.fetch_raw(AccountId(1), MessageRowId(9999)).await,
                Err(FetchRawError::NotFound)
            );
            f.engine.shutdown().await;
        }
    }
}
