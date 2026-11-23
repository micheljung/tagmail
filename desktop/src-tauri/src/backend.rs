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

    /// The API over the current local store, or the response to give
    /// instead (unpaired, still bootstrapping). Built per request: a
    /// re-bootstrap closes the old store once no request holds it.
    fn api(&self) -> Result<Api<Self::Store>, api::Response>;

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

    fn api(&self) -> Result<Api<SqliteStore>, api::Response> {
        Ok(Api::new(Arc::clone(&self.local.store), None))
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
}
