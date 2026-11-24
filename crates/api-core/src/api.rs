//! Transport-agnostic request handling: the server adapts axum requests to
//! [`Request`], the desktop adapts its custom-protocol requests the same way.
//! SSE is not routed here (it streams); see [`crate::events`].

use std::net::IpAddr;
use std::sync::Arc;

use serde::Serialize;
use tagmail_core::{AccountId, ChangeCursor, ThreadId};

use crate::compose::{parse_row_id, valid_address};
use crate::dto::{
    AccountDto, AccountPatchDto, AccountStateDto, AccountStatusDto, AccountsDto, ApiErrorDto,
    ApiVersion, ChangeDto, ChangesDto, NewAccountDto, StatusDto, SyncPhase, ThreadsDto, VersionDto,
};
use crate::id_index;
use crate::routes::{self, Miss, RouteId};
use crate::signing::UrlKeys;
use crate::store::{
    AccountPatch, AccountRegistry, AccountSummary, ChangePage, ImageProxy, MarkDeleting,
    NewAccount, Secret, Store, StoreError, View, ViewError,
};
use crate::writes::WriteService;

pub const API_VERSION: ApiVersion = ApiVersion { major: 1, minor: 3 };
pub const MIN_CLIENT: ApiVersion = ApiVersion { major: 1, minor: 0 };

/// Thread rows per `/api/threads` request (plan §3.11 pages of 100).
pub const MAX_THREAD_IDS: usize = 100;
pub const DEFAULT_CHANGES_LIMIT: usize = 500;
pub const MAX_CHANGES_LIMIT: usize = 5000;
const DEFAULT_MAX_CONNECTIONS: u8 = 2;
const MAX_CONNECTIONS: std::ops::RangeInclusive<u8> = 1..=4;
const MAX_NAME_LEN: usize = 200;
const MAX_USERNAME_LEN: usize = 320;
const MAX_PASSWORD_LEN: usize = 4096;
const MAX_HOSTNAME_LEN: usize = 253;
const MAX_HOST_LABEL_LEN: usize = 63;

/// Response header carrying the `<epoch>:<seq>` state of a binary index.
pub const CURSOR_HEADER: &str = "x-tagmail-cursor";
/// Set to `offline` by the desktop on a search it could only answer from
/// the local window.
pub const PARTIAL_HEADER: &str = "x-tagmail-partial";
pub const JSON: &str = "application/json";
pub const OCTET_STREAM: &str = "application/octet-stream";
/// CSP of every API response; the SPA's own CSP is [`APP_CSP`].
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";
macro_rules! app_csp {
    () => {
        "default-src 'self'; script-src 'self'; img-src 'self' data:; frame-src 'self'; \
         object-src 'none'; base-uri 'none'"
    };
}
/// CSP for the SPA document, identical on the web and in the desktop
/// custom protocol (plan §3.9, spike 4: Tauri's CSP does not cover it).
pub const APP_CSP: &str = app_csp!();
/// [`APP_CSP`] as `tagmail-server` sends it with the SPA's files.
pub const WEB_APP_CSP: &str = concat!(app_csp!(), "; frame-ancestors 'none'");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: Method,
    /// Path as received, still percent-encoded, e.g.
    /// `/api/views/search%3Aafter%3A2026%2F01%2F01/index`. Segments are
    /// decoded after splitting, so a view id may contain `/`.
    pub path: &'a str,
    /// Raw query string without `?`.
    pub query: &'a str,
    pub body: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: vec![
                ("content-type", content_type.to_owned()),
                ("cache-control", "no-store".to_owned()),
                ("x-content-type-options", "nosniff".to_owned()),
                ("content-security-policy", API_CSP.to_owned()),
            ],
            body,
        }
    }

    pub fn json(status: u16, value: &impl Serialize) -> Response {
        let body = serde_json::to_vec(value).expect("DTOs serialize infallibly");
        Response::new(status, JSON, body)
    }

    pub fn error(status: u16, code: &str, message: impl Into<String>) -> Response {
        Response::json(
            status,
            &ApiErrorDto {
                error: code.to_owned(),
                message: message.into(),
                field: None,
            },
        )
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 503 of the desktop for what needs the server; the SPA shows `message`
/// verbatim.
pub fn offline_error(message: impl Into<String>) -> Response {
    Response::error(503, "offline", message)
}

pub fn gone_error() -> ApiErrorDto {
    ApiErrorDto {
        error: "gone".to_owned(),
        message: "the cache was rebuilt or the cursor expired; reload".to_owned(),
        field: None,
    }
}

pub(crate) fn change_page_dto(
    changes: Vec<ChangeDto>,
    next: ChangeCursor,
    more: bool,
) -> ChangesDto {
    ChangesDto {
        changes,
        next: next.to_string(),
        more,
    }
}

fn invalid(field: &str, message: impl Into<String>) -> Response {
    Response::json(
        422,
        &ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.into(),
            field: Some(field.to_owned()),
        },
    )
}

