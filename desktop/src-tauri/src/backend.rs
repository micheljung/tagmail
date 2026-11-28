//! What the protocol serves `/api` from: the replica of a paired server, or
//! a fixed local store (tests and the self-test).

use std::future::Future;
use std::sync::{Arc, Mutex};

use tagmail_api_core::api::{self, Api};
use tagmail_api_core::desktop_dto::{
    DesktopPhaseDto, DesktopSettingsDto, DesktopSettingsPatchDto, DesktopStatusDto,
};
use tagmail_api_core::events::EventSource;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_api_core::store::Store;

use crate::settings;
use crate::state::Local;

/// A store and the feed an event session follows on it.
pub type EventStore<S> = (Arc<S>, Arc<dyn EventSource>);

pub trait Backend: Send + Sync + 'static {
    type Store: Store;

    /// Answers an `/api` request; `content_type` is the request's header
    /// (uploads need its boundary).
    fn handle_api(
        &self,
        req: api::Request<'_>,
        content_type: Option<&str>,
    ) -> impl Future<Output = api::Response> + Send;

    /// The current store and its change feed for an event session.
    fn events(&self) -> Option<EventStore<Self::Store>>;

    fn paired(&self) -> bool;

    fn status(&self) -> DesktopStatusDto;

    fn pair(
        &self,
        server: String,
        code: String,
    ) -> impl Future<Output = Result<DesktopStatusDto, api::Response>> + Send;

    fn unpair(&self) -> impl Future<Output = ()> + Send;

    fn settings(&self) -> DesktopSettingsDto;

    fn update_settings(
        &self,
        patch: DesktopSettingsPatchDto,
    ) -> Result<DesktopSettingsDto, api::Response>;

    /// Passes a request on to the paired server, e.g. reminder settings.
    fn forward(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &[u8],
    ) -> impl Future<Output = api::Response> + Send;
}

/// A fixed, always "paired" local store with in-memory settings.
pub struct LocalBackend {
    local: Local,
    settings: Mutex<DesktopSettingsDto>,
}

impl LocalBackend {
    pub fn new(local: Local) -> LocalBackend {
        LocalBackend {
            local,
            settings: Mutex::new(settings::defaults()),
        }
    }

    pub fn local(&self) -> &Local {
        &self.local
    }
}

impl Backend for LocalBackend {
    type Store = SqliteStore;

    async fn handle_api(
        &self,
        req: api::Request<'_>,
        _content_type: Option<&str>,
    ) -> api::Response {
        Api::new(Arc::clone(&self.local.store), None)
            .handle(req)
            .await
    }

    fn events(&self) -> Option<EventStore<SqliteStore>> {
        let source: Arc<dyn EventSource> = self.local.feed.clone();
        Some((Arc::clone(&self.local.store), source))
    }

    fn paired(&self) -> bool {
        true
    }

    fn status(&self) -> DesktopStatusDto {
        DesktopStatusDto {
            paired: true,
            server: None,
            online: true,
            phase: DesktopPhaseDto::Live,
            last_sync: None,
            warnings: Vec::new(),
        }
    }

    async fn pair(
        &self,
        _server: String,
        _code: String,
    ) -> Result<DesktopStatusDto, api::Response> {
        Err(api::Response::error(
            409,
            "already_paired",
            "this build reads a fixed local store",
        ))
    }

    async fn unpair(&self) {}

    fn settings(&self) -> DesktopSettingsDto {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn update_settings(
        &self,
        patch: DesktopSettingsPatchDto,
    ) -> Result<DesktopSettingsDto, api::Response> {
        let mut current = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        *current = settings::apply(&current, patch)?;
        Ok(current.clone())
    }

    async fn forward(&self, _method: reqwest::Method, _path: &str, _body: &[u8]) -> api::Response {
        api::Response::error(404, "not_found", "no server is paired")
    }
}
