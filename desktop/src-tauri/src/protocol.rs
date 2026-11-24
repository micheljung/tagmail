//! The `app://localhost` protocol: one origin for the SPA and `/api/*`
//! (spike 4). Static files come from [`Assets`] with the SPA's CSP, desktop
//! routes are answered here, everything else goes to the api-core [`Api`].

use std::sync::Arc;

use http::{Method, StatusCode, header};
use serde::de::DeserializeOwned;
use tagmail_api_core::api::{self, API_CSP, APP_CSP};
use tagmail_api_core::auth_dto::{AuthVia, CurrentAuthDto};
use tagmail_api_core::desktop_dto::{DesktopPairDto, DesktopSettingsPatchDto};
use tagmail_core::ChangeCursor;

use crate::backend::Backend;
use crate::events::EventControl;
use crate::mailto::LinkQueue;

pub const INDEX: &str = "index.html";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Asset,
    /// `/api/desktop/*`, served by the shell.
    Desktop,
    /// `/api/auth/session`, answered from the pairing state.
    Session,
    /// Browser credential management and the SSE stream; they belong to
    /// the server and are managed on the web.
    ServerOnly,
    Api,
}

pub fn classify(path: &str) -> Route {
    let rest = match path.strip_prefix("/api") {
        Some("") => return Route::Api,
        Some(rest) => match rest.strip_prefix('/') {
            Some(rest) => rest,
            None => return Route::Asset,
        },
        None => return Route::Asset,
    };
    match rest.split('/').next().unwrap_or("") {
        "desktop" => Route::Desktop,
        "auth" if rest == "auth/session" => Route::Session,
        "auth" | "passkeys" | "sessions" | "devices" | "pairing" | "events" => Route::ServerOnly,
        _ => Route::Api,
    }
}

pub struct Asset {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// The built SPA (`web/build`): Tauri's embedded assets in the app, a map
/// in tests.
pub trait Assets: Send + Sync {
    fn get(&self, path: &str) -> Option<Asset>;
}

pub type HttpResponse = http::Response<Vec<u8>>;

pub struct Shell<B> {
    backend: Arc<B>,
    assets: Box<dyn Assets>,
    events: EventControl,
    index_suffix: Option<String>,
    links: Option<Arc<LinkQueue>>,
}

impl<B: Backend> Shell<B> {
    pub fn new(backend: Arc<B>, assets: Box<dyn Assets>, events: EventControl) -> Shell<B> {
        Shell {
            backend,
            assets,
            events,
            index_suffix: None,
            links: None,
        }
    }

    /// Links (mailto, pair) delivered once the SPA asked for the status.
    pub fn with_links(mut self, links: Arc<LinkQueue>) -> Shell<B> {
        self.links = Some(links);
        self
    }

    /// Appended to the `<head>` of HTML assets (the SPA has only
    /// `index.html`), e.g. the self-test script.
    pub fn with_index_head(mut self, html: String) -> Shell<B> {
        self.index_suffix = Some(html);
        self
    }

    pub async fn handle(&self, req: &http::Request<Vec<u8>>) -> HttpResponse {
        let path = req.uri().path();
        let query = req.uri().query().unwrap_or("");
        to_http(match classify(path) {
            Route::Asset => return self.asset(req.method(), path),
            Route::Desktop => match self.desktop(req.method(), path, query, req.body()).await {
                Some(resp) => resp,
                None => return plain(StatusCode::NO_CONTENT),
            },
            Route::Session => self.session(),
            Route::ServerOnly => api::Response::error(404, "not_found", "manage this on the web"),
            Route::Api => {
                let content_type = req
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok());
                let request = api::Request {
                    method: method(req.method()),
                    path,
                    query,
                    body: req.body(),
                };
                self.backend.handle_api(request, content_type).await
            }
        })
    }

