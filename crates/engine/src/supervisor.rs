//! One task per account (plan §3.4 connection budget): a worker connection
//! runs initial sync, polls and the text pass; a second connection IDLEs on
//! INBOX and wakes the worker. With a budget of one, the worker IDLEs
//! itself between polls. Failures back off and show in the runtime status.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tagmail_api_core::dto::{AccountTestDto, LabelDto, TestStepDto, TlsMode};
use tagmail_api_core::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelPatchDto, NewLabelDto, OpDto, OpStatusDto,
};
use tagmail_api_core::{
    AccountRegistry, BoxFuture, ChangeFeed, FetchRawError, NewAccount, OpRequest, RuntimeStatus,
    Submitted, WriteResult, WriteService,
};
use tagmail_core::{AccountId, MessageRowId};
use tagmail_imap::{
    Capabilities, CapabilityOverride, Connector, IdleStop, ImapSession, Password, Pool, PoolConfig,
    Pooled, Security, SelectOptions, ServerConfig,
};
use tagmail_store::rusqlite::OptionalExtension;
use tagmail_store::{
    BlobStore, CacheDb, ChangeOp, ConfigDb, DEFAULT_RAW_CAP, Entity, Sealed, Vault,
};
use tokio::sync::{Notify, Semaphore, SemaphorePermit, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cache::{self, MailboxRow, SyncState};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::executor;
use crate::jobs::{JobFuture, Jobs, Priority, erase};
use crate::overlay::Overlays;
use crate::probe::{self, ProbeState};
use crate::raw;
use crate::smtp::{LettreSender, SmtpSender, SmtpServer};
use crate::sync::{self, Progress};

/// Compressed size of the raw message cache before LRU eviction.
pub const DEFAULT_RAW_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
/// IMAP connections of all accounts together.
pub const DEFAULT_MAX_TOTAL_CONNECTIONS: usize = 8;
/// Accounts running a bulk step (initial-sync window, text batch) at once.
pub const DEFAULT_BULK_PERMITS: usize = 1;
/// Delay between the starts of two accounts at startup.
pub const DEFAULT_START_STAGGER: Duration = Duration::from_secs(2);
/// Share of `start_stagger` added at random to each delayed start.
const STAGGER_JITTER: f64 = 0.5;
/// Cache messages deleted per write transaction when an account is removed.
const PURGE_CHUNK: usize = 1000;

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
    /// A draft is saved to IMAP once it has not been edited for this long;
    /// a changed attachment list or an explicit save saves it at once.
    pub draft_debounce: Duration,
    /// How long Sent is watched after the first send of an account whose
    /// server may file sent mail itself.
    pub sent_probe: Duration,
    /// Outbox and draft-save retry backoff, doubled up to the max.
    pub outbox_retry_min: Duration,
    pub outbox_retry_max: Duration,
    /// SMTP attempts before an item fails for good.
    pub max_send_attempts: u32,
    /// Uploads not attached to a draft are removed after this long.
    pub upload_grace: Duration,
    /// Sent outbox items stay listed this long.
    pub sent_keep: Duration,
    /// Connection budget shared by all accounts: an account starting gets
    /// its `max_connections`, at most what the running accounts left, and
    /// at least one.
    pub max_total_connections: usize,
    /// Bulk steps of all accounts that may run at once; INBOX on IDLE,
    /// polls and jobs never wait for one.
    pub bulk_permits: usize,
    /// `start_all` starts the n-th account after n times this, plus jitter.
    pub start_stagger: Duration,
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
            draft_debounce: Duration::from_secs(30),
            sent_probe: Duration::from_secs(10),
            outbox_retry_min: Duration::from_secs(30),
            outbox_retry_max: Duration::from_secs(60 * 60),
            max_send_attempts: 5,
            upload_grace: Duration::from_secs(24 * 60 * 60),
            sent_keep: Duration::from_secs(60 * 60),
            max_total_connections: DEFAULT_MAX_TOTAL_CONNECTIONS,
            bulk_permits: DEFAULT_BULK_PERMITS,
            start_stagger: DEFAULT_START_STAGGER,
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
    /// Attachments of drafts, `<id>` files plus `tmp/` for uploads in
    /// progress; durable like `config.db`.
    pub uploads: PathBuf,
    /// Queued outgoing messages, `<id>.eml` plus `tmp/` for messages being
    /// built; durable like `config.db`.
    pub outbox: PathBuf,
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

/// The connection caps of the running accounts.
#[derive(Debug)]
struct Budget {
    total: usize,
    caps: Mutex<HashMap<AccountId, usize>>,
}

impl Budget {
    /// Reserves `wanted` connections for `id`: at most what the other
    /// accounts left, at least one. Released when the guard drops.
    fn reserve(self: &Arc<Self>, id: AccountId, wanted: usize) -> Reserved {
        let mut caps = self.caps.lock().unwrap_or_else(PoisonError::into_inner);
        caps.remove(&id);
        let free = self.total.saturating_sub(caps.values().sum()).max(1);
        let cap = wanted.clamp(1, free);
        if cap < wanted {
            tracing::info!(
                account = id.0,
                wanted,
                cap,
                "connection cap clamped to the budget"
            );
        }
        caps.insert(id, cap);
        Reserved {
            budget: self.clone(),
            id,
            cap,
        }
    }
}

struct Reserved {
    budget: Arc<Budget>,
    id: AccountId,
    cap: usize,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.budget
            .caps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// What `config.db` says about an account beyond its settings.
#[derive(Debug, Clone, Default)]
struct AccountFacts {
    email: Option<String>,
    probe: Option<ProbeState>,
}

impl AccountFacts {
    async fn load(config: &ConfigDb, id: AccountId) -> Result<AccountFacts> {
        let row = config
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT email, keyword_probe FROM account WHERE id = ?1",
                    [id.0],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()?)
            })
            .await?;
        let (email, probe) = row.ok_or(Error::NoAccount(id.0))?;
        Ok(AccountFacts {
            email: email.filter(|e| !e.is_empty()),
            probe: probe.as_deref().and_then(ProbeState::parse),
        })
    }

    /// The degraded codes that do not come from the server's capabilities.
    fn degraded(&self, labels_dropped: bool) -> Vec<String> {
        let mut out = Vec::new();
        if labels_dropped || self.probe == Some(ProbeState::Dropped) {
            out.push("labels_degraded".to_owned());
        }
        if self.email.is_none() {
            out.push("no_sender_address".to_owned());
        }
        out
    }
}

