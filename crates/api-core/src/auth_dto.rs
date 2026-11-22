//! JSON shapes of the server's owner login, sessions, passkeys and device
//! pairing (`tagmail-server` only; the desktop's local API has no login).
//! Errors use [`crate::dto::ApiErrorDto`]; a `401` carries `setup_required`
//! before the owner exists and `unauthorized` after.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `POST /api/auth/setup`: `token` is the one-time setup token from the
/// server log.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SetupDto {
    pub token: String,
    pub password: String,
}

/// `POST /api/auth/login`.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LoginDto {
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AuthVia {
    /// Browser session cookie.
    Session,
    /// Paired desktop device token.
    Device,
}

/// A browser session. It ends at `expiresAt`, or earlier at `idleExpiresAt`
/// if unused; `idleExpiresAt` moves forward while the session is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    pub id: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub idle_expires_at: i64,
    pub user_agent: Option<String>,
    /// The session making this request.
    pub current: bool,
}

/// `GET /api/sessions`, most recently used first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionsDto {
    pub sessions: Vec<SessionDto>,
}

/// `GET /api/auth/session`, and the result of setup and logins. `session`
/// is set for [`AuthVia::Session`], `deviceId` for [`AuthVia::Device`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CurrentAuthDto {
    pub via: AuthVia,
    pub session: Option<SessionDto>,
    pub device_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDto {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

/// `GET /api/devices`: paired devices that are not revoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DevicesDto {
    pub devices: Vec<DeviceDto>,
}

/// `POST /api/pairing`: a single-use code for the desktop app. `url`
/// (`tagmail://pair?server=…&code=…`) is the QR code payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PairingCodeDto {
    pub code: String,
    pub expires_at: i64,
    pub url: String,
}

/// `POST /api/pairing/exchange`, sent by the desktop app's Rust side.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PairingExchangeDto {
    pub code: String,
    /// Device name shown in the device list.
    pub name: String,
}

/// The device token is shown once; send it as `Authorization: Bearer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeviceTokenDto {
    pub device_id: i64,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyDto {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
}

/// `GET /api/passkeys`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PasskeysDto {
    pub passkeys: Vec<PasskeyDto>,
}

/// `POST /api/auth/passkey/register/start`.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NewPasskeyDto {
    pub name: String,
}

/// A started WebAuthn ceremony. `options` is
/// `{ publicKey: PublicKeyCredentialCreationOptionsJSON }` (registration) or
/// `{ publicKey: PublicKeyCredentialRequestOptionsJSON }` (login), binary
/// fields as base64url.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyChallengeDto {
    pub ceremony_id: String,
    #[ts(type = "unknown")]
    pub options: serde_json::Value,
}

/// Finishes a ceremony; `credential` is `PublicKeyCredential.toJSON()`.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyResponseDto {
    pub ceremony_id: String,
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
}
