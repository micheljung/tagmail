//! The paired desktop: pairing, the device token, settings and the replica
//! that keeps a local copy of the server in sync.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tagmail_api_core::api::{self, offline_error};
use tagmail_api_core::auth_dto::DeviceTokenDto;
use tagmail_api_core::desktop_dto::{
    DesktopPhaseDto, DesktopSettingsDto, DesktopSettingsPatchDto, DesktopStatusDto,
    UpdateRequiredDto,
};
use tagmail_api_core::dto::ApiErrorDto;
use tagmail_api_core::events::EventSource;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_replica::client::REQUEST_TIMEOUT;
use tagmail_replica::durable::FROM_OTHER_SERVER;
use tagmail_replica::{
    Binding, ClientError, Durable, Layout, Phase, Replica, ReplicaEvent, Router, ServerClient,
};
use tokio::sync::{broadcast, watch};
use url::Url;

use crate::backend::{Backend, EventStore};
use crate::events::{Emit, RESET};
use crate::settings;
use crate::token::{FILE_WARNING, TokenStore};

pub const STATUS: &str = "desktop:status";
pub const UPDATE_REQUIRED: &str = "desktop:update-required";
const SERVER_KEY: &str = "server_url";
const BOUND_SERVER_KEY: &str = "bound_server";
const DEVICE_NAME_KEY: &str = "device_name";
const TOKEN_FILE: &str = "device-token";
const EVENT_BUFFER: usize = 64;
const MAX_DEVICE_NAME: usize = 100;
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
const INSECURE_SERVER: &str =
    "an https address, e.g. https://mail.example.com (plain http only for localhost)";
pub const INSECURE_WARNING: &str = "The stored server address uses plain http, so this device \
     stopped syncing with it. Pair again with an https address.";

struct Running {
    replica: Arc<Replica>,
    router: Router,
    stop: watch::Sender<bool>,
}

pub struct Desktop {
    me: Weak<Desktop>,
    layout: Layout,
    durable: Durable,
    tokens: TokenStore,
    emit: Arc<dyn Emit>,
    settings: Mutex<DesktopSettingsDto>,
    running: Mutex<Option<Running>>,
    events: broadcast::Sender<ReplicaEvent>,
    last_sync: AtomicI64,
    insecure_server: AtomicBool,
    rebound: Mutex<Option<String>>,
    changed: watch::Sender<u64>,
    settings_tx: watch::Sender<DesktopSettingsDto>,
}

const QUARANTINED: &str = "SELECT \
     (SELECT count(*) FROM intent WHERE state = 'failed' AND error = ?1) \
     + (SELECT count(*) FROM outbox WHERE state = 'failed' AND error = ?1) \
     + (SELECT count(*) FROM draft WHERE other_server = 1)";

const UNREAD_INBOX: &str = "SELECT count(DISTINCT t.id) FROM thread t \
     JOIN thread_mailbox tm ON tm.thread_id = t.id \
     JOIN mailbox mb ON mb.id = tm.mailbox_id \
     WHERE mb.name = 'INBOX' COLLATE NOCASE AND t.unread_count > 0 AND t.merged_into IS NULL";

fn json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("DTOs serialize infallibly")
}

fn invalid(field: &str, message: &str) -> api::Response {
    api::Response::json(
        422,
        &ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.to_owned(),
            field: Some(field.to_owned()),
        },
    )
}

fn internal(e: impl std::fmt::Display) -> api::Response {
    tracing::error!(error = %e, "desktop failure");
    api::Response::error(500, "internal", "internal error")
}

/// Creates `dir` readable by the user only.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(
        dir,
        std::os::unix::fs::PermissionsExt::from_mode(PRIVATE_DIR_MODE),
    )?;
    Ok(())
}

/// The name the server lists this device under.
pub fn device_name() -> String {
    let name = ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|n| {
            n.trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_DEVICE_NAME)
                .collect::<String>()
        })
        .filter(|n| !n.is_empty());
    name.unwrap_or_else(|| "Tagmail desktop".to_owned())
}

fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn host_label(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// Plain http only reaches this machine; anything else needs https.
pub fn server_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let allowed = match url.scheme() {
        "https" => url.host().is_some(),
        "http" => loopback(&url),
        _ => false,
    };
    allowed.then_some(url)
}