/// Known once the account task has read them.
type Facts = Arc<Mutex<Option<AccountFacts>>>;

struct Handle<S> {
    status: Status,
    facts: Facts,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    executor: JoinHandle<()>,
    jobs: Arc<Jobs<S>>,
}

pub(crate) struct Inner<F: ConnectorFactory> {
    pub(crate) config: EngineConfig,
    factory: F,
    pub(crate) stores: Stores,
    accounts: Mutex<HashMap<AccountId, Handle<Session<F>>>>,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) smtp: Arc<dyn SmtpSender>,
    /// Wakes the compose runner.
    pub(crate) outbox_wake: Notify,
    runner: Mutex<Option<(watch::Sender<bool>, JoinHandle<()>)>>,
    /// Open ops of every account, projected over cache writes.
    pub(crate) overlays: Arc<Overlays>,
    /// Wakes an account's op executor.
    pub(crate) wakes: Mutex<HashMap<AccountId, Arc<Notify>>>,
    /// The last failure of each label job.
    pub(crate) job_errors: Mutex<HashMap<i64, String>>,
    budget: Arc<Budget>,
    bulk: Semaphore,
    /// Accounts whose server answered OK to a label but did not keep its
    /// keyword; cleared by the next label it keeps.
    labels_dropped: Mutex<HashSet<AccountId>>,
    /// Blob writes of every account; stops wait for them to finish.
    pub(crate) blob_writes: Arc<Semaphore>,
}

/// The sync engine: implements [`AccountRegistry`] and [`WriteService`]
/// for the API.
pub struct Engine<F: ConnectorFactory = ImapConnectorFactory> {
    pub(crate) inner: Arc<Inner<F>>,
}

impl<F: ConnectorFactory> Clone for Engine<F> {
    fn clone(&self) -> Self {
        Engine {
            inner: self.inner.clone(),
        }
    }
}

impl<F: ConnectorFactory> Engine<F> {
    /// Must be called inside a tokio runtime, which runs the account tasks.
    pub fn new(stores: Stores, factory: F, config: EngineConfig) -> Engine<F> {
        Engine::with_smtp(stores, factory, config, Arc::new(LettreSender::default()))
    }

