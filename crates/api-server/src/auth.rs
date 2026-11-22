//! Owner authentication (plan §3.7): first-run setup, password login,
//! session cookies, device bearer tokens, CSRF checks and the middleware
//! that guards every `/api/*` route except [`is_public`] ones.
//!
//! CSRF: the session cookie is `SameSite=Strict`, and every mutating
//! request authenticated by cookie (or not at all) must carry
//! `X-Requested-With: tagmail`, which a cross-origin page cannot send
//! without a CORS preflight the server never grants. An `Origin` header, if
//! present, must be an allowed origin, and `Sec-Fetch-Site`, if present,
//! must be `same-origin` or `none`. Device tokens are bearer credentials no
//! browser attaches on its own, so their requests skip these checks;
//! requests carrying browser fetch metadata cannot use a device token.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response as HttpResponse};
use ipnet::IpNet;
use serde::de::DeserializeOwned;
use tagmail_api_core::Response;
use tagmail_api_core::auth_dto::{
    AuthVia, CurrentAuthDto, DevicesDto, LoginDto, SessionDto, SessionsDto, SetupDto,
};
use tagmail_store::ConfigDb;
use tagmail_store::rusqlite::{OptionalExtension, params};
use tokio::sync::Semaphore;
use url::Url;
use zeroize::Zeroizing;

use crate::http::{AppState, to_http};
use crate::ratelimit::{self, RateLimiter};
use crate::session::{self, Lifetimes, TokenHash};
use crate::webauthn::Passkeys;

pub const SESSION_COOKIE: &str = "tagmail_session";
/// Value of the `X-Requested-With` header every mutating request needs.
pub const CSRF_HEADER_VALUE: &str = "tagmail";
pub const DEFAULT_SESSION_IDLE: Duration = Duration::from_secs(30 * 24 * 3600);
pub const DEFAULT_SESSION_LIFETIME: Duration = Duration::from_secs(90 * 24 * 3600);
pub const MIN_PASSWORD_CHARS: usize = 8;
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// argon2id, m = 19 MiB, t = 2, p = 1 (OWASP minimum).
const ARGON2_M_KIB: u32 = 19 * 1024;
const ARGON2_T: u32 = 2;
const ARGON2_P: u32 = 1;
const SALT_BYTES: usize = 16;

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The origin browsers use, e.g. `https://mail.example.com` behind a
    /// TLS-terminating reverse proxy. Default: `http://localhost:<port>`
    /// (and the `127.0.0.1` and `[::1]` forms) for loopback use.
    pub public_origin: Option<Url>,
    /// Peers whose `X-Forwarded-For` is believed for rate limiting.
    pub trusted_proxies: Vec<IpNet>,
    /// Allow a non-loopback listen address with a plain `http` origin.
    pub insecure_http: bool,
    pub session_idle: Duration,
    pub session_lifetime: Duration,
}

impl Default for AuthConfig {
    fn default() -> AuthConfig {
        AuthConfig {
            public_origin: None,
            trusted_proxies: Vec::new(),
            insecure_http: false,
            session_idle: DEFAULT_SESSION_IDLE,
            session_lifetime: DEFAULT_SESSION_LIFETIME,
        }
    }
}

/// Checks that `origin` is a bare `http`/`https` origin.
pub fn parse_origin(origin: &str) -> anyhow::Result<Url> {
    let url = Url::parse(origin)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        anyhow::bail!("{origin} is not an origin like https://mail.example.com");
    }
    Ok(url)
}

/// A trusted proxy as a CIDR range or a single address.
pub fn parse_proxy(proxy: &str) -> anyhow::Result<IpNet> {
    match proxy.parse::<IpNet>() {
        Ok(net) => Ok(net),
        Err(_) => Ok(IpNet::from(proxy.parse::<IpAddr>()?)),
    }
}

