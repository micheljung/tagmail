//! axum adapter: auth routes and middleware, `/api/events` streams SSE,
//! every other `/api/*` request is handed to [`tagmail_api_core::Api`] and
//! the rest to the SPA.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method as HttpMethod, StatusCode, Uri};
use axum::middleware;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{delete, get, post};
use futures_util::stream;
use tagmail_api_core::{Api, EventSession, EventSource, Method, OpenError, Request, Response};
use tagmail_core::ChangeCursor;
use tokio::sync::{Semaphore, watch};
use tokio::time::Sleep;

use crate::auth::{self, Auth, Principal};
use crate::ratelimit::RateLimiter;
use crate::web::WebDir;
use crate::{pairing, upload, webauthn};
use tagmail_api_core::sqlite_store::SqliteStore;

/// Request bodies are small JSON documents, except drafts
/// ([`MAX_DRAFT_BODY`]) and attachment uploads (streamed, see
/// [`crate::upload`]).
pub const MAX_BODY: usize = 64 * 1024;
pub use tagmail_api_core::compose::MAX_DRAFT_BODY;
pub use tagmail_api_core::writes::MAX_LABEL_IMPORT;
/// SSE comment interval; keeps proxies from closing idle streams.
pub const HEARTBEAT: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct AppState {
    pub api: Arc<Api<SqliteStore>>,
    pub events: Arc<dyn EventSource>,
    pub sse_slots: Arc<Semaphore>,
    pub shutdown: watch::Receiver<bool>,
    pub auth: Arc<Auth>,
    pub web: Option<Arc<WebDir>>,
    /// Shared by every caller of the account test routes.
    pub account_tests: Arc<RateLimiter>,
}

/// Declares the routes axum serves itself as [`SERVER_ROUTES`] and builds
/// them, so the table cannot miss a route.
macro_rules! server_routes {
    ($($method:ident $path:literal => $handler:expr,)*) => {
        /// `/api` routes served here rather than by api-core's
        /// [`tagmail_api_core::routes::ROUTES`], as (method, path).
        pub const SERVER_ROUTES: &[(&str, &str)] = &[$((stringify!($method), $path)),*];

        fn server_routes(router: Router<AppState>) -> Router<AppState> {
            router$(.route($path, $method($handler)))*
        }
    };
}

server_routes! {
    get "/api/events" => events,
    post "/api/auth/setup" => auth::setup,
    post "/api/auth/login" => auth::login,
    get "/api/auth/session" => auth::current,
    post "/api/auth/logout" => auth::logout,
    post "/api/auth/passkey/register/start" => webauthn::register_start,
    post "/api/auth/passkey/register/finish" => webauthn::register_finish,
    post "/api/auth/passkey/login/start" => webauthn::login_start,
    post "/api/auth/passkey/login/finish" => webauthn::login_finish,
    get "/api/passkeys" => webauthn::list,
    delete "/api/passkeys/{id}" => webauthn::delete,
    get "/api/sessions" => auth::sessions,
    delete "/api/sessions/{id}" => auth::revoke_session,
    get "/api/devices" => auth::devices,
    delete "/api/devices/{id}" => auth::revoke_device,
    post "/api/pairing" => pairing::create,
    post "/api/pairing/exchange" => pairing::exchange,
    post "/api/attachments" => upload::upload,
}

/// Every `/api` route as (method, path template): the server's own and
/// api-core's.
pub fn api_routes() -> Vec<(HttpMethod, &'static str)> {
    let server = SERVER_ROUTES.iter().map(|(method, path)| {
        let method = HttpMethod::from_bytes(method.to_ascii_uppercase().as_bytes())
            .expect("route methods are valid");
        (method, *path)
    });
    let core = tagmail_api_core::routes::ROUTES
        .iter()
        .map(|r| (to_http_method(r.method), r.path));
    server.chain(core).collect()
}

fn to_http_method(method: Method) -> HttpMethod {
    match method {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
        Method::Put => HttpMethod::PUT,
        Method::Patch => HttpMethod::PATCH,
        Method::Delete => HttpMethod::DELETE,
        Method::Other => unreachable!("api-core routes name a concrete method"),
    }
}

fn from_http_method(method: &HttpMethod) -> Method {
    match *method {
        HttpMethod::GET => Method::Get,
        HttpMethod::POST => Method::Post,
        HttpMethod::PUT => Method::Put,
        HttpMethod::PATCH => Method::Patch,
        HttpMethod::DELETE => Method::Delete,
        _ => Method::Other,
    }
}

