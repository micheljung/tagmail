//! The desktop's `/api` (§7): reads, writes and compose served from the
//! replica, search online through the server's index, and what only the
//! server can do (accounts, identities, labels) passed through to it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use reqwest::Method as HttpMethod;
use tagmail_api_core::api::{API_CSP, CURSOR_HEADER, JSON, OCTET_STREAM};
use tagmail_api_core::calendar_dto::{AutoApplyDto, SenderAuthDto};
use tagmail_api_core::compose::{
    Compose, MAX_UPLOAD, NewUpload, clean_content_type, clean_file_name, error_response,
};
use tagmail_api_core::dto::ApiErrorDto;
use tagmail_api_core::routes::{self, RouteId};
use tagmail_api_core::store::View;
use tagmail_api_core::writes::Submitted;
use tagmail_api_core::{
    Api, Method, PARTIAL_HEADER, Request, Response, Store, id_index, offline_error,
};
use tagmail_core::{MessageRowId, ThreadId};

use crate::cal_intents::CalKind;
use crate::client::ClientError;
use crate::compose::DesktopCompose;
use crate::store::{DesktopImages, DesktopRegistry, ReplicaStore};
use crate::sync::Replica;
use crate::writes::DesktopWrites;

pub const OLDER_RESULTS: &str = "Older results need a connection";
pub const OLDER_MESSAGE: &str = "Older message needs a connection";
pub const NEEDS_CONNECTION: &str = "This needs a connection to the server";
pub const IMAGES_OFFLINE: &str = "Images need a connection";
pub const BOOTSTRAPPING: &str = "Your mail is still downloading";
/// `x-tagmail-partial` value of an offline search.
pub const PARTIAL_OFFLINE: &str = "offline";
const UPLOAD_PATH: &str = "/api/attachments";
/// Account tests log in to IMAP and SMTP.
const PASS_THROUGH_TIMEOUT: Duration = Duration::from_secs(60);
const CATCH_UP_TIMEOUT: Duration = Duration::from_secs(5);

/// Routes only the server can answer.
fn server_only(route: RouteId) -> bool {
    matches!(
        route,
        RouteId::CreateAccount
            | RouteId::TestNewAccount
            | RouteId::TestAccount
            | RouteId::UpdateAccount
            | RouteId::DeleteAccount
            | RouteId::CreateIdentity
            | RouteId::UpdateIdentity
            | RouteId::DeleteIdentity
            | RouteId::SetImageSender
            | RouteId::CreateLabel
            | RouteId::UpdateLabel
            | RouteId::DeleteLabel
            | RouteId::LabelJobs
            | RouteId::ExportLabels
            | RouteId::ImportLabels
            | RouteId::CaldavSettings
            | RouteId::UpdateCaldavSettings
            | RouteId::DiscoverCaldav
            | RouteId::ConfirmCaldavHost
            | RouteId::UpdateCalendar
    )
}

/// Routes served from calendar records, while the replica holds them.
fn calendar_read(route: RouteId) -> bool {
    matches!(route, RouteId::Calendars | RouteId::CalendarEvents)
}

/// Calendar writes, queued as intents while the replica holds calendars.
fn calendar_write(route: RouteId) -> Option<CalKind> {
    match route {
        RouteId::CalendarRsvp => Some(CalKind::Rsvp),
        RouteId::CalendarImport => Some(CalKind::Import),
        RouteId::CalendarApply => Some(CalKind::Apply),
        _ => None,
    }
}

fn http_method(m: Method) -> HttpMethod {
    match m {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
        Method::Put => HttpMethod::PUT,
        Method::Patch => HttpMethod::PATCH,
        Method::Delete => HttpMethod::DELETE,
        Method::Other => HttpMethod::OPTIONS,
    }
}