    /// Like [`Engine::new`], submitting mail through `smtp`. Starts the
    /// compose runner (drafts, outbox).
    pub fn with_smtp(
        stores: Stores,
        factory: F,
        config: EngineConfig,
        smtp: Arc<dyn SmtpSender>,
    ) -> Engine<F> {
        for dir in [
            stores.uploads.join(crate::compose::UPLOAD_TMP),
            stores.outbox.join(crate::compose::OUTBOX_TMP),
        ] {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::error!(error = %e, dir = %dir.display(), "cannot create a data directory");
            }
        }
        let engine = Engine {
            inner: Arc::new(Inner {
                factory,
                stores,
                accounts: Mutex::new(HashMap::new()),
                runtime: tokio::runtime::Handle::current(),
                smtp,
                outbox_wake: Notify::new(),
                runner: Mutex::new(None),
                overlays: Arc::default(),
                wakes: Mutex::new(HashMap::new()),
                job_errors: Mutex::new(HashMap::new()),
                budget: Arc::new(Budget {
                    total: config.max_total_connections,
                    caps: Mutex::new(HashMap::new()),
                }),
                bulk: Semaphore::new(config.bulk_permits.max(1)),
                labels_dropped: Mutex::new(HashSet::new()),
                blob_writes: crate::ctx::blob_writes(),
                config,
            }),
        };
        let (stop, stopped) = watch::channel(false);
        let task = engine
            .inner
            .runtime
            .spawn(crate::outbox::run(engine.clone(), stopped));
        *engine
            .inner
            .runner
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((stop, task));
        engine
    }

    /// Syncs every mailbox of the account soon, e.g. after a Sent copy.
    pub(crate) fn request_poll(&self, account: AccountId) {
        let accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = accounts.get(&account) {
            h.jobs.poll_now.store(true, Ordering::SeqCst);
            h.jobs.wake.notify_one();
        }
    }

    /// Restores the pending overlay, then starts every `active` account in
    /// `config.db`, staggered; returns how many.
    pub async fn start_all(&self) -> Result<usize> {
        self.restore_overlays().await?;
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
        let stagger = self.inner.config.start_stagger;
        for (n, &id) in ids.iter().enumerate() {
            let delay = if n == 0 {
                Duration::ZERO
            } else {
                stagger.saturating_mul(u32::try_from(n).unwrap_or(u32::MAX))
                    + stagger.mul_f64(random_fraction() * STAGGER_JITTER)
            };
            self.start_after(AccountId(id), delay);
        }
        Ok(ids.len())
    }

    /// Starts syncing an account unless it already runs.
    pub fn start(&self, id: AccountId) {
        self.start_after(id, Duration::ZERO);
    }

    fn start_after(&self, id: AccountId, delay: Duration) {
        let mut accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if accounts.get(&id).is_some_and(|h| !h.task.is_finished()) {
            return;
        }
        let status = Status::default();
        let facts = Facts::default();
        let (stop, stopped) = watch::channel(false);
        let jobs = Arc::new(Jobs::new());
        let executor = self
            .inner
            .runtime
            .spawn(executor::run(self.clone(), id, stopped.clone()));
        let task = self.inner.runtime.spawn(run(
            self.inner.clone(),
            id,
            status.clone(),
            facts.clone(),
            stopped,
            jobs.clone(),
            delay,
        ));
        accounts.insert(
            id,
            Handle {
                status,
                facts,
                stop,
                task,
                executor,
                jobs,
            },
        );
        drop(accounts);
        self.mirror_registry(id);
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

    /// Records whether the account's server keeps labels, after an op
    /// batch dropped one or added some.
    pub(crate) fn record_label_health(
        &self,
        account: AccountId,
        dropped: Option<String>,
        kept: bool,
    ) {
        let mut accounts = self
            .inner
            .labels_dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(label) = dropped {
            tracing::warn!(
                account = account.0,
                label,
                "the server did not keep a label"
            );
            accounts.insert(account);
        } else if kept {
            accounts.remove(&account);
        }
    }

    /// Stops all accounts and waits for their tasks.
    pub async fn shutdown(&self) {
        let runner = self
            .inner
            .runner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((stop, task)) = runner {
            stop.send_replace(true);
            if let Err(e) = task.await
                && e.is_panic()
            {
                tracing::error!("compose runner panicked");
            }
        }
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
            join(h).await;
        }
        crate::ctx::drain_blob_writes(&self.inner.blob_writes).await;
    }

    #[cfg(test)]
    pub(crate) fn connection_caps(&self) -> Vec<(i64, usize)> {
        self.inner
            .budget
            .caps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(a, c)| (a.0, *c))
            .collect()
    }

    /// Stops the account's sync task and op executor and waits for both.
    /// Its pending ops stay queued.
    pub async fn stop_account(&self, id: AccountId) {
        let handle = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(h) = handle {
            h.stop.send_replace(true);
            join(h).await;
        }
        crate::ctx::drain_blob_writes(&self.inner.blob_writes).await;
        self.inner
            .wakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// Stops the account and starts it again with its current settings if
    /// it is `active`; a changed password logs in at once.
    pub async fn restart_account(&self, id: AccountId) -> Result<()> {
        self.stop_account(id).await;
        let active = self
            .inner
            .stores
            .config
            .write(move |tx| {
                tx.execute(
                    "UPDATE account SET auth_backoff_until = NULL WHERE id = ?1",
                    [id.0],
                )?;
                Ok(tx
                    .query_row("SELECT status FROM account WHERE id = ?1", [id.0], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?
                    .is_some_and(|s| s == "active"))
            })
            .await?;
        if active {
            self.start(id);
        }
        Ok(())
    }

    /// Stops the account, deletes its cache rows in chunks with their
    /// blobs, then its config rows, and emits the change events: one bulk
    /// per mailbox, then the account's delete.
    pub async fn remove_account(&self, id: AccountId) -> Result<()> {
        self.stop_account(id).await;
        self.inner.overlays.remove_account(id);
        self.inner
            .labels_dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        let ctx = self.ctx(id);
        loop {
            let (purged, done) = ctx.write(|w| cache::purge_account(w, PURGE_CHUNK)).await?;
            sync::delete_blobs(&ctx, purged).await?;
            if done {
                break;
            }
        }
        self.inner
            .stores
            .config
            .write(move |tx| {
                tx.execute("DELETE FROM op_seen WHERE account_id = ?1", [id.0])?;
                tx.execute("DELETE FROM account WHERE id = ?1", [id.0])?;
                Ok(())
            })
            .await?;
        ctx.write(move |w| {
            w.emit(Entity::Account, id.0, ChangeOp::Delete, None);
            Ok(())
        })
        .await
    }

    /// Re-reads the sender address and probe result of a running account.
    async fn refresh_facts(&self, id: AccountId) -> Result<()> {
        let known = AccountFacts::load(&self.inner.stores.config, id).await?;
        let accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = accounts.get(&id) {
            *h.facts.lock().unwrap_or_else(PoisonError::into_inner) = Some(known);
        }
        Ok(())
    }

    /// Logs in to IMAP (CAPABILITY, LIST) and SMTP with unsaved settings.
    async fn test_settings(&self, settings: NewAccount) -> AccountTestDto {
        let imap_settings = AccountSettings {
            id: AccountId(0),
            host: settings.imap_host.clone(),
            port: settings.imap_port,
            security: security(settings.imap_tls),
            username: settings.username.clone(),
            password: Password::new(settings.password.expose()),
            max_connections: 1,
        };
        let connector = self.inner.factory.connector(&imap_settings);
        let started = Instant::now();
        let imap = tokio::time::timeout(TEST_TIMEOUT, async {
            let mut s = connector.connect().await?;
            let caps = capability_names(s.capabilities());
            let mailboxes = s.list().await?.len();
            let _ = s.logout().await;
            Ok::<_, tagmail_imap::Error>((caps, mailboxes))
        })
        .await;
        let imap_ms = elapsed_ms(started);
        let (imap, capabilities, mailboxes) = match imap {
            Ok(Ok((caps, n))) => (step(None, imap_ms), caps, u32::try_from(n).ok()),
            Ok(Err(e)) => (step(Some(e.to_string()), imap_ms), Vec::new(), None),
            Err(_) => (step(Some(TIMED_OUT.to_owned()), imap_ms), Vec::new(), None),
        };
        let server = SmtpServer {
            host: settings.smtp_host,
            port: settings.smtp_port,
            security: security(settings.smtp_tls),
            username: settings.username,
            password: Password::new(settings.password.expose()),
        };
        let started = Instant::now();
        let smtp = tokio::time::timeout(TEST_TIMEOUT, self.inner.smtp.check(server)).await;
        let smtp_ms = elapsed_ms(started);
        let smtp = match smtp {
            Ok(Ok(())) => step(None, smtp_ms),
            Ok(Err(e)) => step(Some(e.message), smtp_ms),
            Err(_) => step(Some(TIMED_OUT.to_owned()), smtp_ms),
        };
        AccountTestDto {
            imap,
            smtp,
            capabilities,
            mailboxes,
        }
    }
}

/// Each step of an account test gives up after this long.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);
const TIMED_OUT: &str = "the server did not answer in time";

fn security(mode: TlsMode) -> Security {
    match mode {
        TlsMode::Tls => Security::Tls,
        TlsMode::Starttls => Security::StartTls,
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn step(error: Option<String>, ms: u64) -> TestStepDto {
    TestStepDto {
        ok: error.is_none(),
        error,
        ms,
    }
}

async fn join<S>(h: Handle<S>) {
    if let Err(e) = h.task.await
        && e.is_panic()
    {
        tracing::error!("account task panicked");
    }
    if let Err(e) = h.executor.await
        && e.is_panic()
    {
        tracing::error!("op executor panicked");
    }
}

impl<F: ConnectorFactory> WriteService for Engine<F> {
    fn submit_op(&self, req: OpRequest) -> BoxFuture<WriteResult<Submitted>> {
        let engine = self.clone();
        Box::pin(async move { engine.queue_op(req).await })
    }

    fn ops(&self, status: Option<OpStatusDto>) -> BoxFuture<WriteResult<Vec<OpDto>>> {
        let engine = self.clone();
        Box::pin(async move { engine.list_ops(status).await })
    }

    fn op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.op_dto(op_id).await })
    }

    fn retry_op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.retry_failed_op(op_id).await })
    }

    fn discard_op(&self, op_id: String) -> BoxFuture<WriteResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.discard_op_id(op_id).await })
    }

    fn create_label(&self, label: NewLabelDto) -> BoxFuture<WriteResult<LabelDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.create_label(label).await })
    }

    fn update_label(
        &self,
        key: String,
        patch: LabelPatchDto,
    ) -> BoxFuture<WriteResult<LabelChangeDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.update_label(key, patch).await })
    }

    fn delete_label(&self, key: String) -> BoxFuture<WriteResult<LabelChangeDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.delete_label(key).await })
    }

    fn label_jobs(&self) -> BoxFuture<WriteResult<Vec<LabelJobDto>>> {
        let engine = self.clone();
        Box::pin(async move { engine.label_job_dtos().await })
    }
}