pub fn router(state: AppState) -> Router {
    server_routes(Router::new().route("/healthz", get(|| async { "ok" })))
        .fallback(dispatch)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::middleware,
        ))
        .layer(middleware::map_response(no_referrer))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

async fn no_referrer(mut resp: HttpResponse) -> HttpResponse {
    resp.headers_mut()
        .entry(axum::http::header::REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("no-referrer"));
    resp
}

/// Body cap of an api-core request: drafts quote whole threads.
fn body_limit(method: &HttpMethod, path: &str) -> usize {
    let draft = match *method {
        HttpMethod::POST => path == "/api/drafts",
        HttpMethod::PUT => path
            .strip_prefix("/api/drafts/")
            .is_some_and(|id| !id.contains('/')),
        _ => false,
    };
    if draft {
        MAX_DRAFT_BODY
    } else if *method == HttpMethod::POST && path == "/api/labels/import" {
        MAX_LABEL_IMPORT
    } else {
        MAX_BODY
    }
}

/// Patch keys that make the engine log in somewhere, so a patch with any of
/// them probes credentials like a test does.
const CONNECTION_KEYS: [&str; 7] = [
    "imapHost", "imapPort", "imapTls", "smtpHost", "smtpPort", "smtpTls", "username",
];

/// `POST /api/accounts/test`, `POST /api/accounts/{id}/test`, or a
/// `PATCH /api/accounts/{id}` that changes the connection.
fn is_account_test(method: &HttpMethod, path: &str, body: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix("/api/accounts/") else {
        return false;
    };
    match *method {
        HttpMethod::POST => {
            rest == "test" || rest.split_once('/').is_some_and(|(_, t)| t == "test")
        }
        HttpMethod::PATCH => {
            !rest.contains('/')
                && serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body)
                    .is_ok_and(|patch| CONNECTION_KEYS.iter().any(|k| patch.contains_key(*k)))
        }
        _ => false,
    }
}

async fn dispatch(
    State(state): State<AppState>,
    method: HttpMethod,
    uri: Uri,
    body: Body,
) -> HttpResponse {
    let Ok(path) = percent_encoding::percent_decode_str(uri.path()).decode_utf8() else {
        return to_http(Response::error(400, "bad_request", "path is not UTF-8"));
    };
    if auth::is_api(&path) {
        let body: Bytes = match axum::body::to_bytes(body, body_limit(&method, &path)).await {
            Ok(body) => body,
            Err(_) => {
                return to_http(Response::error(
                    413,
                    "too_large",
                    "the request body is too large",
                ));
            }
        };
        if is_account_test(&method, &path, &body) {
            let shared = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
            if let Err(wait) = state.account_tests.check(shared) {
                let mut resp = Response::error(
                    429,
                    "rate_limited",
                    "too many account tests; try again later",
                );
                resp.headers
                    .push(("retry-after", wait.as_secs().max(1).to_string()));
                return to_http(resp);
            }
        }
        return api(state, method, uri.path(), uri.query().unwrap_or(""), &body).await;
    }
    match &state.web {
        Some(web) => web.serve(&method, &path).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) fn to_http(resp: Response) -> HttpResponse {
    let mut out = HttpResponse::new(Body::from(resp.body));
    *out.status_mut() =
        StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in resp.headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            out.headers_mut()
                .insert(HeaderName::from_static(name), value);
        }
    }
    out
}

async fn api(
    state: AppState,
    method: HttpMethod,
    path: &str,
    query: &str,
    body: &[u8],
) -> HttpResponse {
    let method = from_http_method(&method);
    let resp = state
        .api
        .handle(Request {
            method,
            path,
            query,
            body,
        })
        .await;
    to_http(resp)
}

/// `Last-Event-ID` (browser reconnects) wins over `?since=` (first connect,
/// with the cursor of the index the client just loaded).
fn resume_cursor(headers: &HeaderMap, query: Option<&str>) -> Result<Option<ChangeCursor>, ()> {
    let header = headers
        .get("last-event-id")
        .map(|v| v.to_str().map(str::to_owned).map_err(|_| ()))
        .transpose()?;
    let since = || {
        form_urlencoded::parse(query.unwrap_or("").as_bytes())
            .find(|(k, _)| k == "since")
            .map(|(_, v)| v.into_owned())
    };
    header
        .or_else(since)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| ()))
        .transpose()
}