    fn asset(&self, method: &Method, path: &str) -> HttpResponse {
        if method != Method::GET && method != Method::HEAD {
            return plain(StatusCode::METHOD_NOT_ALLOWED);
        }
        let relative = path.trim_start_matches('/');
        let relative = if relative.is_empty() { INDEX } else { relative };
        let last = relative.rsplit('/').next().unwrap_or("");
        let Some(asset) = self
            .assets
            .get(relative)
            .or_else(|| (!last.contains('.')).then(|| self.assets.get(INDEX))?)
        else {
            return plain(StatusCode::NOT_FOUND);
        };
        let mut bytes = asset.bytes;
        if asset.mime.starts_with("text/html")
            && let Some(extra) = &self.index_suffix
        {
            bytes = inject_head(&bytes, extra);
        }
        http::Response::builder()
            .header(header::CONTENT_TYPE, asset.mime)
            .header(header::CONTENT_SECURITY_POLICY, APP_CSP)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(bytes)
            .expect("valid response")
    }

    /// `None` is `204 No Content`.
    async fn desktop(
        &self,
        method: &Method,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Option<api::Response> {
        let backend = &self.backend;
        Some(match (method, path.trim_start_matches("/api/desktop/")) {
            (&Method::GET, "events/start") => {
                let since = form_urlencoded::parse(query.as_bytes())
                    .find(|(k, _)| k == "since")
                    .map(|(_, v)| v.parse::<ChangeCursor>());
                match since {
                    Some(Err(_)) => {
                        api::Response::error(400, "bad_request", "since is not a cursor")
                    }
                    Some(Ok(cursor)) => {
                        self.events.start(Some(cursor));
                        return None;
                    }
                    None => {
                        self.events.start(None);
                        return None;
                    }
                }
            }
            (&Method::GET, "status") => {
                let status = api::Response::json(200, &backend.status());
                if let Some(links) = &self.links {
                    links.ready();
                }
                status
            }
            (&Method::POST, "pair") => match parse::<DesktopPairDto>(body) {
                Ok(dto) => match backend.pair(dto.server, dto.code).await {
                    Ok(status) => api::Response::json(200, &status),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            },
            (&Method::POST, "unpair") => {
                backend.unpair().await;
                return None;
            }
            (&Method::GET, "settings") => api::Response::json(200, &backend.settings()),
            (&Method::PATCH, "settings") => match parse::<DesktopSettingsPatchDto>(body) {
                Ok(patch) => match backend.update_settings(patch) {
                    Ok(settings) => api::Response::json(200, &settings),
                    Err(resp) => resp,
                },
                Err(resp) => resp,
            },
            _ => api::Response::error(404, "not_found", "no such route"),
        })
    }

    fn session(&self) -> api::Response {
        if !self.backend.paired() {
            return api::Response::error(401, "unpaired", "pair this device with your server");
        }
        api::Response::json(
            200,
            &CurrentAuthDto {
                via: AuthVia::Device,
                session: None,
                device_id: None,
            },
        )
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, api::Response> {
    serde_json::from_slice(body)
        .map_err(|e| api::Response::error(400, "bad_request", format!("invalid JSON: {e}")))
}

fn inject_head(html: &[u8], extra: &str) -> Vec<u8> {
    let marker = b"</head>";
    match html.windows(marker.len()).position(|w| w == marker) {
        Some(at) => [&html[..at], extra.as_bytes(), &html[at..]].concat(),
        None => [html, extra.as_bytes()].concat(),
    }
}

fn method(method: &Method) -> api::Method {
    match *method {
        Method::GET | Method::HEAD => api::Method::Get,
        Method::POST => api::Method::Post,
        Method::PUT => api::Method::Put,
        Method::PATCH => api::Method::Patch,
        Method::DELETE => api::Method::Delete,
        _ => api::Method::Other,
    }
}

pub fn to_http(resp: api::Response) -> HttpResponse {
    let mut builder = http::Response::builder().status(resp.status);
    for (name, value) in &resp.headers {
        builder = builder.header(*name, value);
    }
    builder.body(resp.body).expect("api-core headers are valid")
}

fn plain(status: StatusCode) -> HttpResponse {
    http::Response::builder()
        .status(status)
        .header(header::CONTENT_SECURITY_POLICY, API_CSP)
        .body(Vec::new())
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::backend::LocalBackend;
    use crate::desktop::Desktop;
    use crate::events::{Emit, Starts};
    use crate::state;

    struct MapAssets(HashMap<&'static str, (&'static str, &'static str)>);

    impl Assets for MapAssets {
        fn get(&self, path: &str) -> Option<Asset> {
            self.0.get(path).map(|(mime, body)| Asset {
                bytes: body.as_bytes().to_vec(),
                mime: (*mime).to_owned(),
            })
        }
    }

    fn assets() -> Box<MapAssets> {
        Box::new(MapAssets(HashMap::from([
            (
                INDEX,
                ("text/html", "<html><head></head><body>spa</body></html>"),
            ),
            ("_app/app.js", ("text/javascript", "app()")),
        ])))
    }

    async fn shell() -> (TempDir, Shell<LocalBackend>, Starts) {
        let dir = TempDir::new().unwrap();
        let local = state::open(dir.path()).await.unwrap();
        let (control, starts) = EventControl::new();
        let backend = Arc::new(LocalBackend::new(local));
        (dir, Shell::new(backend, assets(), control), starts)
    }

    struct NoEmit;

    impl Emit for NoEmit {
        fn emit(&self, _event: &str, _payload: Value) {}
    }

    async fn unpaired_shell() -> (TempDir, Shell<Desktop>) {
        let dir = TempDir::new().unwrap();
        let desktop = Desktop::open(dir.path(), false, Arc::new(NoEmit))
            .await
            .unwrap();
        let (control, _) = EventControl::new();
        (dir, Shell::new(desktop, assets(), control))
    }

    async fn request<B: Backend>(
        shell: &Shell<B>,
        method: Method,
        uri: &str,
        body: &str,
    ) -> HttpResponse {
        let req = http::Request::builder()
            .method(method)
            .uri(format!("app://localhost{uri}"))
            .body(body.as_bytes().to_vec())
            .unwrap();
        shell.handle(&req).await
    }

    async fn call<B: Backend>(shell: &Shell<B>, method: Method, uri: &str) -> HttpResponse {
        request(shell, method, uri, "").await
    }

    fn json(resp: &HttpResponse) -> Value {
        serde_json::from_slice(resp.body()).unwrap()
    }

    #[test]
    fn routing_table() {
        for (path, route) in [
            ("/", Route::Asset),
            ("/inbox", Route::Asset),
            ("/_app/immutable/x.js", Route::Asset),
            ("/apix", Route::Asset),
            ("/api", Route::Api),
            ("/api/views/inbox/index", Route::Api),
            ("/api/labels", Route::Api),
            ("/api/messages/1/render", Route::Api),
            ("/api/version", Route::Api),
            ("/api/desktop/events/start", Route::Desktop),
            ("/api/desktop/status", Route::Desktop),
            ("/api/auth/session", Route::Session),
            ("/api/auth/login", Route::ServerOnly),
            ("/api/passkeys", Route::ServerOnly),
            ("/api/sessions/3", Route::ServerOnly),
            ("/api/devices", Route::ServerOnly),
            ("/api/pairing", Route::ServerOnly),
            ("/api/events", Route::ServerOnly),
        ] {
            assert_eq!(classify(path), route, "{path}");
        }
    }

    #[tokio::test]
    async fn serves_the_spa_with_its_csp_and_falls_back_to_index() {
        let (_dir, shell, _) = shell().await;
        let shell = shell.with_index_head("<script src=\"/t.js\"></script>".to_owned());
        for path in ["/", "/inbox", "/thread/7"] {
            let resp = call(&shell, Method::GET, path).await;
            assert_eq!(resp.status(), 200, "{path}");
            assert_eq!(resp.headers()[header::CONTENT_SECURITY_POLICY], APP_CSP);
            assert_eq!(
                resp.body(),
                b"<html><head><script src=\"/t.js\"></script></head><body>spa</body></html>"
            );
        }
        let js = call(&shell, Method::GET, "/_app/app.js").await;
        assert_eq!(js.body(), b"app()");
        assert_eq!(js.headers()[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(call(&shell, Method::GET, "/missing.js").await.status(), 404);
        assert_eq!(call(&shell, Method::POST, "/").await.status(), 405);
    }

    #[tokio::test]
    async fn forwards_api_routes_to_the_local_store() {
        let (_dir, shell, _) = shell().await;
        let version = call(&shell, Method::GET, "/api/version").await;
        assert_eq!(version.status(), 200);
        assert_eq!(json(&version)["api"]["major"], 1);
        let index = call(&shell, Method::GET, "/api/views/inbox/index").await;
        assert_eq!(index.status(), 200);
        assert!(index.headers().contains_key("x-tagmail-cursor"));
        assert_eq!(
            index.headers()[header::CONTENT_SECURITY_POLICY],
            tagmail_api_core::api::API_CSP
        );
        let missing = call(&shell, Method::GET, "/api/nope").await;
        assert_eq!(json(&missing)["error"], "not_found");
    }

    #[tokio::test]
    async fn keeps_server_only_routes_on_the_server() {
        let (_dir, shell, _) = shell().await;
        for path in ["/api/passkeys", "/api/devices", "/api/events"] {
            let resp = call(&shell, Method::GET, path).await;
            assert_eq!(resp.status(), 404, "{path}");
            assert_eq!(json(&resp)["message"], "manage this on the web");
        }
    }

    #[tokio::test]
    async fn answers_the_session_from_the_pairing_state() {
        let (_dir, shell, _) = shell().await;
        let paired = call(&shell, Method::GET, "/api/auth/session").await;
        assert_eq!(paired.status(), 200);
        assert_eq!(json(&paired)["via"], "device");
        let (_dir, unpaired) = unpaired_shell().await;
        for path in ["/api/auth/session", "/api/views/inbox/index"] {
            let resp = call(&unpaired, Method::GET, path).await;
            assert_eq!(resp.status(), 401, "{path}");
            assert_eq!(json(&resp)["error"], "unpaired");
        }
        let status = call(&unpaired, Method::GET, "/api/desktop/status").await;
        assert_eq!(json(&status)["paired"], false);
        assert_eq!(json(&status)["phase"]["kind"], "unpaired");
    }

    #[tokio::test]
    async fn validates_pairing_and_settings_requests() {
        let (_dir, shell) = unpaired_shell().await;
        let bad_server = request(
            &shell,
            Method::POST,
            "/api/desktop/pair",
            r#"{"server":"ftp://x","code":"K7QD2XPM"}"#,
        )
        .await;
        assert_eq!(bad_server.status(), 422);
        assert_eq!(json(&bad_server)["field"], "server");
        let unknown_field = request(
            &shell,
            Method::POST,
            "/api/desktop/pair",
            r#"{"server":"x"}"#,
        )
        .await;
        assert_eq!(unknown_field.status(), 400);

        let settings = call(&shell, Method::GET, "/api/desktop/settings").await;
        assert_eq!(json(&settings)["windowDays"], 90);
        let patched = request(
            &shell,
            Method::PATCH,
            "/api/desktop/settings",
            r#"{"notify":false,"windowDays":30}"#,
        )
        .await;
        assert_eq!(patched.status(), 200);
        assert_eq!(json(&patched)["notify"], false);
        assert_eq!(json(&patched)["windowDays"], 30);
        let rejected = request(
            &shell,
            Method::PATCH,
            "/api/desktop/settings",
            r#"{"windowDays":0}"#,
        )
        .await;
        assert_eq!(json(&rejected)["field"], "windowDays");
        let unpair = call(&shell, Method::POST, "/api/desktop/unpair").await;
        assert_eq!(unpair.status(), 204);
    }

    #[tokio::test]
    async fn events_start_takes_an_optional_cursor() {
        let (_dir, shell, mut starts) = shell().await;
        let resp = call(
            &shell,
            Method::GET,
            "/api/desktop/events/start?since=00000000000000ab:3",
        )
        .await;
        assert_eq!(resp.status(), 204);
        assert!(starts.has_changed().unwrap());
        assert_eq!(
            starts
                .borrow_and_update()
                .unwrap()
                .since
                .unwrap()
                .to_string(),
            "00000000000000ab:3"
        );
        let bad = call(&shell, Method::GET, "/api/desktop/events/start?since=x").await;
        assert_eq!(bad.status(), 400);
        assert!(!starts.has_changed().unwrap());
        call(&shell, Method::GET, "/api/desktop/events/start").await;
        assert_eq!(starts.borrow_and_update().unwrap().since, None);
        let other = call(&shell, Method::GET, "/api/desktop/nope").await;
        assert_eq!(other.status(), 404);
    }
}
