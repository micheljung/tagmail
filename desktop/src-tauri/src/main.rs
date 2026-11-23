#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tagmail_api_core::api::Api;
use tagmail_api_core::events::EventSource;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_desktop::events::{self, Emit, EventControl};
use tagmail_desktop::protocol::{Asset, Assets, Shell};
use tagmail_desktop::state;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

#[cfg(windows)]
const ORIGIN: &str = "http://app.localhost/";
#[cfg(not(windows))]
const ORIGIN: &str = "app://localhost/";

struct TauriAssets(tauri::AssetResolver<Wry>);

impl Assets for TauriAssets {
    fn get(&self, path: &str) -> Option<Asset> {
        self.0.get(path.to_owned()).map(|a| Asset {
            bytes: a.bytes,
            mime: a.mime_type,
        })
    }
}

struct AppEmit(AppHandle);

impl Emit for AppEmit {
    fn emit(&self, event: &str, payload: Value) {
        if let Err(e) = self.0.emit(event, payload) {
            tracing::warn!(error = %e, event, "emit failed");
        }
    }
}

struct AppState {
    shell: Shell<SqliteStore>,
    #[cfg(feature = "test-hooks")]
    selftest: Option<tagmail_desktop::selftest::SelfTest>,
}

fn data_dir(app: &AppHandle) -> tauri::Result<PathBuf> {
    #[cfg(feature = "test-hooks")]
    if let Some(dir) = std::env::var_os("TAGMAIL_DESKTOP_DATA_DIR") {
        return Ok(dir.into());
    }
    app.path().app_data_dir()
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let local = tauri::async_runtime::block_on(state::open(&data_dir(&handle)?))?;
    let (control, starts) = EventControl::new();
    let source: Arc<dyn EventSource> = local.feed.clone();
    tauri::async_runtime::spawn(events::run(
        local.store.clone(),
        source,
        starts,
        AppEmit(handle.clone()),
    ));
    let api = Api::new(local.store.clone(), None);
    #[allow(unused_mut)]
    let mut shell = Shell::new(api, Box::new(TauriAssets(app.asset_resolver())), control);
    #[cfg(feature = "test-hooks")]
    let selftest = if std::env::var_os("TAGMAIL_DESKTOP_SELFTEST").is_some() {
        shell = shell.with_index_head(tagmail_desktop::selftest::INDEX_HEAD.to_owned());
        Some(selftest::start(&handle, local)?)
    } else {
        None
    };
    app.manage(Arc::new(AppState {
        shell,
        #[cfg(feature = "test-hooks")]
        selftest,
    }));
    WebviewWindowBuilder::new(app, "main", WebviewUrl::CustomProtocol(ORIGIN.parse()?))
        .title("Tagmail")
        .inner_size(1200.0, 800.0)
        .build()?;
    Ok(())
}

#[cfg(feature = "test-hooks")]
mod selftest {
    use std::time::Duration;

    use tagmail_desktop::selftest::{SelfTest, seed};
    use tagmail_desktop::state::Local;
    use tauri::AppHandle;

    const TIMEOUT: Duration = Duration::from_secs(90);

    /// Seeds the fixture and exits with the report's verdict: 0 ok, 1 a
    /// check failed, 2 no report in time.
    pub fn start(app: &AppHandle, local: Local) -> Result<SelfTest, Box<dyn std::error::Error>> {
        tauri::async_runtime::block_on(seed(&local))?;
        let (test, mut reports) = SelfTest::new(local);
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            match tokio::time::timeout(TIMEOUT, reports.recv()).await {
                Ok(Some(report)) => {
                    println!("DESKTOP_SELFTEST {report}");
                    let ok = report["ok"].as_bool().unwrap_or(false);
                    handle.exit(if ok { 0 } else { 1 });
                }
                _ => {
                    eprintln!("DESKTOP_SELFTEST_TIMEOUT no report after {TIMEOUT:?}");
                    handle.exit(2);
                }
            }
        });
        Ok(test)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("app", |ctx, req, responder| {
            let state = ctx.app_handle().state::<Arc<AppState>>().inner().clone();
            tauri::async_runtime::spawn(async move {
                #[cfg(feature = "test-hooks")]
                if let Some(test) = &state.selftest
                    && let Some(resp) = test.handle(&req).await
                {
                    responder.respond(resp);
                    return;
                }
                responder.respond(state.shell.handle(&req).await);
            });
        })
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("run tauri app");
}