impl<F: ConnectorFactory> AccountRegistry for Engine<F> {
    fn account_added(&self, id: AccountId) {
        self.start(id);
    }

    fn account_updated(&self, id: AccountId, restart: bool) {
        let engine = self.clone();
        self.inner.runtime.spawn(async move {
            let result = if restart {
                engine.restart_account(id).await
            } else {
                engine.refresh_facts(id).await
            };
            if let Err(e) = result {
                tracing::error!(account = id.0, error = %e, "cannot apply the account change");
            }
        });
    }

    fn account_removed(&self, id: AccountId) -> BoxFuture<()> {
        let engine = self.clone();
        Box::pin(async move {
            if let Err(e) = engine.remove_account(id).await {
                tracing::error!(account = id.0, error = %e, "cannot remove the account");
            }
        })
    }

    fn test_account(&self, settings: NewAccount) -> BoxFuture<AccountTestDto> {
        let engine = self.clone();
        Box::pin(async move { engine.test_settings(settings).await })
    }

    fn runtime_status(&self, id: AccountId) -> Option<RuntimeStatus> {
        let accounts = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let handle = accounts.get(&id)?;
        let mut status = handle
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let dropped = self
            .inner
            .labels_dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id);
        if let Some(facts) = handle
            .facts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            status.degraded.extend(facts.degraded(dropped));
        }
        Some(status)
    }

    fn fetch_raw(
        &self,
        account: AccountId,
        message: MessageRowId,
    ) -> BoxFuture<Result<(), FetchRawError>> {
        let engine = self.clone();
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

/// A random number in [0, 1).
fn random_fraction() -> f64 {
    let r = RandomState::new().hash_one(Instant::now()) % 1000;
    r as f64 / 1000.0
}

/// `base` scaled by a random factor in [0.5, 1.5).
fn jitter(base: Duration) -> Duration {
    base.mul_f64(0.5 + random_fraction())
}

async fn run<F: ConnectorFactory>(
    inner: Arc<Inner<F>>,
    id: AccountId,
    status: Status,
    facts: Facts,
    stop: watch::Receiver<bool>,
    jobs: Arc<Jobs<Session<F>>>,
    delay: Duration,
) {
    run_account(inner, id, status, facts, stop, &jobs, delay).await;
    jobs.refuse_all(NOT_SYNCING).await;
}

/// Waits until `until` while refusing queued jobs with `why`; `false` when
/// stopped.
async fn wait_refusing<S>(
    until: Instant,
    stop: &mut watch::Receiver<bool>,
    jobs: &Jobs<S>,
    why: impl Fn() -> String,
) -> bool {
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return true,
            _ = stop.wait_for(|s| *s) => return false,
            () = jobs.wake.notified() => {}
        }
        jobs.refuse_all(&why()).await;
    }
}

