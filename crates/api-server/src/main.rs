use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use ipnet::IpNet;
use tagmail_api_server::auth::{AuthConfig, parse_origin, parse_proxy};
use tagmail_api_server::credentials::{self, KEY_FILE_ENV};
use tagmail_api_server::{Config, DEFAULT_LISTEN, DEFAULT_MAX_SSE_CLIENTS, Server};
use tagmail_engine::EngineConfig;
use tracing_subscriber::EnvFilter;

/// glibc's allocator idles anywhere between 24 and 55 MiB after a sync;
/// jemalloc returns freed pages and keeps two arenas, with settings compiled
/// in (see `.cargo/config.toml`). Not used on musl, whose own allocator idles
/// lower (docs/dev/memory.md).
#[cfg(not(target_env = "musl"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// Tagmail server: syncs IMAP accounts and serves the web app's API.
#[derive(Debug, Parser)]
#[command(name = "tagmail-server", version)]
struct Cli {
    /// Address to listen on. A non-loopback address needs an https
    /// --public-origin (TLS terminated by a reverse proxy) or --insecure-http.
    #[arg(long, default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// Directory for config.db and cache.db
    #[arg(long)]
    data_dir: PathBuf,
    /// Master key file. Default: $CREDENTIALS_DIRECTORY/master.key (systemd),
    /// then $TAGMAIL_MASTER_KEY_FILE (Docker secret), then
    /// /etc/tagmail/master.key. Keep it outside the data directory.
    #[arg(long)]
    master_key: Option<PathBuf>,
    /// Generate the master key (mode 0600) if it does not exist
    #[arg(long)]
    init_key: bool,
    /// Origin browsers use, e.g. https://mail.example.com. Default:
    /// http://localhost:<port>. Used for CSRF origin checks, passkeys,
    /// Secure cookies (https) and pairing links.
    #[arg(long, value_parser = parse_origin)]
    public_origin: Option<url::Url>,
    /// Reverse proxy address or CIDR whose X-Forwarded-For is trusted for
    /// rate limiting; repeatable or comma-separated
    #[arg(long, value_delimiter = ',', value_parser = parse_proxy)]
    trusted_proxy: Vec<IpNet>,
    /// DEVELOPMENT ONLY: allow a non-loopback --listen address with a plain
    /// http --public-origin (passwords and cookies unencrypted)
    #[arg(long)]
    insecure_http: bool,
    /// Built web app to serve at / (default: `web` next to the binary, if
    /// present)
    #[arg(long)]
    web_dir: Option<PathBuf>,
    /// DEVELOPMENT ONLY: connect to IMAP and SMTP servers without TLS,
    /// sending passwords in cleartext (local test servers)
    #[arg(long)]
    insecure_plain_imap: bool,
    /// Maximum concurrent event streams
    #[arg(long, default_value_t = DEFAULT_MAX_SSE_CLIENTS)]
    max_sse_clients: usize,
    /// Seconds between polls of mailboxes not covered by IDLE
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    poll_interval: u64,
    /// Async worker threads; the work is IO-bound, so few keep memory low
    #[arg(long, default_value_t = DEFAULT_WORKER_THREADS, value_parser = clap::value_parser!(u64).range(1..=256))]
    worker_threads: u64,
    /// Upper bound of the blocking pool (SQLite reads, password hashing)
    #[arg(long, default_value_t = DEFAULT_BLOCKING_THREADS, value_parser = clap::value_parser!(u64).range(1..=512))]
    max_blocking_threads: u64,
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
}

const DEFAULT_WORKER_THREADS: u64 = 2;
const DEFAULT_BLOCKING_THREADS: u64 = 8;

fn default_web_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("web");
    dir.join("index.html").is_file().then_some(dir)
}

async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot watch SIGTERM");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(usize::try_from(cli.worker_threads)?)
        .max_blocking_threads(usize::try_from(cli.max_blocking_threads)?)
        .enable_all()
        .build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = tracing_subscriber::fmt().with_env_filter(filter);
    match cli.log_format {
        LogFormat::Text => logs.init(),
        LogFormat::Json => logs.json().init(),
    }

    let config = Config {
        listen: cli.listen,
        master_key: credentials::resolve_key_path(
            cli.master_key,
            std::env::var_os("CREDENTIALS_DIRECTORY"),
            std::env::var_os(KEY_FILE_ENV),
        ),
        data_dir: cli.data_dir,
        init_key: cli.init_key,
        insecure_plain_imap: cli.insecure_plain_imap,
        max_sse_clients: cli.max_sse_clients,
        auth: AuthConfig {
            public_origin: cli.public_origin,
            trusted_proxies: cli.trusted_proxy,
            insecure_http: cli.insecure_http,
            ..AuthConfig::default()
        },
        web_dir: cli.web_dir.or_else(default_web_dir),
        push: Default::default(),
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting tagmail-server"
    );
    let engine = EngineConfig {
        poll_interval: Duration::from_secs(cli.poll_interval),
        ..EngineConfig::default()
    };
    let mut server = Server::start_with_engine(&config, engine).await?;
    tokio::select! {
        result = server.stopped() => result?,
        () = terminated() => {
            tracing::info!("shutting down");
            server.shutdown().await?;
        }
    }
    Ok(())
}