fn binary(body: Vec<u8>) -> Response {
    Response {
        status: 200,
        headers: vec![
            ("content-type", OCTET_STREAM.to_owned()),
            ("cache-control", "no-store".to_owned()),
            ("x-content-type-options", "nosniff".to_owned()),
            ("content-security-policy", API_CSP.to_owned()),
        ],
        body,
    }
}

fn error_code(resp: &Response) -> Option<String> {
    serde_json::from_slice::<ApiErrorDto>(&resp.body)
        .ok()
        .map(|e| e.error)
}

/// Serves the SPA's `/api` requests from a replica.
#[derive(Clone)]
pub struct Router {
    replica: Arc<Replica>,
}

impl Router {
    pub fn new(replica: Arc<Replica>) -> Router {
        Router { replica }
    }

    pub fn replica(&self) -> &Arc<Replica> {
        &self.replica
    }

    /// The api-core [`Api`] over the replica's current store with the
    /// desktop's writes, compose, raw fetch and image proxy. Built per
    /// request: a re-bootstrap replaces the store.
    pub fn api(&self) -> Result<Api<ReplicaStore>, Response> {
        let store = self
            .replica
            .store()
            .ok_or_else(|| Response::error(503, "bootstrapping", BOOTSTRAPPING))?;
        let r = &self.replica;
        Ok(Api::new(
            Arc::new(ReplicaStore::new(store, r.client().is_online())),
            Some(Arc::new(DesktopRegistry(Arc::clone(r)))),
        )
        .with_writes(Arc::new(DesktopWrites(Arc::clone(r))))
        .with_compose(Arc::new(DesktopCompose(Arc::clone(r))))
        .with_image_proxy(Arc::new(DesktopImages(Arc::clone(r)))))
    }

    /// `content_type` is the request's header (multipart uploads need its
    /// boundary).
    pub async fn handle(&self, req: Request<'_>, content_type: Option<&str>) -> Response {
        if req.method == Method::Post && req.path == UPLOAD_PATH {
            return self.upload(req.body, content_type).await;
        }
        let route = route_of(req);
        if let Some((route, id)) = &route {
            let calendar = self.replica.calendar();
            if server_only(*route) || (calendar_read(*route) && !calendar) {
                return self.pass_through(req).await;
            }
            if let Some(kind) = calendar_write(*route) {
                if !calendar {
                    return self.pass_through(req).await;
                }
                return match self.replica.submit_cal(kind, req.body).await {
                    Ok(Submitted::Created(op)) => Response::json(202, &op),
                    Ok(Submitted::Existing(op)) => Response::json(200, &op),
                    Err(e) => tagmail_api_core::writes::write_error(e),
                };
            }
            if *route == RouteId::Invitation {
                return self.invitation(req, id).await;
            }
        }
        let api = match self.api() {
            Ok(api) => api,
            Err(resp) => return resp,
        };
        match route {
            Some((RouteId::ViewIndex, view)) if is_search(&view) => {
                return self.search(&api, req).await;
            }
            Some((RouteId::ProxyImage, _)) if !self.replica.client().is_online() => {
                return offline_error(IMAGES_OFFLINE);
            }
            _ => {}
        }
        let resp = api.handle(req).await;
        if resp.status == 503
            && error_code(&resp).as_deref() == Some("body_unavailable")
            && !self.replica.client().is_online()
        {
            return offline_error(OLDER_MESSAGE);
        }
        resp
    }

