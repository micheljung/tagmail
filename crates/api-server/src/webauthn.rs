//! Owner passkeys (webauthn-rs, OpenSSL-free). Ceremony state stays in
//! memory for [`CEREMONY_TTL`]; passkeys are stored as webauthn-rs
//! `Passkey` JSON in `passkey.public_key`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::Response as HttpResponse;
use tagmail_api_core::Response;
use tagmail_api_core::auth_dto::{
    NewPasskeyDto, PasskeyChallengeDto, PasskeyDto, PasskeyResponseDto, PasskeysDto,
};
use tagmail_store::rusqlite::{self, OptionalExtension, params};
use url::Url;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Uuid, Webauthn, WebauthnBuilder,
};

use crate::auth::{Principal, error, internal, invalid, no_content, parse};
use crate::http::{AppState, to_http};
use crate::session;

pub const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
/// Pending ceremonies; the oldest is dropped beyond this.
const MAX_CEREMONIES: usize = 32;
const MAX_PASSKEY_NAME: usize = 100;
const USER_ID_META: &str = "webauthn_user_id";
const USER_NAME: &str = "owner";
const USER_DISPLAY_NAME: &str = "Tagmail owner";

enum Pending {
    Register {
        state: PasskeyRegistration,
        name: String,
    },
    Login(PasskeyAuthentication),
}

struct Ceremony {
    pending: Pending,
    started: Instant,
}

pub struct Passkeys {
    webauthn: Webauthn,
    ceremonies: Mutex<HashMap<String, Ceremony>>,
}

impl Passkeys {
    /// `None` when `origin` has no domain: WebAuthn relying party ids cannot
    /// be IP addresses.
    pub fn new(origin: &Url) -> Option<Passkeys> {
        let Some(rp_id) = origin.domain() else {
            tracing::info!(%origin, "passkeys need a domain name origin; only password login is available");
            return None;
        };
        let webauthn =
            WebauthnBuilder::new(rp_id, origin).and_then(|b| b.rp_name("Tagmail").build());
        match webauthn {
            Ok(webauthn) => Some(Passkeys {
                webauthn,
                ceremonies: Mutex::new(HashMap::new()),
            }),
            Err(e) => {
                tracing::warn!(%origin, error = %e, "passkeys unavailable");
                None
            }
        }
    }

    fn start(&self, pending: Pending) -> String {
        let id = hex::encode(session::random_bytes::<16>());
        let mut ceremonies = self.ceremonies.lock().unwrap_or_else(|e| e.into_inner());
        ceremonies.retain(|_, c| c.started.elapsed() < CEREMONY_TTL);
        if ceremonies.len() >= MAX_CEREMONIES
            && let Some(oldest) = ceremonies
                .iter()
                .min_by_key(|(_, c)| c.started)
                .map(|(id, _)| id.clone())
        {
            ceremonies.remove(&oldest);
        }
        ceremonies.insert(
            id.clone(),
            Ceremony {
                pending,
                started: Instant::now(),
            },
        );
        id
    }

    /// Removes the ceremony: each one can be finished once.
    fn take(&self, id: &str) -> Option<Pending> {
        self.ceremonies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .filter(|c| c.started.elapsed() < CEREMONY_TTL)
            .map(|c| c.pending)
    }
}

fn unavailable() -> HttpResponse {
    error(
        409,
        "passkeys_unavailable",
        "passkeys need a domain name origin (--public-origin)",
    )
}

fn bad_ceremony() -> HttpResponse {
    error(
        400,
        "invalid_ceremony",
        "unknown or expired ceremony; start again",
    )
}

fn passkey_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PasskeyDto> {
    Ok(PasskeyDto {
        id: r.get(0)?,
        name: r.get(1)?,
        created_at: r.get(2)?,
    })
}

async fn stored_passkeys(state: &AppState) -> tagmail_store::Result<Vec<Passkey>> {
    let rows: Vec<Vec<u8>> = state
        .auth
        .db()
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT public_key FROM passkey ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows
        .iter()
        .filter_map(|json| match serde_json::from_slice::<Passkey>(json) {
            Ok(passkey) => Some(passkey),
            Err(e) => {
                tracing::error!(error = %e, "unreadable stored passkey skipped");
                None
            }
        })
        .collect())
}

