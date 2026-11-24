//! Keeps the replica in step with the server: version gate, bootstrap,
//! SSE-driven pulls with a polling fallback, window sweep and raw prefetch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tagmail_api_core::ChangeFeed;
use tagmail_api_core::dto::{ApiVersion, ChangeDto};
use tagmail_api_core::replica_dto::DEFAULT_WINDOW_DAYS;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_core::{CacheEpoch, ChangeCursor};
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, watch};

use crate::bootstrap;
use crate::client::{ClientError, ServerClient};
use crate::db::{self, Expected, Layout, Opened};
use crate::durable::{Binding, Durable};
use crate::error::{Error, Result};
use crate::events::{self, Doorbell};
use crate::import::Arrival;
use crate::pull;
use crate::version::{self, Compat};
use crate::window;

pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The server's `Retry-After` for a busy snapshot slot.
pub const BUSY_RETRY: Duration = Duration::from_secs(30);
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CLOSE_WAIT: Duration = Duration::from_secs(5);
const CLOSE_POLL: Duration = Duration::from_millis(20);
const EVENT_BUFFER: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub window_days: u32,
    pub raw_cap_bytes: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            window_days: DEFAULT_WINDOW_DAYS,
            raw_cap_bytes: window::DEFAULT_RAW_CAP_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Bootstrapping {
        records: u64,
    },
    Live,
    Offline,
    UpdateRequired {
        server: ApiVersion,
        min_client: ApiVersion,
    },
    Unpaired,
}

#[derive(Debug, Clone)]
pub enum ReplicaEvent {
    /// Unread INBOX mail first seen by a pull.
    NewMail(Vec<Arrival>),
    /// Server `op`/`outbox` changes for the durable intents and outbox.
    Writes(Vec<ChangeDto>),
    /// A bootstrap replaced the replica.
    Replaced,
}

pub struct Replica {
    layout: Layout,
    client: ServerClient,
    durable: Durable,
    flush_lock: AsyncMutex<()>,
    send_lock: AsyncMutex<()>,
    flush_wanted: Notify,
    settings: Mutex<Settings>,
    store: RwLock<Option<Arc<SqliteStore>>>,
    cursor: Mutex<Option<ChangeCursor>>,
    feed: Arc<ChangeFeed>,
    phase: watch::Sender<Phase>,
    events: broadcast::Sender<ReplicaEvent>,
    sync_lock: AsyncMutex<()>,
    last_sweep: Mutex<Option<Instant>>,
    prefetching: Arc<AtomicBool>,
}