/// Without TLS in front, passwords and session cookies would cross the
/// network in clear, so non-loopback addresses need an `https` public
/// origin (TLS terminated by a reverse proxy) or `insecure_http`.
pub fn check_listen(addr: SocketAddr, auth: &AuthConfig) -> anyhow::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    match &auth.public_origin {
        Some(origin) if origin.scheme() == "https" => Ok(()),
        Some(_) if auth.insecure_http => {
            tracing::warn!(%addr, "serving plain HTTP on a non-loopback address (--insecure-http): passwords and session cookies are not encrypted; development only");
            Ok(())
        }
        _ => anyhow::bail!(
            "refusing to listen on {addr} without TLS: put a TLS-terminating reverse proxy in \
             front and pass --public-origin https://<host>, or pass --insecure-http \
             (development only)"
        ),
    }
}

#[derive(Debug, Clone)]
pub enum Principal {
    Session(TokenHash, SessionDto),
    Device(i64),
}

impl Principal {
    fn current(&self) -> CurrentAuthDto {
        match self {
            Principal::Session(_, dto) => CurrentAuthDto {
                via: AuthVia::Session,
                session: Some(dto.clone()),
                device_id: None,
            },
            Principal::Device(id) => CurrentAuthDto {
                via: AuthVia::Device,
                session: None,
                device_id: Some(*id),
            },
        }
    }
}

pub struct Auth {
    db: Arc<ConfigDb>,
    origins: Vec<String>,
    public_origin: Url,
    secure_cookies: bool,
    trusted_proxies: Vec<IpNet>,
    lifetimes: Lifetimes,
    owner_exists: AtomicBool,
    setup_token: Mutex<Option<Zeroizing<String>>>,
    hasher: Arc<Semaphore>,
    limiter: RateLimiter,
    pub(crate) passkeys: Option<Passkeys>,
}