/// The stable WebAuthn user handle of the owner.
async fn user_id(state: &AppState) -> tagmail_store::Result<Uuid> {
    let fresh = Uuid::from_bytes(session::random_bytes::<16>()).to_string();
    let id: String = state
        .auth
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
                params![USER_ID_META, fresh],
            )?;
            Ok(tx.query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [USER_ID_META],
                |r| r.get(0),
            )?)
        })
        .await?;
    Ok(Uuid::parse_str(&id).unwrap_or_default())
}

fn challenge(id: String, options: &impl serde::Serialize) -> HttpResponse {
    match serde_json::to_value(options) {
        Ok(options) => to_http(Response::json(
            200,
            &PasskeyChallengeDto {
                ceremony_id: id,
                options,
            },
        )),
        Err(e) => internal(e),
    }
}

/// `POST /api/auth/passkey/register/start` (browser session).
pub async fn register_start(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
    body: Bytes,
) -> HttpResponse {
    if !matches!(*principal, Principal::Session(..)) {
        return error(
            403,
            "session_required",
            "add passkeys from a browser session",
        );
    }
    let Some(passkeys) = &state.auth.passkeys else {
        return unavailable();
    };
    let dto: NewPasskeyDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let name = dto.name.trim().to_owned();
    if name.is_empty()
        || name.chars().count() > MAX_PASSKEY_NAME
        || name.chars().any(char::is_control)
    {
        return invalid(
            "name",
            format!("1 to {MAX_PASSKEY_NAME} printable characters"),
        );
    }
    let (user, existing) = match tokio::try_join!(user_id(&state), stored_passkeys(&state)) {
        Ok(found) => found,
        Err(e) => return internal(e),
    };
    let exclude = existing.iter().map(|p| p.cred_id().clone()).collect();
    match passkeys.webauthn.start_passkey_registration(
        user,
        USER_NAME,
        USER_DISPLAY_NAME,
        Some(exclude),
    ) {
        Ok((options, reg)) => {
            let id = passkeys.start(Pending::Register { state: reg, name });
            challenge(id, &options)
        }
        Err(e) => internal(e),
    }
}

/// `POST /api/auth/passkey/register/finish` (browser session).
pub async fn register_finish(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
    body: Bytes,
) -> HttpResponse {
    if !matches!(*principal, Principal::Session(..)) {
        return error(
            403,
            "session_required",
            "add passkeys from a browser session",
        );
    }
    let Some(passkeys) = &state.auth.passkeys else {
        return unavailable();
    };
    let dto: PasskeyResponseDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let Some(Pending::Register { state: reg, name }) = passkeys.take(&dto.ceremony_id) else {
        return bad_ceremony();
    };
    let credential: RegisterPublicKeyCredential = match serde_json::from_value(dto.credential) {
        Ok(credential) => credential,
        Err(e) => return error(400, "bad_request", format!("invalid credential: {e}")),
    };
    let passkey = match passkeys
        .webauthn
        .finish_passkey_registration(&credential, &reg)
    {
        Ok(passkey) => passkey,
        Err(e) => {
            tracing::warn!(error = %e, "passkey registration rejected");
            return error(
                400,
                "passkey_rejected",
                format!("the passkey could not be verified: {e}"),
            );
        }
    };
    let json = match serde_json::to_vec(&passkey) {
        Ok(json) => json,
        Err(e) => return internal(e),
    };
    let cred_id = passkey.cred_id().clone();
    let row = state
        .auth
        .db()
        .write(move |tx| {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO passkey (cred_id, public_key, name, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![cred_id, json, name, session::now()],
            )?;
            if inserted == 0 {
                return Ok(None);
            }
            Ok(Some(tx.query_row(
                "SELECT id, name, created_at FROM passkey WHERE id = ?1",
                [tx.last_insert_rowid()],
                passkey_row,
            )?))
        })
        .await;
    match row {
        Ok(Some(dto)) => {
            tracing::info!(passkey = dto.id, "passkey added");
            to_http(Response::json(201, &dto))
        }
        Ok(None) => error(409, "passkey_exists", "this passkey is already registered"),
        Err(e) => internal(e),
    }
}

