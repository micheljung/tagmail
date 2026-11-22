//! Desktop pairing (plan §3.12): a logged-in browser session creates a
//! short-lived single-use code; the desktop app's Rust side exchanges it for
//! a device token.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::Response as HttpResponse;
use tagmail_api_core::Response;
use tagmail_api_core::auth_dto::{DeviceTokenDto, PairingCodeDto, PairingExchangeDto};
use tagmail_store::rusqlite::params;

use crate::auth::{Principal, error, internal, invalid, is_browser, parse, require_session};
use crate::http::{AppState, to_http};
use crate::session;

/// Crockford base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 8;
pub const CODE_TTL_SECS: i64 = 5 * 60;
pub const MAX_DEVICE_NAME: usize = 100;

fn new_code() -> String {
    session::random_bytes::<CODE_LEN>()
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(b % 32)]))
        .collect()
}

/// Upper case without separators, so `abcd-efgh` matches `ABCDEFGH`.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `POST /api/pairing`: browser sessions only, so a device token cannot
/// enroll further devices.
pub async fn create(
    State(state): State<AppState>,
    principal: axum::Extension<Principal>,
) -> HttpResponse {
    if let Err(resp) = require_session(&principal, "pair devices") {
        return *resp;
    }
    let auth = &state.auth;
    let code = new_code();
    let hash = session::hash_token(&code);
    let expires_at = session::now() + CODE_TTL_SECS;
    let stored = auth
        .db()
        .write(move |tx| {
            tx.execute(
                "DELETE FROM pairing_code WHERE expires_at <= ?1",
                [session::now()],
            )?;
            tx.execute(
                "INSERT INTO pairing_code (code_hash, expires_at) VALUES (?1, ?2)",
                params![hash.as_slice(), expires_at],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = stored {
        return internal(e);
    }
    let server = auth.public_origin().origin().ascii_serialization();
    let query: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("server", &server)
        .append_pair("code", &code)
        .finish();
    to_http(Response::json(
        201,
        &PairingCodeDto {
            code,
            expires_at,
            url: format!("tagmail://pair?{query}"),
        },
    ))
}

/// `POST /api/pairing/exchange`: consumes a code and returns a new device
/// token. Browsers are refused, so a token never reaches page scripts.
pub async fn exchange(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let auth = &state.auth;
    if is_browser(&headers) {
        return error(
            403,
            "browser_not_allowed",
            "device tokens are only issued to the desktop app",
        );
    }
    if let Err(resp) = auth.limit(peer, &headers) {
        return *resp;
    }
    let dto: PairingExchangeDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let name = dto.name.trim().to_owned();
    if name.is_empty()
        || name.chars().count() > MAX_DEVICE_NAME
        || name.chars().any(char::is_control)
    {
        return invalid(
            "name",
            format!("1 to {MAX_DEVICE_NAME} printable characters"),
        );
    }
    let hash = session::hash_token(&normalize(&dto.code));
    let token = session::new_token();
    let token_hash = session::hash_token(&token);
    let device = auth
        .db()
        .write(move |tx| {
            let now = session::now();
            let consumed = tx.execute(
                "DELETE FROM pairing_code WHERE code_hash = ?1 AND expires_at > ?2",
                params![hash.as_slice(), now],
            )?;
            if consumed == 0 {
                return Ok(None);
            }
            tx.execute(
                "INSERT INTO device (name, token_hash, created_at) VALUES (?1, ?2, ?3)",
                params![name, token_hash.as_slice(), now],
            )?;
            Ok(Some(tx.last_insert_rowid()))
        })
        .await;
    match device {
        Ok(Some(device_id)) => {
            tracing::info!(device = device_id, "device paired");
            to_http(Response::json(201, &DeviceTokenDto { device_id, token }))
        }
        Ok(None) => error(
            401,
            "invalid_pairing_code",
            "unknown or expired pairing code",
        ),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_unambiguous_alphabet() {
        let code = new_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)));
        assert_eq!(normalize(" abcd-efgh "), "ABCDEFGH");
    }
}