impl Auth {
    /// `port` is the bound port, for the default loopback origins. Logs a
    /// one-time setup token while no owner exists.
    pub async fn new(db: Arc<ConfigDb>, config: &AuthConfig, port: u16) -> anyhow::Result<Auth> {
        let (public_origin, origins) = match &config.public_origin {
            Some(origin) => (origin.clone(), vec![origin.origin().ascii_serialization()]),
            None => {
                let origins: Vec<String> = ["localhost", "127.0.0.1", "[::1]"]
                    .iter()
                    .map(|host| format!("http://{host}:{port}"))
                    .collect();
                (Url::parse(&origins[0])?, origins)
            }
        };
        let owner_exists = db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT EXISTS (SELECT 1 FROM owner)", [], |r| {
                        r.get::<_, bool>(0)
                    })?,
                )
            })
            .await?;
        let setup_token = (!owner_exists).then(|| {
            let token = Zeroizing::new(session::new_token());
            tracing::warn!(
                "no owner yet: open {} and finish setup with the one-time setup token {}",
                public_origin.origin().ascii_serialization(),
                token.as_str()
            );
            token
        });
        Ok(Auth {
            passkeys: Passkeys::new(&public_origin),
            secure_cookies: public_origin.scheme() == "https",
            db,
            origins,
            public_origin,
            trusted_proxies: config.trusted_proxies.clone(),
            lifetimes: Lifetimes {
                idle: config.session_idle,
                absolute: config.session_lifetime,
            },
            owner_exists: AtomicBool::new(owner_exists),
            setup_token: Mutex::new(setup_token),
            hasher: Arc::new(Semaphore::new(1)),
            limiter: RateLimiter::new(ratelimit::PER_IP, ratelimit::GLOBAL),
        })
    }

    pub fn db(&self) -> &Arc<ConfigDb> {
        &self.db
    }

    pub fn public_origin(&self) -> &Url {
        &self.public_origin
    }

    pub fn owner_exists(&self) -> bool {
        self.owner_exists.load(Ordering::Acquire)
    }

    /// The one-time setup token, while no owner exists.
    pub fn setup_token(&self) -> Option<String> {
        self.setup_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|t| t.as_str().to_owned())
    }

    pub(crate) fn unauthorized(&self) -> HttpResponse {
        if self.owner_exists() {
            error(401, "unauthorized", "log in first")
        } else {
            error(401, "setup_required", "no owner yet; finish setup first")
        }
    }

    /// Takes one login attempt for the client, or answers `429`.
    pub(crate) fn limit(
        &self,
        peer: SocketAddr,
        headers: &HeaderMap,
    ) -> Result<(), Box<HttpResponse>> {
        let ip: IpAddr = ratelimit::client_ip(peer.ip(), headers, &self.trusted_proxies);
        self.limiter.check(ip).map_err(|wait| {
            tracing::warn!(%ip, "login rate limit hit");
            let mut resp = error(429, "rate_limited", "too many attempts; try again later");
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(wait.as_secs().max(1)),
            );
            Box::new(resp)
        })
    }

    fn check_csrf(&self, headers: &HeaderMap) -> Result<(), Box<HttpResponse>> {
        let value = |name: &str| headers.get(name).map(|v| v.to_str().unwrap_or(""));
        if value("x-requested-with") != Some(CSRF_HEADER_VALUE) {
            return Err(Box::new(error(
                403,
                "csrf",
                format!("mutating requests need the header X-Requested-With: {CSRF_HEADER_VALUE}"),
            )));
        }
        if let Some(origin) = value("origin")
            && !self.origins.iter().any(|o| o == origin)
        {
            return Err(Box::new(error(403, "csrf", "cross-origin request")));
        }
        if let Some(site) = value("sec-fetch-site")
            && !matches!(site, "same-origin" | "none")
        {
            return Err(Box::new(error(403, "csrf", "cross-site request")));
        }
        Ok(())
    }

    /// `Err` for a bearer token that is invalid or sent by a browser.
    async fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<Principal>, Box<HttpResponse>> {
        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let token = value
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim);
            let Some(token) = token.filter(|_| !is_browser(headers)) else {
                return Err(Box::new(error(
                    401,
                    "unauthorized",
                    "invalid authorization",
                )));
            };
            return match session::device(&self.db, session::hash_token(token)).await {
                Ok(Some(id)) => Ok(Some(Principal::Device(id))),
                Ok(None) => Err(Box::new(error(
                    401,
                    "unauthorized",
                    "invalid or revoked device token",
                ))),
                Err(e) => Err(Box::new(internal(e))),
            };
        }
        let Some(token) = cookie(headers, SESSION_COOKIE) else {
            return Ok(None);
        };
        let hash = session::hash_token(token);
        match session::session(&self.db, self.lifetimes, hash).await {
            Ok(found) => Ok(found.map(|dto| Principal::Session(hash, dto))),
            Err(e) => Err(Box::new(internal(e))),
        }
    }

    fn session_cookie(&self, token: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure_cookies { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
        ))
        .expect("cookie is ASCII")
    }

    pub(crate) fn clear_cookie(&self, resp: &mut HttpResponse) {
        resp.headers_mut()
            .append(header::SET_COOKIE, self.session_cookie("", 0));
    }

    /// Starts a browser session and answers with its cookie.
    pub(crate) async fn log_in(&self, headers: &HeaderMap, status: u16) -> HttpResponse {
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match session::create_session(&self.db, self.lifetimes, user_agent).await {
            Ok((token, dto)) => {
                let mut resp = to_http(Response::json(
                    status,
                    &Principal::Session(session::hash_token(&token), dto).current(),
                ));
                resp.headers_mut().append(
                    header::SET_COOKIE,
                    self.session_cookie(&token, self.lifetimes.absolute.as_secs()),
                );
                resp
            }
            Err(e) => internal(e),
        }
    }

    async fn hash_password(&self, password: Zeroizing<String>) -> anyhow::Result<String> {
        let permit = self.hasher.clone().acquire_owned().await?;
        let salt = session::random_bytes::<SALT_BYTES>();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let salt = SaltString::encode_b64(&salt).map_err(anyhow::Error::msg)?;
            let hash = argon2()?
                .hash_password(password.as_bytes(), &salt)
                .map_err(anyhow::Error::msg)?;
            Ok(hash.to_string())
        })
        .await?
    }

    async fn verify_password(
        &self,
        password: Zeroizing<String>,
        stored: String,
    ) -> anyhow::Result<bool> {
        let permit = self.hasher.clone().acquire_owned().await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let hash = PasswordHash::new(&stored).map_err(anyhow::Error::msg)?;
            Ok(argon2()?
                .verify_password(password.as_bytes(), &hash)
                .is_ok())
        })
        .await?
    }
}