impl Desktop {
    /// Opens the durable store under `data_dir` and resumes syncing when a
    /// pairing is stored. `keychain: false` keeps the token in the file.
    pub async fn open(
        data_dir: &Path,
        keychain: bool,
        emit: Arc<dyn Emit>,
    ) -> tagmail_replica::Result<Arc<Desktop>> {
        let dir = data_dir.to_owned();
        let layout = Layout::new(&dir);
        let durable_path = layout.durable();
        let (durable, settings) = tokio::task::spawn_blocking(move || {
            private_dir(&dir)?;
            let durable = Durable::open(&durable_path)?;
            let settings = settings::load(&durable)?;
            Ok::<_, tagmail_replica::Error>((durable, settings))
        })
        .await
        .expect("opening the durable store does not panic")?;
        let desktop = Arc::new_cyclic(|me| Desktop {
            me: me.clone(),
            layout,
            durable,
            tokens: TokenStore::new(data_dir.join(TOKEN_FILE), keychain),
            emit,
            settings_tx: watch::Sender::new(settings.clone()),
            settings: Mutex::new(settings),
            changed: watch::Sender::new(0),
            running: Mutex::new(None),
            events: broadcast::channel(EVENT_BUFFER).0,
            last_sync: AtomicI64::new(0),
            insecure_server: AtomicBool::new(false),
            rebound: Mutex::new(None),
        });
        let server = desktop.stored_server()?;
        let token = desktop.tokens.load().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "reading the device token failed");
            None
        });
        if let (Some(server), Some(token)) = (server, token) {
            desktop.start(&server, token).await?;
        }
        Ok(desktop)
    }

    /// New mail, write and replace events of whichever replica runs.
    pub fn subscribe(&self) -> broadcast::Receiver<ReplicaEvent> {
        self.events.subscribe()
    }

    /// Bumped whenever local mail may have changed (pulls, local writes,
    /// a new replica), e.g. to recount the tray's unread number.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn settings_changes(&self) -> watch::Receiver<DesktopSettingsDto> {
        self.settings_tx.subscribe()
    }

    /// Unread INBOX conversations of the local replica.
    pub async fn unread_inbox(&self) -> Option<i64> {
        let store = self.replica()?.store()?;
        store
            .cache()
            .read(|c| Ok(c.query_row(UNREAD_INBOX, [], |r| r.get(0))?))
            .await
            .map_err(|e| tracing::warn!(error = %e, "counting unread mail failed"))
            .ok()
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn replica(&self) -> Option<Arc<Replica>> {
        self.lock_running().as_ref().map(|r| Arc::clone(&r.replica))
    }

    fn arc(&self) -> Arc<Desktop> {
        self.me
            .upgrade()
            .expect("a Desktop is only used through its Arc")
    }

    fn lock_running(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_rebound(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.rebound.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Binds the durable rows to `server` and words what stays behind when
    /// they were written for another one.
    fn bind(&self, server: &Url) -> tagmail_replica::Result<Option<String>> {
        let previous = self
            .durable
            .meta(BOUND_SERVER_KEY)?
            .and_then(|raw| Url::parse(&raw).ok());
        if self.durable.bind_server(server)? == Binding::Bound {
            return Ok(None);
        }
        let unsent: i64 = self
            .durable
            .conn()
            .query_row(QUARANTINED, [FROM_OTHER_SERVER], |r| r.get(0))?;
        let old = previous.map_or_else(|| "the previous server".to_owned(), |u| host_label(&u));
        Ok((unsent > 0).then(|| {
            let (changes, stay, are) = if unsent == 1 {
                ("change", "stays", "is")
            } else {
                ("changes", "stay", "are")
            };
            format!(
                "{unsent} unsent {changes} for {old} {stay} on this device and {are} not sent to {}.",
                host_label(server)
            )
        }))
    }

    fn lock_settings(&self) -> std::sync::MutexGuard<'_, DesktopSettingsDto> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stored_server(&self) -> tagmail_replica::Result<Option<Url>> {
        let raw = self.durable.meta(SERVER_KEY)?.unwrap_or_default();
        let url = server_url(&raw);
        self.insecure_server
            .store(url.is_none() && !raw.is_empty(), Ordering::Relaxed);
        Ok(url)
    }

    async fn start(self: &Arc<Self>, server: &Url, token: String) -> tagmail_replica::Result<()> {
        let client = ServerClient::new(server, Some(token))?;
        let replica_settings = settings::replica_settings(&self.lock_settings());
        let replica = Replica::open(self.layout.clone(), client, replica_settings).await?;
        let (stop, stop_rx) = watch::channel(false);
        tokio::spawn(Arc::clone(&replica).run(stop_rx));
        tokio::spawn(watch_replica(
            Arc::downgrade(self),
            Arc::clone(&replica),
            stop.subscribe(),
        ));
        let router = Router::new(Arc::clone(&replica));
        if let Some(old) = self.lock_running().replace(Running {
            replica,
            router,
            stop,
        }) {
            let _ = old.stop.send(true);
        }
        Ok(())
    }

    fn stop(&self) {
        if let Some(old) = self.lock_running().take() {
            let _ = old.stop.send(true);
        }
        self.bump();
    }

    fn synced(&self) {
        self.last_sync
            .store(tagmail_replica::sync::now(), Ordering::Relaxed);
    }

    fn publish_status(&self) {
        let status = json(&self.status());
        #[cfg(feature = "test-hooks")]
        println!("DESKTOP_STATUS {status}");
        self.emit.emit(STATUS, status);
    }

    async fn exchange(server: &Url, code: &str) -> Result<DeviceTokenDto, api::Response> {
        let client = ServerClient::new(server, None).map_err(|e| match e {
            ClientError::Insecure(_) => invalid("server", INSECURE_SERVER),
            other => internal(other),
        })?;
        let request = client
            .request(reqwest::Method::POST, "api/pairing/exchange")
            .map_err(internal)?
            .timeout(REQUEST_TIMEOUT)
            .header("x-requested-with", "tagmail")
            .json(&serde_json::json!({ "code": code, "name": device_name() }));
        let resp = client.send(request).await.map_err(|e| match e {
            ClientError::Offline(_) => offline_error("Could not reach the server"),
            ClientError::Api { status, body } if status != 401 => {
                api::Response::json(status, &body)
            }
            ClientError::Unauthorized | ClientError::Api { .. } | ClientError::Status { .. } => {
                api::Response::error(400, "invalid_code", "That code is wrong or expired")
            }
            other => internal(other),
        })?;
        resp.json::<DeviceTokenDto>().await.map_err(internal)
    }
}

/// Mirrors a replica's phase into `desktop:status`, notes when it applied
/// server changes and forwards its events, until it is stopped.
async fn watch_replica(
    desktop: Weak<Desktop>,
    replica: Arc<Replica>,
    mut stop: watch::Receiver<bool>,
) {
    let mut phase = replica.phase();
    let mut events = replica.subscribe();
    let mut head = replica.feed().subscribe();
    drop(replica);
    loop {
        tokio::select! {
            changed = phase.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = phase.borrow_and_update().clone();
                let Some(desktop) = desktop.upgrade() else { return };
                match current {
                    Phase::UpdateRequired { server, min_client } => desktop
                        .emit
                        .emit(UPDATE_REQUIRED, json(&UpdateRequiredDto { server, min_client })),
                    Phase::Live => desktop.synced(),
                    _ => {}
                }
                desktop.publish_status();
            }
            changed = head.changed() => {
                if changed.is_err() {
                    return;
                }
                head.borrow_and_update();
                let Some(desktop) = desktop.upgrade() else { return };
                desktop.synced();
                desktop.bump();
            }
            event = events.recv() => {
                let Some(desktop) = desktop.upgrade() else { return };
                match event {
                    Ok(event) => {
                        if matches!(event, ReplicaEvent::Replaced) {
                            desktop.emit.emit(RESET, json(&api::gone_error()));
                            desktop.publish_status();
                            desktop.bump();
                        }
                        let _ = desktop.events.send(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "replica events lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

fn phase_dto(phase: &Phase, bootstrapped: bool) -> DesktopPhaseDto {
    let downloading = DesktopPhaseDto::Bootstrapping { done: 0, total: 0 };
    match phase {
        Phase::Bootstrapping { records } => DesktopPhaseDto::Bootstrapping {
            done: *records,
            total: 0,
        },
        Phase::Starting | Phase::Offline | Phase::Live if !bootstrapped => downloading,
        Phase::Starting | Phase::Offline | Phase::Live => DesktopPhaseDto::Live,
        Phase::UpdateRequired { server, min_client } => DesktopPhaseDto::UpdateRequired {
            server: *server,
            min_client: *min_client,
        },
        Phase::Unpaired => DesktopPhaseDto::Unpaired,
    }
}

impl Backend for Desktop {
    type Store = SqliteStore;

    async fn handle_api(&self, req: api::Request<'_>, content_type: Option<&str>) -> api::Response {
        let router = self.lock_running().as_ref().map(|r| r.router.clone());
        let Some(router) = router else {
            return unpaired();
        };
        if *router.replica().phase().borrow() == Phase::Unpaired {
            return unpaired();
        }
        router.handle(req, content_type).await
    }

    fn events(&self) -> Option<EventStore<SqliteStore>> {
        let replica = self.replica()?;
        let source: Arc<dyn EventSource> = replica.feed().clone();
        Some((replica.store()?, source))
    }

    fn paired(&self) -> bool {
        self.replica()
            .is_some_and(|r| *r.phase().borrow() != Phase::Unpaired)
    }

    fn status(&self) -> DesktopStatusDto {
        let mut warnings = Vec::new();
        if self.tokens.in_file() {
            warnings.push(FILE_WARNING.to_owned());
        }
        if self.insecure_server.load(Ordering::Relaxed) {
            warnings.push(INSECURE_WARNING.to_owned());
        }
        warnings.extend(self.lock_rebound().clone());
        let Some(replica) = self.replica() else {
            return DesktopStatusDto {
                paired: false,
                server: None,
                online: false,
                phase: DesktopPhaseDto::Unpaired,
                last_sync: None,
                warnings,
            };
        };
        let phase = phase_dto(&replica.phase().borrow(), replica.store().is_some());
        DesktopStatusDto {
            paired: phase != DesktopPhaseDto::Unpaired,
            server: Some(
                replica
                    .client()
                    .server()
                    .as_str()
                    .trim_end_matches('/')
                    .to_owned(),
            ),
            online: replica.client().is_online(),
            phase,
            last_sync: Some(self.last_sync.load(Ordering::Relaxed)).filter(|t| *t > 0),
            warnings,
        }
    }

    async fn pair(&self, server: String, code: String) -> Result<DesktopStatusDto, api::Response> {
        let Some(url) = server_url(&server) else {
            return Err(invalid("server", INSECURE_SERVER));
        };
        let code = code.trim();
        if code.is_empty() {
            return Err(invalid("code", "enter the pairing code"));
        }
        let token = Desktop::exchange(&url, code).await?;
        let previous = self.stored_server().map_err(internal)?;
        self.stop();
        if previous.as_ref() != Some(&url) {
            self.layout.discard().map_err(internal)?;
        }
        self.insecure_server.store(false, Ordering::Relaxed);
        *self.lock_rebound() = self.bind(&url).map_err(internal)?;
        self.tokens.save(&token.token).map_err(internal)?;
        self.durable
            .set_meta(SERVER_KEY, url.as_str())
            .map_err(internal)?;
        self.durable
            .set_meta(DEVICE_NAME_KEY, &device_name())
            .map_err(internal)?;
        let me = self.arc();
        me.start(&url, token.token).await.map_err(internal)?;
        self.publish_status();
        Ok(self.status())
    }

    async fn unpair(&self) {
        self.stop();
        if let Err(e) = self.tokens.delete() {
            tracing::warn!(error = %e, "removing the device token failed");
        }
        if let Err(e) = self.durable.set_meta(SERVER_KEY, "") {
            tracing::warn!(error = %e, "forgetting the server failed");
        }
        self.insecure_server.store(false, Ordering::Relaxed);
        *self.lock_rebound() = None;
        if let Err(e) = self.layout.discard() {
            tracing::warn!(error = %e, "removing the replica failed");
        }
        self.publish_status();
    }

    fn settings(&self) -> DesktopSettingsDto {
        self.lock_settings().clone()
    }

    fn update_settings(
        &self,
        patch: DesktopSettingsPatchDto,
    ) -> Result<DesktopSettingsDto, api::Response> {
        let mut current = self.lock_settings();
        let next = settings::apply(&current, patch)?;
        settings::save(&self.durable, &next).map_err(internal)?;
        if let Some(replica) = self.replica() {
            replica.set_settings(settings::replica_settings(&next));
        }
        *current = next.clone();
        self.settings_tx.send_replace(next.clone());
        Ok(next)
    }
}

fn unpaired() -> api::Response {
    api::Response::error(401, "unpaired", "pair this device with your server")
}

#[cfg(test)]
mod tests {
    use super::server_url;

    #[test]
    fn plain_http_only_reaches_loopback() {
        for ok in [
            "https://mail.example.com",
            "https://192.0.2.1:8443/",
            "http://localhost:8080",
            "http://LOCALHOST",
            "http://127.0.0.1:3000",
            "http://127.8.9.10",
            "http://[::1]:8080",
        ] {
            assert!(server_url(ok).is_some(), "{ok}");
        }
        for refused in [
            "http://mail.example.com",
            "http://192.168.1.10",
            "http://10.0.0.1",
            "http://[::ffff:127.0.0.1]",
            "http://localhost.example.com",
            "http://localhost.",
            "ftp://localhost",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(server_url(refused).is_none(), "{refused}");
        }
    }
}