fn last_error(status: &Status, fallback: &str) -> String {
    status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last_error
        .clone()
        .unwrap_or_else(|| fallback.to_owned())
}

/// `account.auth_backoff_until`, so a restart does not log in again
/// before the backoff ends.
struct PersistedBackoff {
    config: Arc<ConfigDb>,
    id: AccountId,
    until: Option<i64>,
}

impl PersistedBackoff {
    async fn load(config: Arc<ConfigDb>, id: AccountId) -> Result<PersistedBackoff> {
        let until = config
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT auth_backoff_until FROM account WHERE id = ?1",
                    [id.0],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
            })
            .await?;
        Ok(PersistedBackoff { config, id, until })
    }

    fn remaining(&self) -> Option<Duration> {
        let left = self.until? - executor::now_secs();
        (left > 0).then(|| Duration::from_secs(left.unsigned_abs()))
    }

    async fn write(&mut self, until: Option<i64>) {
        self.until = until;
        let id = self.id.0;
        let result = self
            .config
            .write(move |tx| {
                tx.execute(
                    "UPDATE account SET auth_backoff_until = ?2 WHERE id = ?1",
                    tagmail_store::rusqlite::params![id, until],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(account = id, error = %e, "cannot record the authentication backoff");
        }
    }

    async fn set(&mut self, wait: Duration) {
        let secs = i64::try_from(wait.as_secs()).unwrap_or(i64::MAX);
        self.write(Some(executor::now_secs().saturating_add(secs)))
            .await;
    }

    async fn clear(&mut self) {
        if self.until.is_some() {
            self.write(None).await;
        }
    }
}

async fn run_account<F: ConnectorFactory>(
    inner: Arc<Inner<F>>,
    id: AccountId,
    status: Status,
    facts: Facts,
    mut stop: watch::Receiver<bool>,
    jobs: &Jobs<Session<F>>,
    delay: Duration,
) {
    if !delay.is_zero()
        && !wait_refusing(Instant::now() + delay, &mut stop, jobs, || {
            "the account is starting".to_owned()
        })
        .await
    {
        return;
    }
    let config = &inner.stores.config;
    let loaded = async {
        let settings = load_account(&inner.stores, id).await?;
        let backoff = PersistedBackoff::load(config.clone(), id).await?;
        let known = AccountFacts::load(config, id).await?;
        Ok::<_, Error>((settings, backoff, known))
    };
    let (settings, mut backoff) = match loaded.await {
        Ok((settings, backoff, known)) => {
            *facts.lock().unwrap_or_else(PoisonError::into_inner) = Some(known);
            (settings, backoff)
        }
        Err(e) => {
            tracing::error!(account = id.0, error = %e, "cannot load account");
            update(&status, |s| s.last_error = Some(e.to_string()));
            return;
        }
    };
    if let Some(wait) = backoff.remaining() {
        update(&status, |s| {
            s.last_error = Some(format!(
                "authentication failed; retrying in {}s",
                wait.as_secs()
            ));
        });
        let why = || last_error(&status, "backing off");
        if !wait_refusing(Instant::now() + wait, &mut stop, jobs, why).await {
            return;
        }
    }
    let reserved = inner.budget.reserve(id, settings.max_connections);
    let pool = Pool::new(
        inner.factory.connector(&settings),
        PoolConfig {
            max_connections: reserved.cap,
            ..inner.config.pool.clone()
        },
    );
    let stores = &inner.stores;
    let ctx = Ctx::new(
        stores.cache.clone(),
        stores.blobs.clone(),
        stores.feed.clone(),
        id,
    )
    .with_overlays(inner.overlays.clone())
    .with_blob_writes(inner.blob_writes.clone());
    let account = AccountRun {
        id,
        config: &inner.config,
        config_db: config,
        facts: &facts,
        pool: &pool,
        ctx: &ctx,
        status: &status,
        jobs,
        bulk: &inner.bulk,
    };
    let mut retry = inner.config.retry_min;
    loop {
        let result = tokio::select! {
            r = account_loop(&account, &mut retry, &mut backoff) => r,
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
                backoff.set(retry_in).await;
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
        let why = || last_error(&status, "reconnecting");
        if !wait_refusing(Instant::now() + wait, &mut stop, jobs, why).await {
            return;
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

/// What an account's worker loop works with.
struct AccountRun<'a, C: Connector> {
    id: AccountId,
    config: &'a EngineConfig,
    config_db: &'a ConfigDb,
    facts: &'a Facts,
    pool: &'a Pool<C>,
    ctx: &'a Ctx,
    status: &'a Status,
    jobs: &'a Jobs<C::Session>,
    bulk: &'a Semaphore,
}

fn probe_state(facts: &Facts) -> Option<ProbeState> {
    facts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|f| f.probe)
}

async fn record_probe<C: Connector>(run: &AccountRun<'_, C>, state: ProbeState) -> Result<()> {
    let id = run.id.0;
    run.config_db
        .write(move |tx| {
            tx.execute(
                "UPDATE account SET keyword_probe = ?2, keyword_probe_at = ?3 WHERE id = ?1",
                tagmail_store::rusqlite::params![id, state.as_str(), executor::now_secs()],
            )?;
            Ok(())
        })
        .await?;
    tracing::info!(account = id, result = state.as_str(), "keyword probe");
    if let Some(f) = run
        .facts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        f.probe = Some(state);
    }
    Ok(())
}

/// A bulk permit, or `None` when INBOX, a job or the poll timer needs the
/// worker first.
async fn bulk_permit<'s, S>(
    bulk: &'s Semaphore,
    wake: &Notify,
    jobs: &Jobs<S>,
    next_poll: Instant,
) -> Option<SemaphorePermit<'s>> {
    tokio::select! {
        permit = bulk.acquire() => permit.ok(),
        () = wake.notified() => None,
        () = jobs.wake.notified() => None,
        () = tokio::time::sleep_until(next_poll) => None,
    }
}