fn argon2() -> anyhow::Result<Argon2<'static>> {
    let params = Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, None).map_err(anyhow::Error::msg)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Requests from a web page carry fetch metadata; the desktop's Rust side
/// sends none.
pub(crate) fn is_browser(headers: &HeaderMap) -> bool {
    ["origin", "sec-fetch-site", "sec-fetch-mode"]
        .iter()
        .any(|h| headers.contains_key(*h))
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

pub(crate) fn error(status: u16, code: &str, message: impl Into<String>) -> HttpResponse {
    to_http(Response::error(status, code, message))
}

pub(crate) fn invalid(field: &str, message: impl Into<String>) -> HttpResponse {
    to_http(Response::json(
        422,
        &tagmail_api_core::dto::ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.into(),
            field: Some(field.to_owned()),
        },
    ))
}

pub(crate) fn internal(err: impl std::fmt::Display) -> HttpResponse {
    tracing::error!(error = %err, "auth failure");
    error(500, "internal", "internal error")
}

pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Box<HttpResponse>> {
    serde_json::from_slice(body)
        .map_err(|e| Box::new(error(400, "bad_request", format!("invalid JSON: {e}"))))
}

pub(crate) fn no_content() -> HttpResponse {
    StatusCode::NO_CONTENT.into_response()
}

pub fn is_api(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Routes reachable without credentials; everything else under `/api/` is
/// denied by default.
pub fn is_public(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => path == "/api/version",
        Method::POST => matches!(
            path,
            "/api/auth/setup"
                | "/api/auth/login"
                | "/api/auth/passkey/login/start"
                | "/api/auth/passkey/login/finish"
                | "/api/pairing/exchange"
        ),
        _ => false,
    }
}

fn is_mutating(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

pub async fn middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> HttpResponse {
    let Ok(path) = percent_encoding::percent_decode_str(req.uri().path()).decode_utf8() else {
        return error(400, "bad_request", "path is not UTF-8");
    };
    if !is_api(&path) {
        return next.run(req).await;
    }
    let public = is_public(req.method(), &path);
    let auth = &state.auth;
    let principal = match auth.authenticate(req.headers()).await {
        Ok(principal) => principal,
        Err(resp) => return *resp,
    };
    if principal.is_none() && !public {
        return auth.unauthorized();
    }
    if is_mutating(req.method())
        && !matches!(principal, Some(Principal::Device(_)))
        && let Err(resp) = auth.check_csrf(req.headers())
    {
        return *resp;
    }
    if let Some(principal) = principal {
        req.extensions_mut().insert(principal);
    }
    next.run(req).await
}

fn valid_password(password: &str) -> Result<(), Box<HttpResponse>> {
    if password.chars().count() < MIN_PASSWORD_CHARS || password.len() > MAX_PASSWORD_BYTES {
        return Err(Box::new(invalid(
            "password",
            format!("{MIN_PASSWORD_CHARS} characters to {MAX_PASSWORD_BYTES} bytes"),
        )));
    }
    Ok(())
}

/// `POST /api/auth/setup`: sets the owner password once, with the setup
/// token, and logs in.
pub async fn setup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let auth = &state.auth;
    if let Err(resp) = auth.limit(peer, &headers) {
        return *resp;
    }
    let dto: SetupDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let password = Zeroizing::new(dto.password);
    if auth.owner_exists() {
        return error(409, "already_set_up", "the owner is already set up");
    }
    let expected = auth.setup_token();
    let matches =
        expected.is_some_and(|t| session::hash_token(&t) == session::hash_token(dto.token.trim()));
    if !matches {
        return error(403, "invalid_setup_token", "wrong setup token");
    }
    if let Err(resp) = valid_password(&password) {
        return *resp;
    }
    let hash = match auth.hash_password(password).await {
        Ok(hash) => hash,
        Err(e) => return internal(e),
    };
    let inserted = auth
        .db
        .write(move |tx| {
            Ok(tx.execute(
                "INSERT OR IGNORE INTO owner (id, password_hash, created_at) VALUES (1, ?1, ?2)",
                params![hash, session::now()],
            )? > 0)
        })
        .await;
    match inserted {
        Ok(true) => {}
        Ok(false) => return error(409, "already_set_up", "the owner is already set up"),
        Err(e) => return internal(e),
    }
    auth.owner_exists.store(true, Ordering::Release);
    *auth.setup_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    tracing::info!("owner set up");
    auth.log_in(&headers, 201).await
}

