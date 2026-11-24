//! `/api/push/*` (contract §4). Session-only: a desktop device has its own
//! notifications, and a subscription lives as long as its browser session.

use std::net::SocketAddr;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use p256::PublicKey;
use tagmail_api_core::Response;
use tagmail_api_core::push_dto::{
    PushKeyDto, PushSubscribeDto, PushSubscriptionDto, PushSubscriptionPatchDto,
};

use super::Push;
use super::ece::{AUTH_LEN, PUBLIC_KEY_LEN};
use super::store::{self, NewSubscription, Upserted};
use crate::auth::{Principal, error, internal, invalid, no_content, parse};
use crate::http::{AppState, rate_limited, to_http};
use crate::session::TokenHash;

/// Accounts one subscription may name.
pub const MAX_ACCOUNT_IDS: usize = 64;
const MAX_USER_AGENT_CHARS: usize = 256;

/// Base64url as browsers give it, with or without padding.
const KEY_B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn session_of(principal: &Principal) -> Result<TokenHash, Box<HttpResponse>> {
    match principal {
        Principal::Session(hash, _) => Ok(*hash),
        Principal::Device(_) => Err(Box::new(error(
            403,
            "session_only",
            "Notifications are set up in the browser",
        ))),
    }
}

fn limit_writes(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<(), Box<HttpResponse>> {
    let ip = state.auth.client_ip(peer, headers);
    state
        .push
        .writes
        .check(ip)
        .map_err(|wait| Box::new(rate_limited(wait, "too many changes; try again later")))
}

const ACCOUNT_IDS: &str = "accountIds";

fn account_id_set(ids: &[i64]) -> Result<Vec<i64>, Box<HttpResponse>> {
    if ids.is_empty() {
        return Err(Box::new(invalid(
            ACCOUNT_IDS,
            "name at least one account, or none for all",
        )));
    }
    if ids.len() > MAX_ACCOUNT_IDS {
        return Err(Box::new(invalid(
            ACCOUNT_IDS,
            format!("at most {MAX_ACCOUNT_IDS} accounts"),
        )));
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

async fn known_account_ids(push: &Push, ids: &[i64]) -> Result<Vec<i64>, Box<HttpResponse>> {
    let ids = account_id_set(ids)?;
    match store::unknown_accounts(&push.db, ids.clone()).await {
        Ok(unknown) if unknown.is_empty() => Ok(ids),
        Ok(unknown) => Err(Box::new(invalid(
            ACCOUNT_IDS,
            format!("unknown accounts {unknown:?}"),
        ))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn validate(
    push: &Push,
    dto: PushSubscribeDto,
    headers: &HeaderMap,
) -> Result<NewSubscription, Box<HttpResponse>> {
    if let Err(why) = push.policy.validate(&dto.endpoint) {
        return Err(Box::new(invalid("endpoint", why)));
    }
    let p256dh = KEY_B64
        .decode(dto.keys.p256dh.trim())
        .ok()
        .filter(|k| k.len() == PUBLIC_KEY_LEN && PublicKey::from_sec1_bytes(k).is_ok())
        .ok_or_else(|| {
            Box::new(invalid(
                "keys.p256dh",
                "not an uncompressed P-256 public key",
            ))
        })?;
    let auth: [u8; AUTH_LEN] = KEY_B64
        .decode(dto.keys.auth.trim())
        .ok()
        .and_then(|a| a.try_into().ok())
        .ok_or_else(|| Box::new(invalid("keys.auth", "not 16 bytes of base64url")))?;
    let account_ids = dto.account_ids.as_deref().map(account_id_set).transpose()?;
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(MAX_USER_AGENT_CHARS).collect());
    Ok(NewSubscription {
        endpoint: dto.endpoint,
        p256dh,
        auth,
        preview: dto.preview,
        account_ids,
        user_agent,
        old_endpoint: dto.old_endpoint,
    })
}

fn json(status: u16, value: &impl serde::Serialize) -> HttpResponse {
    to_http(Response::json(status, value))
}

/// `GET /api/push/key`.
pub async fn key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> HttpResponse {
    if let Err(resp) = session_of(&principal) {
        return *resp;
    }
    match state.push.vapid.get().await {
        Ok(vapid) => json(
            200,
            &PushKeyDto {
                public_key: vapid.key().public_key_b64(),
            },
        ),
        Err(e) => internal(e),
    }
}

/// `GET /api/push/subscriptions`: every device, `current` marking this one.
pub async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> HttpResponse {
    let session = match session_of(&principal) {
        Ok(session) => session,
        Err(resp) => return *resp,
    };
    match store::list(&state.push.db, Some(session)).await {
        Ok(list) => json(200, &list),
        Err(e) => internal(e),
    }
}

/// `POST /api/push/subscriptions`.
pub async fn subscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let session = match session_of(&principal) {
        Ok(session) => session,
        Err(resp) => return *resp,
    };
    if let Err(resp) = limit_writes(&state, peer, &headers) {
        return *resp;
    }
    let dto: PushSubscribeDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let sub = match validate(&state.push, dto, &headers) {
        Ok(sub) => sub,
        Err(resp) => return *resp,
    };
    match store::upsert(&state.push.db, session, sub, state.push.session_idle).await {
        Ok(Upserted::Created(dto)) => json(201, &dto),
        Ok(Upserted::Updated(dto)) => json(200, &dto),
        Ok(Upserted::TooMany) => error(
            409,
            "too_many_subscriptions",
            format!(
                "at most {} devices get notifications",
                store::MAX_SUBSCRIPTIONS
            ),
        ),
        Err(e) => internal(e),
    }
}

/// `PATCH /api/push/subscriptions/{id}`.
pub async fn patch(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: Bytes,
) -> HttpResponse {
    let session = match session_of(&principal) {
        Ok(session) => session,
        Err(resp) => return *resp,
    };
    if let Err(resp) = limit_writes(&state, peer, &headers) {
        return *resp;
    }
    let dto: PushSubscriptionPatchDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    let account_ids = match dto.account_ids {
        Some(Some(ids)) => match known_account_ids(&state.push, &ids).await {
            Ok(ids) => Some(Some(ids)),
            Err(resp) => return *resp,
        },
        other => other,
    };
    let patched: Result<Option<PushSubscriptionDto>, _> =
        store::patch(&state.push.db, id, session, dto.preview, account_ids).await;
    match patched {
        Ok(Some(dto)) => json(200, &dto),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

fn not_found() -> HttpResponse {
    error(404, "not_found", "no such subscription")
}

/// `DELETE /api/push/subscriptions/{id}`.
pub async fn delete(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> HttpResponse {
    if let Err(resp) = session_of(&principal) {
        return *resp;
    }
    if let Err(resp) = limit_writes(&state, peer, &headers) {
        return *resp;
    }
    match store::delete(&state.push.db, id).await {
        Ok(true) => no_content(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

/// `POST /api/push/subscriptions/{id}/test`: queues a test notification.
pub async fn test(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> HttpResponse {
    if let Err(resp) = session_of(&principal) {
        return *resp;
    }
    match store::exists(&state.push.db, id).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return internal(e),
    }
    if let Err(wait) = state.push.test_limit.check_key(id) {
        return rate_limited(wait, "one test notification every few seconds");
    }
    if state.push.tests.try_send(id).is_err() {
        return rate_limited(
            super::PUSH_TESTS.refill,
            "too many test notifications queued",
        );
    }
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose;

    use super::*;

    #[test]
    fn keys_decode_with_or_without_padding() {
        let raw = [7u8; AUTH_LEN];
        for encoded in [
            general_purpose::URL_SAFE_NO_PAD.encode(raw),
            general_purpose::URL_SAFE.encode(raw),
        ] {
            assert_eq!(KEY_B64.decode(encoded).unwrap(), raw);
        }
        assert!(KEY_B64.decode("a+b/").is_err(), "standard alphabet");
    }
}
