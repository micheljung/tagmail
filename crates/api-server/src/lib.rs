//! `tagmail-server`: HTTP API, SSE push channel and credential storage over
//! the engine's databases.

pub mod credentials;
pub mod http;
pub mod sqlite_store;

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use tagmail_api_core::{AccountRegistry, Api, ChangeFeed, EventSource};
use tagmail_engine::{Engine, EngineConfig, ImapConnectorFactory, Stores};
use tagmail_store::{BlobStore, CacheDb, ConfigDb};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use tagmail_store::MasterKey;

use crate::credentials::{KeyWarning, load_master_key};
use crate::sqlite_store::SqliteStore;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7878";
pub const DEFAULT_MAX_SSE_CLIENTS: usize = 8;
pub const CONFIG_DB: &str = "config.db";
pub const CACHE_DB: &str = "cache.db";
/// Message blobs (raw and text), next to `cache.db` and as disposable.
pub const BLOBS_DIR: &str = "blobs";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub master_key: PathBuf,
    /// Generate the master key if it does not exist.
    pub init_key: bool,
    /// Allow a non-loopback listen address although M1 has no login.
    pub insecure_no_auth: bool,
    /// Connect to IMAP servers without TLS (local test servers only).
    pub insecure_plain_imap: bool,
    pub max_sse_clients: usize,
}

/// Until login exists (M2), anyone who can connect has full access, so only
/// loopback is allowed unless explicitly overridden.
pub fn check_listen(addr: SocketAddr, insecure_no_auth: bool) -> anyhow::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if !insecure_no_auth {
        bail!(
            "refusing to listen on {addr}: the API has no authentication yet, so it only binds \
             to loopback; pass --insecure-no-auth to override (development only)"
        );
    }
    tracing::warn!(%addr, "listening on a non-loopback address WITHOUT AUTHENTICATION (--insecure-no-auth); development only");
    Ok(())
}

pub struct Server {
    addr: SocketAddr,
    store: Arc<SqliteStore>,
    feed: Arc<ChangeFeed>,
    engine: Option<Arc<Engine>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}

/// What syncs the accounts.
enum Sync {
    Registry(Option<Arc<dyn AccountRegistry>>),
    Engine(EngineConfig),
}

impl Server {
    /// Opens the databases, unlocks credentials and starts serving.
    /// `registry` stands in for the sync engine; without it accounts cannot
    /// be added.
    pub async fn start(
        config: &Config,
        registry: Option<Arc<dyn AccountRegistry>>,
    ) -> anyhow::Result<Server> {
        Server::start_with(config, Sync::Registry(registry)).await
    }

    /// Like [`Server::start`] with the sync engine: it starts every active
    /// account and syncs accounts added through the API.
    pub async fn start_with_engine(
        config: &Config,
        engine: EngineConfig,
    ) -> anyhow::Result<Server> {
        Server::start_with(config, Sync::Engine(engine)).await
    }

    async fn start_with(config: &Config, sync: Sync) -> anyhow::Result<Server> {
        check_listen(config.listen, config.insecure_no_auth)?;
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("creating data directory {}", config.data_dir.display()))?;
        let (master, warnings) =
            load_master_key(&config.master_key, &config.data_dir, config.init_key)?;
        for warning in &warnings {
            match warning {
                KeyWarning::InsideDataDir { .. } => tracing::warn!("!!! {warning} !!!"),
                #[cfg(unix)]
                KeyWarning::LoosePermissions { .. } => tracing::warn!("{warning}"),
            }
        }
        let store = open_store(&config.data_dir, &master).await?;
        drop(master);
        let store = Arc::new(store);
        let feed = Arc::new(ChangeFeed::new(store.cache().head().await?));
        let (registry, engine) = match sync {
            Sync::Registry(registry) => (registry, None),
            Sync::Engine(engine_config) => {
                let blobs_dir = config.data_dir.join(BLOBS_DIR);
                let blobs = tokio::task::spawn_blocking(move || BlobStore::open(&blobs_dir))
                    .await?
                    .context("opening the blob store")?;
                let stores = Stores {
                    config: store.config().clone(),
                    vault: store.vault().clone(),
                    cache: store.cache().clone(),
                    blobs: Arc::new(blobs),
                    feed: feed.clone(),
                };
                if config.insecure_plain_imap {
                    tracing::warn!(
                        "connecting to IMAP servers WITHOUT TLS (--insecure-plain-imap); development only"
                    );
                }
                let factory = ImapConnectorFactory {
                    insecure_plaintext: config.insecure_plain_imap,
                    ..ImapConnectorFactory::default()
                };
                let engine = Arc::new(Engine::new(stores, factory, engine_config));
                let started = engine.start_all().await?;
                tracing::info!(accounts = started, "sync engine started");
                (
                    Some(engine.clone() as Arc<dyn AccountRegistry>),
                    Some(engine),
                )
            }
        };