/// `POST /api/auth/login` with the owner password.
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let auth = &state.auth;
    if let Err(resp) = auth.limit(peer, &headers) {
        return *resp;
    }
    let dto: LoginDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let password = Zeroizing::new(dto.password);
    if password.len() > MAX_PASSWORD_BYTES {
        return error(401, "invalid_credentials", "wrong password");
    }
    let stored = auth
        .db
        .read(|conn| {
            Ok(conn
                .query_row("SELECT password_hash FROM owner WHERE id = 1", [], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?)
        })
        .await;
    let stored = match stored {
        Ok(Some(stored)) => stored,
        Ok(None) => return auth.unauthorized(),
        Err(e) => return internal(e),
    };
    match auth.verify_password(password, stored).await {
        Ok(true) => auth.log_in(&headers, 200).await,
        Ok(false) => {
            let ip = ratelimit::client_ip(peer.ip(), &headers, &auth.trusted_proxies);
            tracing::warn!(%ip, "failed login");
            error(401, "invalid_credentials", "wrong password")
        }
        Err(e) => internal(e),
    }
}

/// `GET /api/auth/session`.
pub async fn current(principal: axum::Extension<Principal>) -> HttpResponse {
    to_http(Response::json(200, &principal.current()))
}

/// `POST /api/auth/logout`: ends the session, or revokes the device.
pub async fn logout(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
) -> HttpResponse {
    let auth = &state.auth;
    let done = match &*principal {
        Principal::Session(hash, _) => session::delete_session(&auth.db, *hash).await,
        Principal::Device(id) => session::revoke_device(&auth.db, *id).await,
    };
    if let Err(e) = done {
        return internal(e);
    }
    let mut resp = no_content();
    if matches!(*principal, Principal::Session(..)) {
        auth.clear_cookie(&mut resp);
    }
    resp
}

