//! Transport-agnostic request handling: the server adapts axum requests to
//! [`Request`], the desktop adapts its custom-protocol requests the same way.
//! SSE is not routed here (it streams); see [`crate::events`].

use std::net::IpAddr;
use std::sync::Arc;

use serde::Serialize;
use tagmail_core::{ChangeCursor, ThreadId};

use crate::dto::{
    AccountDto, AccountStatusDto, ApiErrorDto, ApiVersion, ChangeDto, ChangesDto, NewAccountDto,
    StatusDto, SyncPhase, ThreadsDto, VersionDto,
};
use crate::id_index;
use crate::store::{
    AccountRegistry, AccountSummary, ChangePage, NewAccount, Secret, Store, StoreError, View,
};

pub const API_VERSION: ApiVersion = ApiVersion { major: 1, minor: 0 };
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
pub const JSON: &str = "application/json";
pub const OCTET_STREAM: &str = "application/octet-stream";
/// CSP of every API response; the SPA's own CSP is [`APP_CSP`].
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";
/// CSP for the SPA document, identical on the web and in the desktop
/// custom protocol (plan §3.9, spike 4: Tauri's CSP does not cover it).
pub const APP_CSP: &str = "default-src 'self'; script-src 'self'; frame-src 'self'; \
                           object-src 'none'; base-uri 'none'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: Method,
    /// Decoded path, e.g. `/api/views/inbox/index`.
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
    fn new(status: u16, content_type: &str, body: Vec<u8>) -> Response {
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

fn bad_request(message: impl Into<String>) -> Response {
    Response::error(400, "bad_request", message)
}

fn internal(err: StoreError) -> Response {
    tracing::error!(error = %err, "store failure");
    Response::error(500, "internal", "internal error")
}

pub struct Api<S> {
    store: Arc<S>,
    registry: Option<Arc<dyn AccountRegistry>>,
}

impl<S: Store> Api<S> {
    /// `registry` is the sync engine. Without it the API serves reads only
    /// and refuses to add accounts (`503`), since nothing would sync them.
    pub fn new(store: Arc<S>, registry: Option<Arc<dyn AccountRegistry>>) -> Api<S> {
        Api { store, registry }
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    pub async fn handle(&self, req: Request<'_>) -> Response {
        let Some(rest) = req.path.strip_prefix("/api/") else {
            return Response::error(404, "not_found", "no such route");
        };
        let segments: Vec<&str> = rest.split('/').collect();
        let route = match (req.method, segments.as_slice()) {
            (Method::Get, ["version"]) => Ok(self.version()),
            (Method::Get, ["status"]) => self.status().await,
            (Method::Get, ["views", view, "index"]) => self.view_index(view).await,
            (Method::Get, ["threads"]) => self.threads(req.query).await,
            (Method::Get, ["changes"]) => self.changes(req.query).await,
            (Method::Post, ["accounts"]) => self.create_account(req.body).await,
            (_, ["version" | "status" | "threads" | "changes" | "accounts"])
            | (_, ["views", _, "index"]) => Ok(Response::error(
                405,
                "method_not_allowed",
                "method not allowed",
            )),
            _ => Ok(Response::error(404, "not_found", "no such route")),
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
        let Ok(view) = view.parse::<View>() else {
            return Ok(Response::error(404, "not_found", "no such view"));
        };
        let index = self.store.view_index(view).await?;
        let ids: Vec<i64> = index.ids.iter().map(|id| id.0).collect();
        let body = id_index::encode(&ids)?;
        let mut resp = Response::new(200, OCTET_STREAM, body);
        resp.headers.push((CURSOR_HEADER, index.cursor.to_string()));
        Ok(resp)
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

    async fn create_account(&self, body: &[u8]) -> Result<Response, StoreError> {
        let Some(registry) = &self.registry else {
            return Ok(Response::error(
                503,
                "engine_unavailable",
                "no sync engine is running; accounts cannot be added",
            ));
        };
        let dto: NewAccountDto = match serde_json::from_slice(body) {
            Ok(dto) => dto,
            Err(e) => return Ok(bad_request(format!("invalid account JSON: {e}"))),
        };
        let account = match validate(dto) {
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
        };
        let id = self.store.create_account(account).await?;
        tracing::info!(account = id.0, "account added");
        registry.account_added(id);
        Ok(Response::json(201, &AccountDto { id: id.0, ..out }))
    }
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

fn query_param(query: &str, name: &str) -> Option<String> {
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

fn validate(dto: NewAccountDto) -> Result<NewAccount, Response> {
    let name = dto.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN || name.chars().any(char::is_control)
    {
        return Err(invalid(
            "name",
            format!("1 to {MAX_NAME_LEN} printable characters"),
        ));
    }
    for (field, host) in [("imapHost", &dto.imap_host), ("smtpHost", &dto.smtp_host)] {
        if !valid_host(host) {
            return Err(invalid(field, "not a host name or IP address"));
        }
    }
    for (field, port) in [("imapPort", dto.imap_port), ("smtpPort", dto.smtp_port)] {
        if port == 0 {
            return Err(invalid(field, "port must be 1 to 65535"));
        }
    }
    if dto.username.is_empty()
        || dto.username.len() > MAX_USERNAME_LEN
        || dto.username.chars().any(char::is_control)
    {
        return Err(invalid(
            "username",
            format!("1 to {MAX_USERNAME_LEN} bytes without control characters"),
        ));
    }
    let password = Secret::new(dto.password);
    if password.expose().is_empty() || password.expose().len() > MAX_PASSWORD_LEN {
        return Err(invalid(
            "password",
            format!("1 to {MAX_PASSWORD_LEN} bytes"),
        ));
    }
    let max_connections = dto.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
    if !MAX_CONNECTIONS.contains(&max_connections) {
        return Err(invalid(
            "maxConnections",
            format!("{} to {}", MAX_CONNECTIONS.start(), MAX_CONNECTIONS.end()),
        ));
    }
    Ok(NewAccount {
        name: name.to_owned(),
        imap_host: dto.imap_host.to_ascii_lowercase(),
        imap_port: dto.imap_port,
        imap_tls: dto.imap_tls,
        smtp_host: dto.smtp_host.to_ascii_lowercase(),
        smtp_port: dto.smtp_port,
        smtp_tls: dto.smtp_tls,
        username: dto.username,
        password,
        max_connections,
    })
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
