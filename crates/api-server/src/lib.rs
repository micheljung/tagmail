//! `tagmail-server`: HTTP API, SSE push channel, owner authentication and
//! credential storage over the engine's databases, plus the built SPA.

pub mod auth;
pub mod credentials;
pub mod http;
mod pairing;
pub mod proxy;
pub mod ratelimit;
pub mod session;
pub use tagmail_api_core::sqlite_store;
mod upload;
pub mod web;
mod webauthn;

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tagmail_api_core::compose::Compose;
use tagmail_api_core::{AccountRegistry, Api, ChangeFeed, EventSource, WriteService};
use tagmail_engine::{Engine, EngineConfig, ImapConnectorFactory, LettreSender, Stores};
use tagmail_store::{BlobStore, CacheDb, ConfigDb};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use tagmail_store::MasterKey;

use crate::auth::{Auth, AuthConfig};
use crate::credentials::{KeyWarning, load_master_key};
use crate::proxy::ImageFetcher;
use crate::web::WebDir;
use tagmail_api_core::sqlite_store::SqliteStore;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7878";
pub const DEFAULT_MAX_SSE_CLIENTS: usize = 8;
pub const CONFIG_DB: &str = "config.db";
pub const CACHE_DB: &str = "cache.db";
/// Message blobs (raw and text), next to `cache.db` and as disposable.
pub const BLOBS_DIR: &str = "blobs";
/// Draft attachments; durable, back up with `config.db`.
pub const UPLOADS_DIR: &str = "uploads";
/// Queued outgoing messages; durable, back up with `config.db`.
pub const OUTBOX_DIR: &str = "outbox";

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub master_key: PathBuf,
    /// Generate the master key if it does not exist.
    pub init_key: bool,
    /// Connect to IMAP and SMTP servers without TLS (local test servers
    /// only).
    pub insecure_plain_imap: bool,
    pub max_sse_clients: usize,
    pub auth: AuthConfig,
    /// Built SPA served at `/`; without it only the API is served.
    pub web_dir: Option<PathBuf>,
}

pub struct Server {
    addr: SocketAddr,
    store: Arc<SqliteStore>,
    feed: Arc<ChangeFeed>,
    auth: Arc<Auth>,
    engine: Option<Arc<Engine>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}

/// What syncs the accounts.
enum Sync {
    Registry(Option<Arc<dyn AccountRegistry>>),
    Engine(Box<EngineConfig>),
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
        Server::start_with(config, Sync::Engine(Box::new(engine))).await
    }

    async fn start_with(config: &Config, sync: Sync) -> anyhow::Result<Server> {
        auth::check_listen(config.listen, &config.auth)?;
        // Err means a provider is already installed, which is all we need.
        let _ = rustls::crypto::ring::default_provider().install_default();
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
        store.set_feed(feed.clone());
        let (registry, engine) = match sync {
            Sync::Registry(registry) => (registry, None),
            Sync::Engine(engine_config) => {
                let stores = Stores {
                    config: store.config().clone(),
                    vault: store.vault().clone(),
                    cache: store.cache().clone(),
                    blobs: store.blobs().clone(),
                    feed: feed.clone(),
                    uploads: config.data_dir.join(UPLOADS_DIR),
                    outbox: config.data_dir.join(OUTBOX_DIR),
                };
                if config.insecure_plain_imap {
                    tracing::warn!(
                        "connecting to IMAP and SMTP servers WITHOUT TLS (--insecure-plain-imap); development only"
                    );
                }
                let factory = ImapConnectorFactory {
                    insecure_plaintext: config.insecure_plain_imap,
                    ..ImapConnectorFactory::default()
                };
                let smtp = Arc::new(LettreSender {
                    insecure_plaintext: config.insecure_plain_imap,
                });
                let engine = Arc::new(Engine::with_smtp(stores, factory, *engine_config, smtp));
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
        let auth = Arc::new(Auth::new(store.config().clone(), &config.auth, addr.port()).await?);
        let (stop, shutdown) = watch::channel(false);
        let mut api = Api::new(store.clone(), registry)
            .with_image_proxy(Arc::new(ImageFetcher::with_webpki_roots()?));
        if let Some(engine) = &engine {
            api = api.with_compose(engine.clone() as Arc<dyn Compose>);
            api = api.with_writes(engine.clone() as Arc<dyn WriteService>);
        }
        let state = http::AppState {
            api: Arc::new(api),
            events: feed.clone() as Arc<dyn EventSource>,
            sse_slots: Arc::new(Semaphore::new(config.max_sse_clients)),
            shutdown: shutdown.clone(),
            auth: auth.clone(),
            web: config.web_dir.clone().map(|dir| Arc::new(WebDir::new(dir))),
            account_tests: Arc::new(ratelimit::RateLimiter::new(
                ratelimit::ACCOUNT_TESTS,
                ratelimit::ACCOUNT_TESTS,
            )),
        };
        let mut stopped = shutdown;
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                http::router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
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
            auth,
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

    pub fn auth(&self) -> &Arc<Auth> {
        &self.auth
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
        drop(self.auth);
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
    let blobs_dir = data_dir.join(BLOBS_DIR);
    let (config, cache, blobs) = tokio::task::spawn_blocking(move || {
        let config = ConfigDb::open(&config_path)
            .with_context(|| format!("opening {}", config_path.display()))?;
        let cache = CacheDb::open(&cache_path)
            .with_context(|| format!("opening {}", cache_path.display()))?;
        let blobs = BlobStore::open(&blobs_dir).context("opening the blob store")?;
        anyhow::Ok((config, cache, blobs))
    })
    .await??;
    Ok(SqliteStore::open(config, cache, blobs, master).await?)
}