pub(crate) fn bad_request(message: impl Into<String>) -> Response {
    Response::error(400, "bad_request", message)
}

fn internal(err: StoreError) -> Response {
    tracing::error!(error = %err, "store failure");
    Response::error(500, "internal", "internal error")
}

pub struct Api<S> {
    pub(crate) store: Arc<S>,
    pub(crate) registry: Option<Arc<dyn AccountRegistry>>,
    pub(crate) proxy: Option<Arc<dyn ImageProxy>>,
    pub(crate) compose: Option<Arc<dyn crate::compose::Compose>>,
    pub(crate) writes: Option<Arc<dyn WriteService>>,
    /// URL signing keys and when they were read.
    pub(crate) keys: tokio::sync::Mutex<Option<(std::time::Instant, UrlKeys)>>,
}

impl<S: Store> Api<S> {
    /// `registry` is the sync engine. Without it the API serves reads only
    /// and refuses to add accounts (`503`), since nothing would sync them.
    pub fn new(store: Arc<S>, registry: Option<Arc<dyn AccountRegistry>>) -> Api<S> {
        Api {
            store,
            registry,
            proxy: None,
            compose: None,
            writes: None,
            keys: tokio::sync::Mutex::new(None),
        }
    }

    /// Serves the write routes (ops, label changes) through `writes`;
    /// without it they answer `503`.
    pub fn with_writes(mut self, writes: Arc<dyn WriteService>) -> Api<S> {
        self.writes = Some(writes);
        self
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    pub fn image_proxy(&self) -> Option<&Arc<dyn ImageProxy>> {
        self.proxy.as_ref()
    }

    /// The raw message of row `id`, fetched from the IMAP server first if
    /// it is not cached; `Err` is the error response.
    pub async fn raw_message(&self, id: &str) -> Result<Result<Vec<u8>, Response>, StoreError> {
        let record = match self.record(id).await? {
            Ok(record) => record,
            Err(resp) => return Ok(Err(resp)),
        };
        self.raw(&record).await
    }

    pub async fn handle(&self, req: Request<'_>) -> Response {
        let Some(rest) = req.path.strip_prefix("/api/") else {
            return Response::error(404, "not_found", "no such route");
        };
        let segments = rest
            .split('/')
            .map(|s| percent_encoding::percent_decode_str(s).decode_utf8())
            .collect::<Result<Vec<_>, _>>();
        let Ok(segments) = segments else {
            return Response::error(400, "bad_request", "path is not UTF-8");
        };
        let segments: Vec<&str> = segments.iter().map(|s| s.as_ref()).collect();
        let route = match routes::resolve(req.method, &segments) {
            Ok((id, params)) => match id {
                RouteId::Version => Ok(self.version()),
                RouteId::Status => self.status().await,
                RouteId::ViewIndex => self.view_index(params[0]).await,
                RouteId::Threads => self.threads(req.query).await,
                RouteId::Changes => self.changes(req.query).await,
                RouteId::Mailboxes => self.mailboxes().await,
                RouteId::Labels => self.labels().await,
                RouteId::Accounts => self.accounts().await,
                RouteId::CreateAccount => self.create_account(req.body).await,
                RouteId::TestNewAccount => self.test_new_account(req.body).await,
                RouteId::TestAccount => self.test_account(params[0], req.body).await,
                RouteId::UpdateAccount => self.update_account(params[0], req.body).await,
                RouteId::DeleteAccount => self.delete_account(params[0]).await,
                RouteId::CreateIdentity => self.create_identity(req.body).await,
                RouteId::UpdateIdentity => self.update_identity(params[0], req.body).await,
                RouteId::DeleteIdentity => self.delete_identity(params[0]).await,
                RouteId::ExportLabels => self.export_labels().await,
                RouteId::ImportLabels => self.import_labels(req.body).await,
                RouteId::Thread => self.thread(params[0]).await,
                RouteId::Message => self.message(params[0]).await,
                RouteId::RenderMessage => self.render_message(params[0], req.query).await,
                RouteId::Part => self.part(params[0], req.query).await,
                RouteId::ProxyImage => self.proxy_image(params[0]).await,
                RouteId::ImageSenders => self.image_senders().await,
                RouteId::SetImageSender => self.set_image_sender(req.body).await,
                RouteId::Identities => self.identities().await,
                RouteId::CreateDraft => self.create_draft(req.body).await,
                RouteId::Draft => self.get_draft(params[0]).await,
                RouteId::UpdateDraft => self.update_draft(params[0], req.body).await,
                RouteId::DeleteDraft => self.delete_draft(params[0]).await,
                RouteId::DeleteAttachment => self.delete_attachment(params[0]).await,
                RouteId::Send => self.send(req.body).await,
                RouteId::Outbox => self.outbox().await,
                RouteId::CancelOutbox => self.cancel_outbox(params[0]).await,
                RouteId::RetryOutbox => self.retry_outbox(params[0]).await,
                RouteId::Reply => self.reply(params[0], req.query).await,
                RouteId::SubmitOp => self.submit_op(req.body).await,
                RouteId::Ops => self.list_ops(req.query).await,
                RouteId::Op => self.get_op(params[0]).await,
                RouteId::RetryOp => self.retry_op(params[0]).await,
                RouteId::DiscardOp => self.discard_op(params[0]).await,
                RouteId::CreateLabel => self.create_label(req.body).await,
                RouteId::UpdateLabel => self.update_label(params[0], req.body).await,
                RouteId::DeleteLabel => self.delete_label(params[0]).await,
                RouteId::LabelJobs => self.label_jobs().await,
            },
            Err(Miss::MethodNotAllowed) => Ok(Response::error(
                405,
                "method_not_allowed",
                "method not allowed",
            )),
            Err(Miss::NotFound) => Ok(Response::error(404, "not_found", "no such route")),
        };
        route.unwrap_or_else(internal)
    }

    fn version(&self) -> Response {
        Response::json(
            200,
            &VersionDto {
                api: API_VERSION,
                min_client: MIN_CLIENT,
                server: env!("CARGO_PKG_VERSION").to_owned(),
            },
        )
    }

    async fn status(&self) -> Result<Response, StoreError> {
        let accounts = self.store.accounts().await?;
        let accounts = accounts
            .into_iter()
            .map(|a| {
                let runtime = self
                    .registry
                    .as_ref()
                    .and_then(|r| r.runtime_status(a.id))
                    .unwrap_or_default();
                AccountStatusDto {
                    id: a.id.0,
                    phase: phase(&a),
                    mailboxes_total: a.mailboxes_initial + a.mailboxes_catchup + a.mailboxes_live,
                    mailboxes_live: a.mailboxes_live,
                    text_pending: a.text_pending,
                    name: a.name,
                    email: a.email,
                    status: a.status,
                    last_error: runtime.last_error,
                    capabilities: runtime.capabilities,
                    degraded: runtime.degraded,
                }
            })
            .collect();
        Ok(Response::json(200, &StatusDto { accounts }))
    }

    async fn view_index(&self, view: &str) -> Result<Response, StoreError> {
        let view = match view.parse::<View>() {
            Ok(view) => view,
            Err(ViewError::Unknown) => {
                return Ok(Response::error(404, "not_found", "no such view"));
            }
            Err(ViewError::Query(e)) => {
                return Ok(Response::error(400, "invalid_query", e.to_string()));
            }
            Err(ViewError::Scope) => {
                return Ok(bad_request(
                    "only inbox, label and search views can be scoped to an account",
                ));
            }
        };
        let index = self.store.view_index(view).await?;
        let ids: Vec<i64> = index.ids.iter().map(|id| id.0).collect();
        let body = id_index::encode(&ids)?;
        let mut resp = Response::new(200, OCTET_STREAM, body);
        resp.headers.push((CURSOR_HEADER, index.cursor.to_string()));
        Ok(resp)
    }

    async fn mailboxes(&self) -> Result<Response, StoreError> {
        Ok(Response::json(200, &self.store.mailboxes().await?))
    }

    async fn labels(&self) -> Result<Response, StoreError> {
        Ok(Response::json(200, &self.store.labels().await?))
    }

    async fn threads(&self, query: &str) -> Result<Response, StoreError> {
        let Some(raw) = query_param(query, "ids") else {
            return Ok(bad_request("missing ids"));
        };
        let ids = raw
            .split(',')
            .map(|s| s.parse::<i64>().ok().filter(|id| *id >= 0).map(ThreadId))
            .collect::<Option<Vec<_>>>();
        let Some(ids) = ids.filter(|ids| !ids.is_empty()) else {
            return Ok(bad_request("ids must be comma-separated thread ids"));
        };
        if ids.len() > MAX_THREAD_IDS {
            return Ok(bad_request(format!("at most {MAX_THREAD_IDS} ids")));
        }
        let rows = self.store.threads(ids).await?;
        Ok(Response::json(
            200,
            &ThreadsDto {
                cursor: rows.cursor.to_string(),
                threads: rows.rows,
            },
        ))
    }

    async fn changes(&self, query: &str) -> Result<Response, StoreError> {
        let Some(since) = query_param(query, "since") else {
            return Ok(bad_request("missing since"));
        };
        let Ok(since) = since.parse::<ChangeCursor>() else {
            return Ok(bad_request("since is not a cursor"));
        };
        let limit = match query_param(query, "limit") {
            None => DEFAULT_CHANGES_LIMIT,
            Some(l) => match l.parse::<usize>() {
                Ok(l) if (1..=MAX_CHANGES_LIMIT).contains(&l) => l,
                _ => {
                    return Ok(bad_request(format!(
                        "limit must be 1..={MAX_CHANGES_LIMIT}"
                    )));
                }
            },
        };
        Ok(match self.store.changes_since(since, limit).await? {
            ChangePage::Gone => Response::json(410, &gone_error()),
            ChangePage::Page {
                changes,
                next,
                more,
            } => Response::json(200, &change_page_dto(changes, next, more)),
        })
    }

    fn engine(&self) -> Result<&Arc<dyn AccountRegistry>, Response> {
        self.registry.as_ref().ok_or_else(|| {
            Response::error(
                503,
                "engine_unavailable",
                "no sync engine is running; accounts cannot be changed",
            )
        })
    }

    async fn accounts(&self) -> Result<Response, StoreError> {
        let accounts = self.store.accounts_full().await?;
        Ok(Response::json(200, &AccountsDto { accounts }))
    }

    async fn create_account(&self, body: &[u8]) -> Result<Response, StoreError> {
        let registry = match self.engine() {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let dto: NewAccountDto = match serde_json::from_slice(body) {
            Ok(dto) => dto,
            Err(e) => return Ok(bad_request(format!("invalid account JSON: {e}"))),
        };
        let account = match validate(dto, true) {
            Ok(account) => account,
            Err(resp) => return Ok(resp),
        };
        let out = AccountDto {
            id: 0,
            name: account.name.clone(),
            imap_host: account.imap_host.clone(),
            imap_port: account.imap_port,
            imap_tls: account.imap_tls,
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port,
            smtp_tls: account.smtp_tls,
            username: account.username.clone(),
            max_connections: account.max_connections,
            email: Some(account.email.clone()),
            status: AccountStateDto::Active,
        };
        let id = self.store.create_account(account).await?;
        tracing::info!(account = id.0, "account added");
        registry.account_added(id);
        Ok(Response::json(201, &AccountDto { id: id.0, ..out }))
    }

    async fn test_new_account(&self, body: &[u8]) -> Result<Response, StoreError> {
        let registry = match self.engine() {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let dto: NewAccountDto = match serde_json::from_slice(body) {
            Ok(dto) => dto,
            Err(e) => return Ok(bad_request(format!("invalid account JSON: {e}"))),
        };
        Ok(match validate(dto, false) {
            Ok(account) => Response::json(200, &registry.test_account(account).await),
            Err(resp) => resp,
        })
    }

    async fn test_account(&self, id: &str, body: &[u8]) -> Result<Response, StoreError> {
        let registry = match self.engine() {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let Some(id) = parse_row_id(id).map(AccountId) else {
            return Ok(no_account());
        };
        let patch = match parse_patch(body) {
            Ok(patch) => patch,
            Err(resp) => return Ok(resp),
        };
        let Some(stored) = self.store.account_settings(id).await? else {
            return Ok(no_account());
        };
        if patch.reuses_password_elsewhere(&stored) {
            return Ok(password_required());
        }
        Ok(Response::json(
            200,
            &registry.test_account(patch.merged(stored)).await,
        ))
    }

    async fn update_account(&self, id: &str, body: &[u8]) -> Result<Response, StoreError> {
        let registry = match self.engine() {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let Some(id) = parse_row_id(id).map(AccountId) else {
            return Ok(no_account());
        };
        let patch = match parse_patch(body) {
            Ok(patch) => patch,
            Err(resp) => return Ok(resp),
        };
        if patch.password.is_none() && patch.touches_connection() {
            let Some(stored) = self.store.account_settings(id).await? else {
                return Ok(no_account());
            };
            if patch.reuses_password_elsewhere(&stored) {
                return Ok(password_required());
            }
        }
        let restart = patch.needs_restart();
        let Some(account) = self.store.update_account(id, patch).await? else {
            return Ok(no_account());
        };
        tracing::info!(account = id.0, restart, "account updated");
        registry.account_updated(id, restart);
        Ok(Response::json(200, &account))
    }

    async fn delete_account(&self, id: &str) -> Result<Response, StoreError> {
        let registry = match self.engine() {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let Some(id) = parse_row_id(id).map(AccountId) else {
            return Ok(no_account());
        };
        match self.store.mark_deleting(id).await? {
            MarkDeleting::Missing => return Ok(no_account()),
            MarkDeleting::AlreadyDeleting => {}
            MarkDeleting::Marked => {
                tracing::info!(account = id.0, "account deleting");
                tokio::spawn(registry.account_removed(id));
            }
        }
        Ok(Response::json(202, &serde_json::json!({ "id": id.0 })))
    }
}

fn no_account() -> Response {
    Response::error(404, "not_found", "no such account")
}

/// The stored password must not follow the account to a different server
/// or login: whoever can change those could otherwise harvest it.
fn password_required() -> Response {
    invalid(
        "password",
        "re-enter the password to change the server, TLS mode or username",
    )
}

fn parse_patch(body: &[u8]) -> Checked<AccountPatch> {
    let dto: AccountPatchDto = serde_json::from_slice(body)
        .map_err(|e| bad_request(format!("invalid account JSON: {e}")))?;
    validate_patch(dto)
}

fn phase(a: &AccountSummary) -> SyncPhase {
    if a.status != "active" {
        SyncPhase::Stopped
    } else if a.mailboxes_initial > 0 {
        SyncPhase::Initial
    } else if a.mailboxes_catchup > 0 {
        SyncPhase::Catchup
    } else if a.mailboxes_live > 0 {
        SyncPhase::Live
    } else {
        SyncPhase::Pending
    }
}

pub(crate) fn query_param(query: &str, name: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= MAX_HOSTNAME_LEN
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= MAX_HOST_LABEL_LEN
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

type Checked<T> = Result<T, Response>;

fn check_name(name: &str) -> Checked<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN || name.chars().any(char::is_control)
    {
        return Err(invalid(
            "name",
            format!("1 to {MAX_NAME_LEN} printable characters"),
        ));
    }
    Ok(name.to_owned())
}

fn check_host(field: &str, host: &str) -> Checked<String> {
    if valid_host(host) {
        Ok(host.to_ascii_lowercase())
    } else {
        Err(invalid(field, "not a host name or IP address"))
    }
}

fn check_port(field: &str, port: u16) -> Checked<u16> {
    if port == 0 {
        return Err(invalid(field, "port must be 1 to 65535"));
    }
    Ok(port)
}

fn check_username(username: String) -> Checked<String> {
    if username.is_empty()
        || username.len() > MAX_USERNAME_LEN
        || username.chars().any(char::is_control)
    {
        return Err(invalid(
            "username",
            format!("1 to {MAX_USERNAME_LEN} bytes without control characters"),
        ));
    }
    Ok(username)
}

fn check_password(password: String) -> Checked<Secret> {
    let password = Secret::new(password);
    if password.expose().is_empty() || password.expose().len() > MAX_PASSWORD_LEN {
        return Err(invalid(
            "password",
            format!("1 to {MAX_PASSWORD_LEN} bytes"),
        ));
    }
    Ok(password)
}

fn check_email(email: &str) -> Checked<String> {
    let email = email.trim();
    if valid_address(email) {
        Ok(email.to_owned())
    } else {
        Err(invalid("email", "not an email address"))
    }
}

fn check_max_connections(max: u8) -> Checked<u8> {
    if !MAX_CONNECTIONS.contains(&max) {
        return Err(invalid(
            "maxConnections",
            format!("{} to {}", MAX_CONNECTIONS.start(), MAX_CONNECTIONS.end()),
        ));
    }
    Ok(max)
}

/// The address to send from: `email`, else the username if it is one.
/// `None` if neither is.
fn sender_address(email: Option<&str>, username: &str) -> Checked<Option<String>> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => check_email(email).map(Some),
        None => Ok(valid_address(username).then(|| username.to_owned())),
    }
}

fn no_sender_address() -> Response {
    invalid(
        "email",
        "the IMAP username is not an email address; enter the address to send from",
    )
}

/// `require_email`: a test logs in without sending, so it does without.
fn validate(dto: NewAccountDto, require_email: bool) -> Checked<NewAccount> {
    let name = check_name(&dto.name)?;
    let imap_host = check_host("imapHost", &dto.imap_host)?;
    let smtp_host = check_host("smtpHost", &dto.smtp_host)?;
    let imap_port = check_port("imapPort", dto.imap_port)?;
    let smtp_port = check_port("smtpPort", dto.smtp_port)?;
    let username = check_username(dto.username)?;
    let password = check_password(dto.password)?;
    let email = match sender_address(dto.email.as_deref(), &username)? {
        Some(email) => email,
        None if require_email => return Err(no_sender_address()),
        None => String::new(),
    };
    let display_name = match dto.display_name.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(n) if n.chars().count() <= MAX_NAME_LEN && !n.chars().any(char::is_control) => {
            Some(n.to_owned())
        }
        Some(_) => {
            return Err(invalid(
                "displayName",
                format!("at most {MAX_NAME_LEN} printable characters"),
            ));
        }
    };
    let max_connections =
        check_max_connections(dto.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS))?;
    Ok(NewAccount {
        name,
        imap_host,
        imap_port,
        imap_tls: dto.imap_tls,
        smtp_host,
        smtp_port,
        smtp_tls: dto.smtp_tls,
        username,
        password,
        max_connections,
        email,
        display_name,
    })
}

fn validate_patch(dto: AccountPatchDto) -> Checked<AccountPatch> {
    let status = match dto.status {
        Some(AccountStateDto::Deleting) => {
            return Err(invalid("status", "active or disabled"));
        }
        status => status,
    };
    Ok(AccountPatch {
        name: dto.name.as_deref().map(check_name).transpose()?,
        email: dto.email.as_deref().map(check_email).transpose()?,
        imap_host: dto
            .imap_host
            .as_deref()
            .map(|h| check_host("imapHost", h))
            .transpose()?,
        imap_port: dto
            .imap_port
            .map(|p| check_port("imapPort", p))
            .transpose()?,
        imap_tls: dto.imap_tls,
        smtp_host: dto
            .smtp_host
            .as_deref()
            .map(|h| check_host("smtpHost", h))
            .transpose()?,
        smtp_port: dto
            .smtp_port
            .map(|p| check_port("smtpPort", p))
            .transpose()?,
        smtp_tls: dto.smtp_tls,
        username: dto.username.map(check_username).transpose()?,
        password: dto.password.map(check_password).transpose()?,
        max_connections: dto.max_connections.map(check_max_connections).transpose()?,
        status,
    })
}

impl AccountPatch {
    /// The engine must reconnect to apply it.
    fn needs_restart(&self) -> bool {
        self.touches_connection()
            || self.password.is_some()
            || self.max_connections.is_some()
            || self.status.is_some()
    }

    fn touches_connection(&self) -> bool {
        self.imap_host.is_some()
            || self.imap_port.is_some()
            || self.imap_tls.is_some()
            || self.smtp_host.is_some()
            || self.smtp_port.is_some()
            || self.smtp_tls.is_some()
            || self.username.is_some()
    }

    /// Changes where or as whom `stored`'s password would be sent without
    /// supplying a new one.
    fn reuses_password_elsewhere(&self, stored: &NewAccount) -> bool {
        fn differs<T: PartialEq>(new: Option<&T>, old: &T) -> bool {
            new.is_some_and(|new| new != old)
        }
        self.password.is_none()
            && (differs(self.imap_host.as_ref(), &stored.imap_host)
                || differs(self.imap_port.as_ref(), &stored.imap_port)
                || differs(self.imap_tls.as_ref(), &stored.imap_tls)
                || differs(self.smtp_host.as_ref(), &stored.smtp_host)
                || differs(self.smtp_port.as_ref(), &stored.smtp_port)
                || differs(self.smtp_tls.as_ref(), &stored.smtp_tls)
                || differs(self.username.as_ref(), &stored.username))
    }

    /// `base` with this patch applied.
    fn merged(self, base: NewAccount) -> NewAccount {
        NewAccount {
            name: self.name.unwrap_or(base.name),
            imap_host: self.imap_host.unwrap_or(base.imap_host),
            imap_port: self.imap_port.unwrap_or(base.imap_port),
            imap_tls: self.imap_tls.unwrap_or(base.imap_tls),
            smtp_host: self.smtp_host.unwrap_or(base.smtp_host),
            smtp_port: self.smtp_port.unwrap_or(base.smtp_port),
            smtp_tls: self.smtp_tls.unwrap_or(base.smtp_tls),
            username: self.username.unwrap_or(base.username),
            password: self.password.unwrap_or(base.password),
            max_connections: self.max_connections.unwrap_or(base.max_connections),
            email: self.email.unwrap_or(base.email),
            display_name: base.display_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_validation() {
        for ok in [
            "imap.example.com",
            "localhost",
            "127.0.0.1",
            "::1",
            "a-b.c.",
            "xn--bcher-kva.de",
        ] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in [
            "", ".", "a..b", "-a.com", "a-.com", "a b", "a_b.com", "[::1]", "h:993",
        ] {
            assert!(!valid_host(bad), "{bad}");
        }
        assert!(!valid_host(&"a".repeat(MAX_HOST_LABEL_LEN + 1)));
    }

    #[test]
    fn query_param_decodes() {
        assert_eq!(
            query_param("since=00000000000000ab%3A3&x=1", "since").as_deref(),
            Some("00000000000000ab:3")
        );
        assert_eq!(query_param("a=1", "b"), None);
    }
}
