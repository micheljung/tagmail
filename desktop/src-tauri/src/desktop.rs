//! The paired desktop: pairing, the device token, settings and the replica
//! that keeps a local copy of the server in sync.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tagmail_api_core::api::{self, Api, offline_error};
use tagmail_api_core::auth_dto::DeviceTokenDto;
use tagmail_api_core::desktop_dto::{
    DesktopPhaseDto, DesktopSettingsDto, DesktopSettingsPatchDto, DesktopStatusDto,
    UpdateRequiredDto,
};
use tagmail_api_core::dto::ApiErrorDto;
use tagmail_api_core::events::EventSource;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_replica::client::REQUEST_TIMEOUT;
use tagmail_replica::writes::DesktopWrites;
use tagmail_replica::{ClientError, Durable, Layout, Phase, Replica, ReplicaEvent, ServerClient};
use tokio::sync::{broadcast, watch};
use url::Url;

use crate::backend::{Backend, EventStore};
use crate::events::{Emit, RESET};
use crate::settings;
use crate::token::{FILE_WARNING, TokenStore};

pub const STATUS: &str = "desktop:status";
pub const UPDATE_REQUIRED: &str = "desktop:update-required";
const SERVER_KEY: &str = "server_url";
const DEVICE_NAME_KEY: &str = "device_name";
const TOKEN_FILE: &str = "device-token";
const EVENT_BUFFER: usize = 64;
const DOWNLOADING: &str = "Your mail is still downloading to this device";
const MAX_DEVICE_NAME: usize = 100;

struct Running {
    replica: Arc<Replica>,
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
}

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

fn server_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host().is_some()).then_some(url)
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
            std::fs::create_dir_all(&dir)?;
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
            settings: Mutex::new(settings),
            running: Mutex::new(None),
            events: broadcast::channel(EVENT_BUFFER).0,
            last_sync: AtomicI64::new(0),
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

    fn lock_settings(&self) -> std::sync::MutexGuard<'_, DesktopSettingsDto> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stored_server(&self) -> tagmail_replica::Result<Option<Url>> {
        Ok(self
            .durable
            .meta(SERVER_KEY)?
            .and_then(|raw| server_url(&raw)))
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
        if let Some(old) = self.lock_running().replace(Running { replica, stop }) {
            let _ = old.stop.send(true);
        }
        Ok(())
    }

    fn stop(&self) {
        if let Some(old) = self.lock_running().take() {
            let _ = old.stop.send(true);
        }
    }

    fn synced(&self) {
        self.last_sync
            .store(tagmail_replica::sync::now(), Ordering::Relaxed);
    }

    fn publish_status(&self) {
        self.emit.emit(STATUS, json(&self.status()));
    }

    async fn exchange(server: &Url, code: &str) -> Result<DeviceTokenDto, api::Response> {
        let client = ServerClient::new(server, None).map_err(internal)?;
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
            }
            event = events.recv() => {
                let Some(desktop) = desktop.upgrade() else { return };
                match event {
                    Ok(event) => {
                        if matches!(event, ReplicaEvent::Replaced) {
                            desktop.emit.emit(RESET, json(&api::gone_error()));
                            desktop.publish_status();
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

    fn api(&self) -> Result<Api<SqliteStore>, api::Response> {
        let Some(replica) = self.replica() else {
            return Err(unpaired());
        };
        if *replica.phase().borrow() == Phase::Unpaired {
            return Err(unpaired());
        }
        match replica.store() {
            Some(store) => Ok(Api::new(store, None).with_writes(Arc::new(DesktopWrites(replica)))),
            None => Err(offline_error(DOWNLOADING)),
        }
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
        let warnings = if self.tokens.in_file() {
            vec![FILE_WARNING.to_owned()]
        } else {
            Vec::new()
        };
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
            return Err(invalid(
                "server",
                "an http or https address, e.g. https://mail.example.com",
            ));
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
        Ok(next)
    }
}

fn unpaired() -> api::Response {
    api::Response::error(401, "unpaired", "pair this device with your server")
}