/// `GET /api/sessions`.
pub async fn sessions(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
) -> HttpResponse {
    let current = match &*principal {
        Principal::Session(hash, _) => Some(*hash),
        Principal::Device(_) => None,
    };
    match session::sessions(&state.auth.db, state.auth.lifetimes, current).await {
        Ok(sessions) => to_http(Response::json(200, &SessionsDto { sessions })),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/sessions/{id}`.
pub async fn revoke_session(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
    Path(id): Path<String>,
) -> HttpResponse {
    let auth = &state.auth;
    let hash: Option<TokenHash> = hex::decode(&id).ok().and_then(|b| b.try_into().ok());
    let Some(hash) = hash else {
        return error(404, "not_found", "no such session");
    };
    match session::delete_session(&auth.db, hash).await {
        Ok(true) => {
            let mut resp = no_content();
            if matches!(&*principal, Principal::Session(current, _) if *current == hash) {
                auth.clear_cookie(&mut resp);
            }
            resp
        }
        Ok(false) => error(404, "not_found", "no such session"),
        Err(e) => internal(e),
    }
}

/// `GET /api/devices`.
pub async fn devices(State(state): State<AppState>) -> HttpResponse {
    match session::devices(&state.auth.db).await {
        Ok(devices) => to_http(Response::json(200, &DevicesDto { devices })),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/devices/{id}`.
pub async fn revoke_device(State(state): State<AppState>, Path(id): Path<String>) -> HttpResponse {
    let Ok(id) = id.parse::<i64>() else {
        return error(404, "not_found", "no such device");
    };
    match session::revoke_device(&state.auth.db, id).await {
        Ok(true) => {
            tracing::info!(device = id, "device revoked");
            no_content()
        }
        Ok(false) => error(404, "not_found", "no such device"),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_routes_are_an_explicit_list() {
        assert!(is_public(&Method::GET, "/api/version"));
        assert!(is_public(&Method::POST, "/api/auth/login"));
        assert!(!is_public(&Method::GET, "/api/auth/login"));
        assert!(!is_public(&Method::GET, "/api/auth/session"));
        assert!(!is_public(&Method::POST, "/api/pairing"));
        assert!(!is_public(&Method::GET, "/api/version/"));
        assert!(is_api("/api") && is_api("/api/x") && !is_api("/apix") && !is_api("/"));
    }

    #[test]
    fn cookies_parse_from_any_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; tagmail_session=tok"),
        );
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("tok"));
        assert_eq!(cookie(&headers, "b"), None);
        let mut empty = HeaderMap::new();
        empty.append(header::COOKIE, HeaderValue::from_static("tagmail_session="));
        assert_eq!(cookie(&empty, SESSION_COOKIE), None);
    }

    #[test]
    fn origins_must_be_bare() {
        assert!(parse_origin("https://mail.example.com").is_ok());
        assert!(parse_origin("http://localhost:5173").is_ok());
        for bad in [
            "https://mail.example.com/app",
            "ftp://x",
            "https://u:p@x",
            "https://x/?q",
            "mail.example.com",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proxies_parse_as_ranges_or_addresses() {
        assert_eq!(parse_proxy("10.0.0.0/8").unwrap().to_string(), "10.0.0.0/8");
        assert_eq!(parse_proxy("10.0.0.5").unwrap().to_string(), "10.0.0.5/32");
        assert_eq!(parse_proxy("::1").unwrap().to_string(), "::1/128");
        assert!(parse_proxy("proxy.lan").is_err());
    }

    #[test]
    fn non_loopback_needs_https_or_the_insecure_flag() {
        let mut auth = AuthConfig::default();
        for ok in ["127.0.0.1:1", "[::1]:1", "127.0.0.2:1"] {
            assert!(check_listen(ok.parse().unwrap(), &auth).is_ok(), "{ok}");
        }
        let open: SocketAddr = "0.0.0.0:1".parse().unwrap();
        assert!(check_listen(open, &auth).is_err());
        auth.public_origin = Some(parse_origin("http://mail.lan").unwrap());
        assert!(check_listen(open, &auth).is_err());
        auth.insecure_http = true;
        assert!(check_listen(open, &auth).is_ok());
        auth.insecure_http = false;
        auth.public_origin = Some(parse_origin("https://mail.example.com").unwrap());
        assert!(check_listen(open, &auth).is_ok());
    }

    #[test]
    fn argon2_parameters_are_the_planned_ones() {
        let params = argon2().unwrap().params().clone();
        assert_eq!(
            (params.m_cost(), params.t_cost(), params.p_cost()),
            (19 * 1024, 2, 1)
        );
    }
}
