//! The `app://localhost` protocol: one origin for the SPA and `/api/*`
//! (spike 4). Static files come from [`Assets`] with the SPA's CSP, desktop
//! routes are answered here, everything else goes to the api-core [`Api`].

use std::sync::atomic::{AtomicBool, Ordering};

use http::{Method, StatusCode, header};
use tagmail_api_core::api::{self, API_CSP, APP_CSP, Api};
use tagmail_api_core::auth_dto::{AuthVia, CurrentAuthDto};
use tagmail_api_core::store::Store;
use tagmail_core::ChangeCursor;

use crate::events::EventControl;

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

pub struct Shell<S> {
    pub api: Api<S>,
    assets: Box<dyn Assets>,
    events: EventControl,
    paired: AtomicBool,
    index_suffix: Option<String>,
}

impl<S: Store> Shell<S> {
    pub fn new(api: Api<S>, assets: Box<dyn Assets>, events: EventControl) -> Shell<S> {
        Shell {
            api,
            assets,
            events,
            paired: AtomicBool::new(true),
            index_suffix: None,
        }
    }

    /// Appended to the `<head>` of HTML assets (the SPA has only
    /// `index.html`), e.g. the self-test script.
    pub fn with_index_head(mut self, html: String) -> Shell<S> {
        self.index_suffix = Some(html);
        self
    }

    pub fn set_paired(&self, paired: bool) {
        self.paired.store(paired, Ordering::Relaxed);
    }

    pub async fn handle(&self, req: &http::Request<Vec<u8>>) -> HttpResponse {
        let path = req.uri().path();
        let query = req.uri().query().unwrap_or("");
        match classify(path) {
            Route::Asset => self.asset(req.method(), path),
            Route::Desktop => self.desktop(req.method(), path, query),
            Route::Session => self.session(),
            Route::ServerOnly => to_http(api::Response::error(
                404,
                "not_found",
                "manage this on the web",
            )),
            Route::Api => {
                let resp = self
                    .api
                    .handle(api::Request {
                        method: method(req.method()),
                        path,
                        query,
                        body: req.body(),
                    })
                    .await;
                if resp.status == 401 {
                    self.set_paired(false);
                }
                to_http(resp)
            }
        }
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

    fn desktop(&self, method: &Method, path: &str, query: &str) -> HttpResponse {
        match (method, path) {
            (&Method::GET, "/api/desktop/events/start") => {
                let since = form_urlencoded::parse(query.as_bytes())
                    .find(|(k, _)| k == "since")
                    .map(|(_, v)| v.parse::<ChangeCursor>());
                match since {
                    Some(Err(_)) => to_http(api::Response::error(
                        400,
                        "bad_request",
                        "since is not a cursor",
                    )),
                    Some(Ok(cursor)) => {
                        self.events.start(Some(cursor));
                        plain(StatusCode::NO_CONTENT)
                    }
                    None => {
                        self.events.start(None);
                        plain(StatusCode::NO_CONTENT)
                    }
                }
            }
            _ => to_http(api::Response::error(404, "not_found", "no such route")),
        }
    }

    fn session(&self) -> HttpResponse {
        if !self.paired.load(Ordering::Relaxed) {
            return to_http(api::Response::error(
                401,
                "unpaired",
                "pair this device with your server",
            ));
        }
        to_http(api::Response::json(
            200,
            &CurrentAuthDto {
                via: AuthVia::Device,
                session: None,
                device_id: None,
            },
        ))
    }
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
    use tagmail_api_core::sqlite_store::SqliteStore;
    use tempfile::TempDir;

    use super::*;
    use crate::events::Starts;
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

    async fn shell() -> (TempDir, Shell<SqliteStore>, Starts) {
        let dir = TempDir::new().unwrap();
        let local = state::open(dir.path()).await.unwrap();
        let assets = MapAssets(HashMap::from([
            (
                INDEX,
                ("text/html", "<html><head></head><body>spa</body></html>"),
            ),
            ("_app/app.js", ("text/javascript", "app()")),
        ]));
        let (control, starts) = EventControl::new();
        let api = Api::new(Arc::clone(&local.store), None);
        (dir, Shell::new(api, Box::new(assets), control), starts)
    }

    async fn call(shell: &Shell<SqliteStore>, method: Method, uri: &str) -> HttpResponse {
        let req = http::Request::builder()
            .method(method)
            .uri(format!("app://localhost{uri}"))
            .body(Vec::new())
            .unwrap();
        shell.handle(&req).await
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
        shell.set_paired(false);
        let unpaired = call(&shell, Method::GET, "/api/auth/session").await;
        assert_eq!(unpaired.status(), 401);
        assert_eq!(json(&unpaired)["error"], "unpaired");
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
