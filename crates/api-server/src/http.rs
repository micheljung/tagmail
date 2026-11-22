//! axum adapter: auth routes and middleware, `/api/events` streams SSE,
//! every other `/api/*` request is handed to [`tagmail_api_core::Api`] and
//! the rest to the SPA.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

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

use crate::auth::{self, Auth};
use crate::sqlite_store::SqliteStore;
use crate::web::WebDir;
use crate::{pairing, webauthn};

/// Request bodies are small JSON documents in M1.
pub const MAX_BODY: usize = 64 * 1024;
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
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/events", get(events))
        .route("/api/auth/setup", post(auth::setup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/session", get(auth::current))
        .route("/api/auth/logout", post(auth::logout))
        .route(
            "/api/auth/passkey/register/start",
            post(webauthn::register_start),
        )
        .route(
            "/api/auth/passkey/register/finish",
            post(webauthn::register_finish),
        )
        .route("/api/auth/passkey/login/start", post(webauthn::login_start))
        .route(
            "/api/auth/passkey/login/finish",
            post(webauthn::login_finish),
        )
        .route("/api/passkeys", get(webauthn::list))
        .route("/api/passkeys/{id}", delete(webauthn::delete))
        .route("/api/sessions", get(auth::sessions))
        .route("/api/sessions/{id}", delete(auth::revoke_session))
        .route("/api/devices", get(auth::devices))
        .route("/api/devices/{id}", delete(auth::revoke_device))
        .route("/api/pairing", post(pairing::create))
        .route("/api/pairing/exchange", post(pairing::exchange))
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

async fn dispatch(
    State(state): State<AppState>,
    method: HttpMethod,
    uri: Uri,
    body: Bytes,
) -> HttpResponse {
    let Ok(path) = percent_encoding::percent_decode_str(uri.path()).decode_utf8() else {
        return to_http(Response::error(400, "bad_request", "path is not UTF-8"));
    };
    if auth::is_api(&path) {
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
    let method = match method {
        HttpMethod::GET => Method::Get,
        HttpMethod::POST => Method::Post,
        _ => Method::Other,
    };
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

async fn events(
    State(state): State<AppState>,
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
    let stream = stream::unfold(
        (session, slot, state.shutdown),
        |(mut session, slot, mut shutdown)| async move {
            let event = tokio::select! {
                event = session.next() => event,
                _ = shutdown.wait_for(|stop| *stop) => return None,
            };
            match event {
                Ok(Some(event)) => {
                    let mut sse = SseEvent::default().event(event.name()).data(event.data());
                    if let Some(id) = event.id() {
                        sse = sse.id(id);
                    }
                    Some((Ok::<_, Infallible>(sse), (session, slot, shutdown)))
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
}