/// `POST /api/auth/passkey/login/start` (public, rate limited).
pub async fn login_start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> HttpResponse {
    let auth = &state.auth;
    if let Err(resp) = auth.limit(peer, &headers) {
        return *resp;
    }
    let Some(passkeys) = &auth.passkeys else {
        return unavailable();
    };
    if !auth.owner_exists() {
        return auth.unauthorized();
    }
    let stored = match stored_passkeys(&state).await {
        Ok(stored) => stored,
        Err(e) => return internal(e),
    };
    if stored.is_empty() {
        return error(404, "no_passkeys", "no passkey is registered");
    }
    match passkeys.webauthn.start_passkey_authentication(&stored) {
        Ok((options, ast)) => {
            let id = passkeys.start(Pending::Login(ast));
            challenge(id, &options)
        }
        Err(e) => internal(e),
    }
}

/// `POST /api/auth/passkey/login/finish` (public, rate limited): starts a
/// browser session.
pub async fn login_finish(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let auth = &state.auth;
    if let Err(resp) = auth.limit(peer, &headers) {
        return *resp;
    }
    let Some(passkeys) = &auth.passkeys else {
        return unavailable();
    };
    let dto: PasskeyResponseDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let Some(Pending::Login(ast)) = passkeys.take(&dto.ceremony_id) else {
        return bad_ceremony();
    };
    let credential: PublicKeyCredential = match serde_json::from_value(dto.credential) {
        Ok(credential) => credential,
        Err(e) => return error(400, "bad_request", format!("invalid credential: {e}")),
    };
    let result = match passkeys
        .webauthn
        .finish_passkey_authentication(&credential, &ast)
    {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, "passkey login rejected");
            return error(
                401,
                "invalid_credentials",
                "the passkey could not be verified",
            );
        }
    };
    let cred_id = result.cred_id().clone();
    let updated = auth
        .db()
        .write(move |tx| {
            let stored: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT public_key FROM passkey WHERE cred_id = ?1",
                    [&cred_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(stored) = stored else {
                return Ok(false);
            };
            let Ok(mut passkey) = serde_json::from_slice::<Passkey>(&stored) else {
                return Ok(false);
            };
            if passkey.update_credential(&result) == Some(true)
                && let Ok(json) = serde_json::to_vec(&passkey)
            {
                tx.execute(
                    "UPDATE passkey SET public_key = ?2, sign_count = ?3 WHERE cred_id = ?1",
                    params![cred_id, json, result.counter()],
                )?;
            }
            Ok(true)
        })
        .await;
    match updated {
        Ok(true) => auth.log_in(&headers, 200).await,
        Ok(false) => error(401, "invalid_credentials", "the passkey was removed"),
        Err(e) => internal(e),
    }
}

/// `GET /api/passkeys`.
pub async fn list(State(state): State<AppState>) -> HttpResponse {
    let rows = state
        .auth
        .db()
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, name, created_at FROM passkey ORDER BY created_at, id")?;
            let rows = stmt
                .query_map([], passkey_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await;
    match rows {
        Ok(passkeys) => to_http(Response::json(200, &PasskeysDto { passkeys })),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/passkeys/{id}`.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> HttpResponse {
    let Ok(id) = id.parse::<i64>() else {
        return error(404, "not_found", "no such passkey");
    };
    let deleted = state
        .auth
        .db()
        .write(move |tx| Ok(tx.execute("DELETE FROM passkey WHERE id = ?1", [id])? > 0))
        .await;
    match deleted {
        Ok(true) => {
            tracing::info!(passkey = id, "passkey removed");
            no_content()
        }
        Ok(false) => error(404, "not_found", "no such passkey"),
        Err(e) => internal(e),
    }
}