impl Replica {
    /// Opens the existing replica, if usable. Blocking file work.
    pub async fn open(
        layout: Layout,
        client: ServerClient,
        settings: Settings,
    ) -> Result<Arc<Replica>> {
        std::fs::create_dir_all(layout.root())?;
        let durable = Durable::open(&layout.durable())?;
        if durable.bind_server(client.server())? == Binding::Changed {
            tracing::warn!(server = %client.server(), "paired with another server; earlier unsent changes stay local");
        }
        let (store, cursor) = match db::open(&layout, Expected::current(), None)? {
            Opened::Ready(dbs) => {
                let (store, meta) = dbs.into_store().await?;
                (Some(Arc::new(store)), Some(meta.cursor))
            }
            Opened::NeedsBootstrap(why) => {
                tracing::info!(?why, "replica needs a bootstrap");
                (None, None)
            }
        };
        let head = match &store {
            Some(store) => store.cache().head().await?,
            None => ChangeCursor {
                epoch: CacheEpoch(0),
                seq: 0,
            },
        };
        let feed = Arc::new(ChangeFeed::new(head));
        if let Some(store) = &store {
            store.set_feed(Arc::clone(&feed));
        }
        Ok(Arc::new(Replica {
            layout,
            client,
            durable,
            flush_lock: AsyncMutex::new(()),
            send_lock: AsyncMutex::new(()),
            flush_wanted: Notify::new(),
            settings: Mutex::new(settings),
            store: RwLock::new(store),
            cursor: Mutex::new(cursor),
            feed,
            phase: watch::channel(Phase::Starting).0,
            events: broadcast::channel(EVENT_BUFFER).0,
            sync_lock: AsyncMutex::new(()),
            last_sweep: Mutex::new(None),
            prefetching: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// The current local store; `None` until the first bootstrap finished.
    pub fn store(&self) -> Option<Arc<SqliteStore>> {
        self.store.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Local change feed; its epoch changes when a bootstrap replaces the
    /// replica, which ends the SPA's event sessions with `reset`.
    pub fn feed(&self) -> &Arc<ChangeFeed> {
        &self.feed
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn client(&self) -> &ServerClient {
        &self.client
    }

    /// Intents, drafts and the outbox; never discarded.
    pub fn durable(&self) -> &Durable {
        &self.durable
    }

    pub(crate) fn flush_lock(&self) -> &AsyncMutex<()> {
        &self.flush_lock
    }

    /// Held while a send is posted and while one is cancelled or retried,
    /// so a cancelled item is never posted.
    pub(crate) fn send_lock(&self) -> &AsyncMutex<()> {
        &self.send_lock
    }

    /// Asks the running sync loop to flush queued writes now.
    pub(crate) fn kick_flush(&self) {
        self.flush_wanted.notify_one();
    }

    pub fn phase(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ReplicaEvent> {
        self.events.subscribe()
    }

    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A smaller window is swept on the next pull.
    pub fn set_settings(&self, settings: Settings) {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner()) = settings;
        *self.last_sweep.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn server_cursor(&self) -> Option<ChangeCursor> {
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.send_if_modified(|p| {
            let changed = *p != phase;
            *p = phase;
            changed
        });
    }

    /// Syncs until `stop` turns true or the device is unpaired.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let flusher = tokio::spawn(Arc::clone(&self).flush_on_demand(stop.clone()));
        self.sync_loop(&mut stop).await;
        let _ = flusher.await;
    }

    async fn flush_on_demand(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = self.flush_wanted.notified() => {}
                _ = stop.wait_for(|s| *s) => return,
            }
            if let Err(e) = self.flush().await {
                tracing::debug!(error = %e, "flush stopped");
            }
        }
    }

    async fn sync_loop(&self, stop: &mut watch::Receiver<bool>) {
        loop {
            let outcome = tokio::select! {
                r = self.session() => r,
                _ = stop.wait_for(|s| *s) => return,
            };
            let wait = match outcome {
                Ok(()) => Duration::ZERO,
                Err(Error::Client(ClientError::Unauthorized)) => {
                    self.set_phase(Phase::Unpaired);
                    return;
                }
                Err(Error::Client(ClientError::Gone)) => match self.rebootstrap().await {
                    Ok(()) => Duration::ZERO,
                    Err(Error::Client(ClientError::Api { status: 429, .. })) => BUSY_RETRY,
                    Err(e) => {
                        tracing::warn!(error = %e, "re-bootstrap failed");
                        POLL_INTERVAL
                    }
                },
                Err(Error::UpdateRequired) => POLL_INTERVAL,
                Err(Error::Client(ClientError::Api { status: 429, .. })) => BUSY_RETRY,
                Err(Error::Client(ClientError::Offline(e))) => {
                    tracing::debug!(error = %e, "server unreachable");
                    self.set_phase(Phase::Offline);
                    POLL_INTERVAL
                }
                Err(e) => {
                    tracing::warn!(error = %e, "replica sync failed");
                    POLL_INTERVAL
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stop.wait_for(|s| *s) => return,
            }
        }
    }

    /// One connection: gate, bootstrap if needed, catch up, then pull on
    /// every doorbell until the stream ends.
    async fn session(&self) -> Result<()> {
        if let Compat::UpdateRequired { server, min_client } = version::probe(&self.client).await? {
            self.set_phase(Phase::UpdateRequired { server, min_client });
            return Err(Error::UpdateRequired);
        }
        if self.store().is_none() {
            self.rebootstrap().await?;
        }
        self.sync_once().await?;
        let cursor = self
            .server_cursor()
            .ok_or(Error::Client(ClientError::Gone))?;
        let mut bells = events::connect(&self.client, cursor).await?;
        self.set_phase(Phase::Live);
        while let Some(bell) = bells.next().await? {
            match bell {
                Doorbell::Changes => self.sync_once().await?,
                Doorbell::Reset => return Err(Error::Client(ClientError::Gone)),
            }
        }
        Ok(())
    }

    /// Pulls everything new, sweeps when due and starts a prefetch.
    pub async fn sync_once(&self) -> Result<()> {
        let _guard = self.sync_lock.lock().await;
        let (Some(store), Some(since)) = (self.store(), self.server_cursor()) else {
            return Err(Error::Client(ClientError::Gone));
        };
        let settings = self.settings();
        let pulled = pull::pull(&self.client, &store, since, settings.window_days).await?;
        if let Some(cursor) = pulled.cursor {
            *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = Some(cursor);
        }
        if let Some(head) = pulled.local_head {
            self.feed.publish(head);
        }
        if !pulled.arrivals.is_empty() {
            let _ = self.events.send(ReplicaEvent::NewMail(pulled.arrivals));
        }
        self.apply_server_ops(&pulled.writes).await?;
        if !pulled.writes.is_empty() {
            let _ = self.events.send(ReplicaEvent::Writes(pulled.writes));
        }
        self.refresh_overlay(None, Vec::new()).await?;
        let start = window::window_start(now(), settings.window_days);
        if self.sweep_due() {
            window::sweep(&store, start).await?;
        }
        self.spawn_prefetch(store, start, settings.raw_cap_bytes);
        drop(_guard);
        self.flush().await
    }

    fn sweep_due(&self) -> bool {
        let mut last = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
        let due = last.is_none_or(|at| at.elapsed() >= SWEEP_INTERVAL);
        if due {
            *last = Some(Instant::now());
        }
        due
    }

    fn spawn_prefetch(&self, store: Arc<SqliteStore>, start: i64, cap: u64) {
        if self.prefetching.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.client.clone();
        let flag = Arc::clone(&self.prefetching);
        tokio::spawn(async move {
            if let Err(e) = window::prefetch(&client, &store, start, cap).await {
                tracing::debug!(error = %e, "raw prefetch stopped");
            }
            flag.store(false, Ordering::Release);
        });
    }

    /// Imports a fresh snapshot and swaps it in; the old replica stays
    /// readable until the import completed.
    pub async fn rebootstrap(&self) -> Result<()> {
        let _guard = self.sync_lock.lock().await;
        self.set_phase(Phase::Bootstrapping { records: 0 });
        let settings = self.settings();
        let snapshot =
            bootstrap::import_snapshot(&self.client, &self.layout, settings.window_days).await?;
        self.set_phase(Phase::Bootstrapping {
            records: snapshot.records,
        });
        let old = self.store.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(old) = old {
            close_when_released(old).await;
        }
        self.layout.install_tmp()?;
        let dbs = match db::open(
            &self.layout,
            Expected::current(),
            Some(snapshot.meta.server_epoch),
        )? {
            Opened::Ready(dbs) => dbs,
            Opened::NeedsBootstrap(why) => {
                return Err(Error::Corrupt(format!("fresh replica rejected: {why:?}")));
            }
        };
        let (store, meta) = dbs.into_store().await?;
        store.set_feed(Arc::clone(&self.feed));
        self.feed.publish(store.cache().head().await?);
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = Some(meta.cursor);
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(store));
        *self.last_sweep.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let _ = self.events.send(ReplicaEvent::Replaced);
        self.durable.recheck_all_sent()?;
        self.refresh_overlay(None, Vec::new()).await
    }
}

/// Waits briefly for in-flight readers before closing; files of a store
/// still referenced close when its last reader lets go.
async fn close_when_released(mut store: Arc<SqliteStore>) {
    let deadline = Instant::now() + CLOSE_WAIT;
    loop {
        match Arc::try_unwrap(store) {
            Ok(owned) => {
                if let Err(e) = owned.close().await {
                    tracing::warn!(error = %e, "closing the old replica");
                }
                return;
            }
            Err(shared) if Instant::now() < deadline => {
                store = shared;
                tokio::time::sleep(CLOSE_POLL).await;
            }
            Err(_) => {
                tracing::warn!("old replica still in use; closing it on last release");
                return;
            }
        }
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
