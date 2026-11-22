use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tagmail_api_server::credentials::{self, KEY_FILE_ENV};
use tagmail_api_server::{Config, DEFAULT_LISTEN, DEFAULT_MAX_SSE_CLIENTS, Server};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// Tagmail server: syncs IMAP accounts and serves the web app's API.
#[derive(Debug, Parser)]
#[command(name = "tagmail-server", version)]
struct Cli {
    /// Address to listen on. Only loopback is allowed without
    /// --insecure-no-auth.
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
    /// DEVELOPMENT ONLY: allow a non-loopback --listen address although the
    /// API has no authentication yet
    #[arg(long)]
    insecure_no_auth: bool,
    /// Maximum concurrent event streams
    #[arg(long, default_value_t = DEFAULT_MAX_SSE_CLIENTS)]
    max_sse_clients: usize,
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
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
        insecure_no_auth: cli.insecure_no_auth,
        max_sse_clients: cli.max_sse_clients,
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting tagmail-server"
    );
    let mut server = Server::start(&config, None).await?;
    tokio::select! {
        result = server.stopped() => result?,
        () = terminated() => {
            tracing::info!("shutting down");
            server.shutdown().await?;
        }
    }
    Ok(())
}