/// Credentials of an open event stream; the stream ends once they do.
struct StreamAuth {
    auth: Arc<Auth>,
    principal: Principal,
    revocations: watch::Receiver<u64>,
    recheck: Pin<Box<Sleep>>,
}

impl StreamAuth {
    async fn still_valid(&mut self) -> bool {
        match self.auth.stream_recheck_at(&self.principal).await {
            Some(at) => {
                self.recheck.as_mut().reset(at);
                true
            }
            None => false,
        }
    }
}

async fn events(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> HttpResponse {
    let Ok(resume) = resume_cursor(&headers, query.as_deref()) else {
        return to_http(Response::error(400, "bad_request", "invalid event id"));
    };
    let Ok(slot) = state.sse_slots.clone().try_acquire_owned() else {
        return to_http(Response::error(
            503,
            "too_many_streams",
            "too many event streams are open",
        ));
    };
    let session =
        match EventSession::open(state.api.store().clone(), state.events.as_ref(), resume).await {
            Err(e) => {
                tracing::error!(error = %e, "opening event stream");
                return to_http(Response::error(500, "internal", "internal error"));
            }
            Ok(Err(OpenError::Gone)) => {
                return to_http(Response::json(410, &tagmail_api_core::api::gone_error()));
            }
            Ok(Ok(session)) => session,
        };
    let mut guard = StreamAuth {
        revocations: state.auth.revocations(),
        auth: state.auth.clone(),
        principal,
        recheck: Box::pin(tokio::time::sleep(Duration::ZERO)),
    };
    if !guard.still_valid().await {
        return state.auth.unauthorized();
    }
    let stream = stream::unfold(
        (session, slot, state.shutdown, guard),
        |(mut session, slot, mut shutdown, mut guard)| async move {
            let event = loop {
                let recheck = tokio::select! {
                    event = session.next() => break event,
                    _ = shutdown.wait_for(|stop| *stop) => return None,
                    changed = guard.revocations.changed() => changed.is_ok(),
                    () = guard.recheck.as_mut() => true,
                };
                if !recheck || !guard.still_valid().await {
                    return None;
                }
            };
            match event {
                Ok(Some(event)) => {
                    let mut sse = SseEvent::default().event(event.name()).data(event.data());
                    if let Some(id) = event.id() {
                        sse = sse.id(id);
                    }
                    Some((Ok::<_, Infallible>(sse), (session, slot, shutdown, guard)))
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(error = %e, "event stream failed");
                    None
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_map_both_ways() {
        for method in [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
        ] {
            assert_eq!(from_http_method(&to_http_method(method)), method);
        }
        assert_eq!(from_http_method(&HttpMethod::HEAD), Method::Other);
    }

    #[test]
    fn resume_prefers_last_event_id() {
        let mut headers = HeaderMap::new();
        assert_eq!(resume_cursor(&headers, None), Ok(None));
        assert_eq!(
            resume_cursor(&headers, Some("since=0000000000000001%3A5"))
                .unwrap()
                .unwrap()
                .seq,
            5
        );
        headers.insert(
            "last-event-id",
            HeaderValue::from_static("0000000000000001:7"),
        );
        assert_eq!(
            resume_cursor(&headers, Some("since=0000000000000001:5"))
                .unwrap()
                .unwrap()
                .seq,
            7
        );
        headers.insert("last-event-id", HeaderValue::from_static("garbage"));
        assert_eq!(resume_cursor(&headers, None), Err(()));
    }

    #[test]
    fn connection_patches_count_as_account_tests() {
        let post = HttpMethod::POST;
        let patch = HttpMethod::PATCH;
        assert!(is_account_test(&post, "/api/accounts/test", b""));
        assert!(is_account_test(&post, "/api/accounts/3/test", b""));
        assert!(!is_account_test(&post, "/api/accounts", b""));
        for key in CONNECTION_KEYS {
            let body = format!(r#"{{"{key}":1}}"#);
            assert!(
                is_account_test(&patch, "/api/accounts/3", body.as_bytes()),
                "{key}"
            );
        }
        assert!(!is_account_test(
            &patch,
            "/api/accounts/3",
            br#"{"name":"x"}"#
        ));
        assert!(!is_account_test(&patch, "/api/accounts/3", b"not json"));
        assert!(!is_account_test(
            &patch,
            "/api/labels/3",
            br#"{"username":"x"}"#
        ));
        assert!(!is_account_test(
            &HttpMethod::DELETE,
            "/api/accounts/3",
            b""
        ));
    }
}