        let listener = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("binding {}", config.listen))?;
        let addr = listener.local_addr()?;
        let (stop, shutdown) = watch::channel(false);
        let state = http::AppState {
            api: Arc::new(Api::new(store.clone(), registry)),
            events: feed.clone() as Arc<dyn EventSource>,
            sse_slots: Arc::new(Semaphore::new(config.max_sse_clients)),
            shutdown: shutdown.clone(),
        };
        let mut stopped = shutdown;
        let task = tokio::spawn(async move {
            axum::serve(listener, http::router(state))
                .with_graceful_shutdown(async move {
                    // An error means the Server was dropped: stop as well.
                    let _ = stopped.wait_for(|stop| *stop).await;
                })
                .await
        });
        tracing::info!(%addr, data_dir = %config.data_dir.display(), "listening");
        Ok(Server {
            addr,
            store,
            feed,
            engine,
            stop,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn store(&self) -> &Arc<SqliteStore> {
        &self.store
    }

    /// The engine publishes the change-log head here after each commit.
    pub fn feed(&self) -> &Arc<ChangeFeed> {
        &self.feed
    }

    /// Resolves when the HTTP server stops on its own (an I/O error).
    pub async fn stopped(&mut self) -> anyhow::Result<()> {
        (&mut self.task).await??;
        Ok(())
    }

    /// Stops syncing, ends event streams, finishes in-flight requests and
    /// closes the databases.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        if let Some(engine) = &self.engine {
            engine.shutdown().await;
        }
        self.stop.send_replace(true);
        if !self.task.is_finished() {
            self.task.await??;
        }
        drop(self.engine);
        drop(self.feed);
        match Arc::try_unwrap(self.store) {
            Ok(store) => store.close().await?,
            Err(_) => tracing::warn!("store still in use at shutdown; not closing explicitly"),
        }
        Ok(())
    }
}

async fn open_store(data_dir: &Path, master: &MasterKey) -> anyhow::Result<SqliteStore> {
    let config_path = data_dir.join(CONFIG_DB);
    let cache_path = data_dir.join(CACHE_DB);
    let (config, cache) = tokio::task::spawn_blocking(move || {
        let config = ConfigDb::open(&config_path)
            .with_context(|| format!("opening {}", config_path.display()))?;
        let cache = CacheDb::open(&cache_path)
            .with_context(|| format!("opening {}", cache_path.display()))?;
        anyhow::Ok((config, cache))
    })
    .await??;
    Ok(SqliteStore::open(config, cache, master).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_needs_the_insecure_flag() {
        for ok in ["127.0.0.1:1", "[::1]:1", "127.0.0.2:1"] {
            assert!(check_listen(ok.parse().unwrap(), false).is_ok(), "{ok}");
        }
        for open in ["0.0.0.0:1", "[::]:1", "192.168.1.2:1"] {
            let addr = open.parse().unwrap();
            assert!(check_listen(addr, false).is_err(), "{open}");
            assert!(check_listen(addr, true).is_ok(), "{open}");
        }
    }
}