    /// The server judges the sender and auto-apply, so the card comes from
    /// it while online. Offline, a cached message's card is built locally
    /// without those verdicts.
    async fn invitation(&self, req: Request<'_>, id: &str) -> Response {
        if self.replica.client().is_online() {
            return self.pass_through(req).await;
        }
        let api = match self.api() {
            Ok(api) => api,
            Err(resp) => return resp,
        };
        let cached = match id.parse::<i64>() {
            Ok(id) => api.store().calendar_source(MessageRowId(id)).await,
            Err(_) => return api.handle(req).await,
        };
        match cached {
            Ok(Some(_)) => {}
            Ok(None) => return offline_error(NEEDS_CONNECTION),
            Err(e) => {
                tracing::error!(error = %e, "reading an invitation offline");
                return Response::error(500, "internal", "internal error");
            }
        }
        let mut resp = api.handle(req).await;
        if resp.status == 200
            && let Ok(mut card) = serde_json::from_slice::<serde_json::Value>(&resp.body)
        {
            card["senderAuth"] = serde_json::to_value(SenderAuthDto::Unknown).unwrap_or_default();
            card["autoApply"] = serde_json::to_value(AutoApplyDto::None).unwrap_or_default();
            resp.body = serde_json::to_vec(&card).unwrap_or(resp.body);
        }
        resp
    }

    /// Forwards the request as it came; the server's answer is returned
    /// unchanged.
    async fn pass_through(&self, req: Request<'_>) -> Response {
        let client = self.replica.client();
        let path = match req.query {
            "" => req.path.trim_start_matches('/').to_owned(),
            q => format!("{}?{q}", req.path.trim_start_matches('/')),
        };
        let builder = match client.request(http_method(req.method), &path) {
            Ok(b) => b.timeout(PASS_THROUGH_TIMEOUT),
            Err(e) => return Response::error(500, "internal", e.to_string()),
        };
        let builder = if req.body.is_empty() {
            builder
        } else {
            builder
                .header(reqwest::header::CONTENT_TYPE, JSON)
                .body(req.body.to_vec())
        };
        match client.exchange(builder).await {
            Ok((status, content_type, body)) => Response {
                status,
                headers: vec![
                    (
                        "content-type",
                        content_type.unwrap_or_else(|| JSON.to_owned()),
                    ),
                    ("cache-control", "no-store".to_owned()),
                    ("x-content-type-options", "nosniff".to_owned()),
                    ("content-security-policy", API_CSP.to_owned()),
                ],
                body,
            },
            Err(_) => offline_error(NEEDS_CONNECTION),
        }
    }

    /// Online: the server's hits (out-of-window ones included), as rows of
    /// the replica. Offline: the local index, marked partial.
    async fn search(&self, api: &Api<ReplicaStore>, req: Request<'_>) -> Response {
        if self.replica.client().is_online() || self.replica.client().probe().await {
            match self.server_search(api, req).await {
                Ok(resp) => return resp,
                Err(ClientError::Offline(_)) => {}
                Err(e) => tracing::warn!(error = %e, "server search failed; searching locally"),
            }
        }
        let mut resp = api.handle(req).await;
        if resp.status == 200 {
            resp.headers
                .push((PARTIAL_HEADER, PARTIAL_OFFLINE.to_owned()));
        }
        resp
    }

    async fn server_search(
        &self,
        api: &Api<ReplicaStore>,
        req: Request<'_>,
    ) -> Result<Response, ClientError> {
        let client = self.replica.client();
        let path = req.path.trim_start_matches('/');
        let builder = client
            .request(HttpMethod::GET, path)?
            .timeout(crate::client::REQUEST_TIMEOUT);
        let (status, _, body) = client.exchange(builder).await?;
        if status != 200 {
            return Ok(Response {
                status,
                headers: vec![("content-type", JSON.to_owned())],
                body,
            });
        }
        let ids = id_index::decode(&body).map_err(|e| ClientError::Decode(e.to_string()))?;
        let mut known = self.known(&ids).await?;
        if known.len() < ids.len() {
            match tokio::time::timeout(CATCH_UP_TIMEOUT, self.replica.sync_once()).await {
                Ok(Err(e)) => tracing::debug!(error = %e, "catching up for search"),
                Err(_) => tracing::debug!("catching up for search timed out"),
                Ok(Ok(())) => {}
            }
            known = self.known(&ids).await?;
        }
        let ids: Vec<i64> = ids.into_iter().filter(|id| known.contains(id)).collect();
        let cursor = api
            .store()
            .head()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        let mut resp =
            binary(id_index::encode(&ids).map_err(|e| ClientError::Decode(e.to_string()))?);
        resp.headers.push((CURSOR_HEADER, cursor.to_string()));
        Ok(resp)
    }

