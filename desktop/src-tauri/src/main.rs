#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tagmail_desktop::backend::Backend;
use tagmail_desktop::desktop::Desktop;
use tagmail_desktop::events::{self, Emit, EventControl};
use tagmail_desktop::mailto::LinkQueue;
use tagmail_desktop::protocol::{Asset, Assets, HttpResponse, Shell};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

mod native;

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

#[derive(Clone)]
struct AppEmit(AppHandle);

impl Emit for AppEmit {
    fn emit(&self, event: &str, payload: Value) {
        if let Err(e) = self.0.emit(event, payload) {
            tracing::warn!(error = %e, event, "emit failed");
        }
    }
}

type Handled<'a> = Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>>;

/// The protocol handler, whichever backend serves it.
trait Handler: Send + Sync {
    fn handle<'a>(&'a self, req: &'a http::Request<Vec<u8>>) -> Handled<'a>;
}

impl<B: Backend> Handler for Shell<B> {
    fn handle<'a>(&'a self, req: &'a http::Request<Vec<u8>>) -> Handled<'a> {
        Box::pin(Shell::handle(self, req))
    }
}

struct AppState {
    shell: Box<dyn Handler>,
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

/// Wires a backend to the protocol and the event loop.
fn shell<B: Backend>(app: &tauri::App, backend: Arc<B>) -> Shell<B> {
    let (control, starts) = EventControl::new();
    tauri::async_runtime::spawn(events::run(
        Arc::clone(&backend),
        starts,
        AppEmit(app.handle().clone()),
    ));
    Shell::new(
        backend,
        Box::new(TauriAssets(app.asset_resolver())),
        control,
    )
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let dir = data_dir(&handle)?;
    #[cfg(feature = "test-hooks")]
    if std::env::var_os("TAGMAIL_DESKTOP_SELFTEST").is_some() {
        let (shell, test) = selftest::start(app, &dir)?;
        return open(
            app,
            AppState {
                shell: Box::new(shell),
                selftest: Some(test),
            },
            None,
        );
    }
    // Test builds keep the token in a file so CI needs no keychain.
    let keychain = !cfg!(feature = "test-hooks");
    let emit = Arc::new(AppEmit(handle.clone()));
    let desktop = tauri::async_runtime::block_on(Desktop::open(&dir, keychain, emit))?;
    #[cfg(feature = "test-hooks")]
    selftest::auto_pair(&desktop);
    let links = Arc::new(LinkQueue::new(Box::new(AppEmit(handle.clone()))));
    let shell = shell(app, Arc::clone(&desktop)).with_links(Arc::clone(&links));
    #[cfg(feature = "test-hooks")]
    let shell = if std::env::var_os("TAGMAIL_DESKTOP_PROBE").is_some() {
        shell.with_index_head(tagmail_desktop::selftest::PROBE_HEAD.to_owned())
    } else {
        shell
    };
    open(
        app,
        AppState {
            shell: Box::new(shell),
            #[cfg(feature = "test-hooks")]
            selftest: None,
        },
        Some(Arc::clone(&links)),
    )?;
    if let Some(window) = app.get_webview_window(native::WINDOW) {
        let desktop = Arc::clone(&desktop);
        let main = window.as_ref().window();
        window.on_window_event(move |event| native::on_window_event(&main, event, &desktop));
    }
    native::install(app, desktop, links)?;
    Ok(())
}

fn open(
    app: &mut tauri::App,
    state: AppState,
    links: Option<Arc<LinkQueue>>,
) -> Result<(), Box<dyn std::error::Error>> {
    app.manage(Arc::new(state));
    WebviewWindowBuilder::new(
        app,
        native::WINDOW,
        WebviewUrl::CustomProtocol(ORIGIN.parse()?),
    )
    .title("Tagmail")
    .inner_size(1200.0, 800.0)
    .on_page_load(move |_, payload| {
        if let (Some(links), PageLoadEvent::Started) = (&links, payload.event()) {
            links.unready();
        }
    })
    .build()?;
    Ok(())
}

#[cfg(feature = "test-hooks")]
mod selftest {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use tagmail_desktop::backend::{Backend, LocalBackend};
    use tagmail_desktop::desktop::Desktop;
    use tagmail_desktop::protocol::Shell;
    use tagmail_desktop::selftest::{INDEX_HEAD, SelfTest, seed};
    use tagmail_desktop::state;

    const TIMEOUT: Duration = Duration::from_secs(90);

    /// Seeds a fixture store and exits with the report's verdict: 0 ok, 1 a
    /// check failed, 2 no report in time.
    pub fn start(
        app: &tauri::App,
        dir: &Path,
    ) -> Result<(Shell<LocalBackend>, SelfTest), Box<dyn std::error::Error>> {
        let local = tauri::async_runtime::block_on(async {
            let local = state::open(dir).await?;
            seed(&local).await?;
            Ok::<_, tagmail_store::Error>(local)
        })?;
        let (test, mut reports) = SelfTest::new(local.clone());
        let shell = super::shell(app, Arc::new(LocalBackend::new(local)))
            .with_index_head(INDEX_HEAD.to_owned());
        let handle = app.handle().clone();
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
        Ok((shell, test))
    }

    /// `TAGMAIL_DESKTOP_PAIR=<server>,<code>` pairs an unpaired app on start.
    pub fn auto_pair(desktop: &Arc<Desktop>) {
        let Ok(spec) = std::env::var("TAGMAIL_DESKTOP_PAIR") else {
            return;
        };
        if desktop.paired() {
            return;
        }
        let Some((server, code)) = spec.split_once(',') else {
            eprintln!("TAGMAIL_DESKTOP_PAIR must be <server>,<code>");
            return;
        };
        let (server, code) = (server.to_owned(), code.to_owned());
        let desktop = Arc::clone(desktop);
        tauri::async_runtime::spawn(async move {
            if let Err(resp) = desktop.pair(server, code).await {
                eprintln!(
                    "DESKTOP_PAIR_FAILED {} {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                );
            }
        });
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    native::plugins(tauri::Builder::default())
        .register_asynchronous_uri_scheme_protocol("app", |ctx, req, responder| {
            let state = ctx.app_handle().state::<Arc<AppState>>().inner().clone();
            tauri::async_runtime::spawn(async move {
                #[cfg(feature = "test-hooks")]
                if let Some(resp) = tagmail_desktop::selftest::probe(&req) {
                    responder.respond(resp);
                    return;
                }
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