/// Runs until an error: initial sync, then INBOX on IDLE wake-ups, polls
/// every `poll_interval`, and the text pass when there is nothing else.
/// Initial-sync windows of other mailboxes and text batches each hold a
/// bulk permit.
async fn account_loop<C: Connector>(
    run: &AccountRun<'_, C>,
    retry: &mut Duration,
    backoff: &mut PersistedBackoff,
) -> Result<()> {
    let AccountRun {
        config,
        pool,
        ctx,
        status,
        jobs,
        bulk,
        ..
    } = *run;
    let mut worker = pool.acquire().await?;
    backoff.clear().await;
    if ProbeState::needed(probe_state(run.facts)) {
        let (probed, state) = probe::run(pool, worker).await?;
        worker = probed;
        if let Some(state) = state {
            record_probe(run, state).await?;
        }
    }
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
        if jobs.poll_now.swap(false, Ordering::SeqCst) {
            next_poll = Instant::now();
        }
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
            let Some(_permit) = bulk_permit(bulk, &wake, jobs, next_poll).await else {
                continue;
            };
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
        if sync::text_pending(ctx, &text_skip).await? {
            let Some(_permit) = bulk_permit(bulk, &wake, jobs, next_poll).await else {
                continue;
            };
            if sync::text_pass(&mut *worker, ctx, config.text_batch, &mut text_skip).await? > 0 {
                continue;
            }
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
            uploads: dir.path().join("uploads"),
            outbox: dir.path().join("outbox"),
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
            let probe_reconnect = usize::from(max_connections == 1);
            assert!(
                lock(&srv).logins <= usize::from(max_connections) + probe_reconnect,
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