    /// Those of `ids` the replica has rows for.
    async fn known(&self, ids: &[i64]) -> Result<BTreeSet<i64>, ClientError> {
        let Some(store) = self.replica.store() else {
            return Ok(BTreeSet::new());
        };
        let mut known = BTreeSet::new();
        for chunk in ids.chunks(tagmail_api_core::api::MAX_THREAD_IDS) {
            let rows = store
                .threads(chunk.iter().copied().map(ThreadId).collect())
                .await
                .map_err(|e| ClientError::Decode(e.to_string()))?;
            known.extend(rows.rows.iter().map(|r| r.id));
        }
        Ok(known)
    }

    /// `POST /api/attachments`: the multipart `file` part into the upload
    /// directory, then into the durable store.
    async fn upload(&self, body: &[u8], content_type: Option<&str>) -> Response {
        let compose = DesktopCompose(Arc::clone(&self.replica));
        let invalid = |message: &str| {
            Response::json(
                422,
                &ApiErrorDto {
                    error: "invalid".to_owned(),
                    message: message.to_owned(),
                    field: Some("file".to_owned()),
                },
            )
        };
        let Some(boundary) = content_type.and_then(|ct| multer::parse_boundary(ct).ok()) else {
            return invalid("a multipart/form-data body is required");
        };
        let bytes = multer::bytes::Bytes::copy_from_slice(body);
        let mut multipart = multer::Multipart::new(
            stream::once(async move { Ok::<_, std::io::Error>(bytes) }),
            boundary,
        );
        let field = loop {
            match multipart.next_field().await {
                Ok(Some(field)) if field.name() == Some("file") => break field,
                Ok(Some(_)) => {}
                Ok(None) => return invalid("a `file` part is required"),
                Err(e) => return invalid(&e.to_string()),
            }
        };
        let Some(name) = field.file_name().and_then(clean_file_name) else {
            return invalid("the file needs a name");
        };
        let content_type = clean_content_type(field.content_type().map(|m| m.essence_str()));
        let data = match field.bytes().await {
            Ok(data) => data,
            Err(e) => return invalid(&e.to_string()),
        };
        if data.len() as u64 > MAX_UPLOAD {
            return Response::error(
                413,
                "too_large",
                format!("attachments are at most {MAX_UPLOAD} bytes"),
            );
        }
        let dir = compose.upload_dir();
        let id = match crate::compose::random_id() {
            Ok(id) => id,
            Err(e) => return error_response(e),
        };
        let file = dir.join(format!("upload-{id}"));
        if let Err(e) = std::fs::write(&file, &data) {
            tracing::error!(error = %e, "cannot store an upload");
            return Response::error(500, "internal", "internal error");
        }
        match compose
            .add_upload(NewUpload {
                name,
                content_type,
                file,
                size: data.len() as u64,
            })
            .await
        {
            Ok(dto) => Response::json(201, &dto),
            Err(e) => error_response(e),
        }
    }
}

fn route_of(req: Request<'_>) -> Option<(RouteId, String)> {
    let rest = req.path.strip_prefix("/api/")?;
    let segments: Vec<String> = rest
        .split('/')
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|s| s.into_owned())
        })
        .collect::<Result<_, _>>()
        .ok()?;
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (route, params) = routes::resolve(req.method, &refs).ok()?;
    Some((
        route,
        params.first().map(|p| (*p).to_owned()).unwrap_or_default(),
    ))
}

fn is_search(view: &str) -> bool {
    match view.parse::<View>() {
        Ok(View::Search(_)) => true,
        Ok(View::Account(_, inner)) => matches!(*inner, View::Search(_)),
        _ => false,
    }
}
