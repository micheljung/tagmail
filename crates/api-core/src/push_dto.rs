//! DTOs of the Web Push routes (`/api/push/*`) and of the encrypted payload
//! the service worker turns into a notification.

use serde::{Deserialize, Deserializer, Serialize};
use ts_rs::TS;

/// `GET /api/push/key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PushKeyDto {
    /// VAPID public key: base64url of the 65-byte uncompressed P-256 point,
    /// the `applicationServerKey` of `PushManager.subscribe`.
    pub public_key: String,
}

/// Base64url keys as `PushSubscription.toJSON()` gives them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PushKeysDto {
    pub p256dh: String,
    pub auth: String,
}

/// `POST /api/push/subscriptions` body. Posting a known endpoint moves it
/// to the caller's session and replaces its keys. A new endpoint naming a
/// known `oldEndpoint` replaces that subscription, keeping its settings
/// unless given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields)]
pub struct PushSubscribeDto {
    #[ts(optional = false)]
    pub endpoint: String,
    #[ts(optional = false)]
    pub keys: PushKeysDto,
    /// Show sender and subject; default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<bool>,
    /// Accounts to notify about; absent = all accounts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_ids: Option<Vec<i64>>,
    /// The endpoint this one renews (`pushsubscriptionchange`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_endpoint: Option<String>,
}

/// `PATCH /api/push/subscriptions/{id}` body: the fields to change.
/// `accountIds: null` means all accounts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushSubscriptionPatchDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub preview: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub account_ids: Option<Option<Vec<i64>>>,
}

/// Tells a field that is `null` (`Some(None)`) from one that is absent
/// (`None`, through `#[serde(default)]`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// A subscribed device. The endpoint URL is a bearer capability of the
/// push service, so only its host is returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PushSubscriptionDto {
    pub id: i64,
    pub endpoint_host: String,
    pub preview: bool,
    /// `null` = all accounts.
    pub account_ids: Option<Vec<i64>>,
    pub user_agent: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the last delivered push.
    pub last_success_at: Option<i64>,
    /// Belongs to the caller's session: this browser.
    pub current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PushKindDto {
    NewMail,
    Test,
}

/// The plaintext of a push before RFC 8291 encryption. Without preview,
/// `from`, `subject` and `threadId` are absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields)]
pub struct PushPayloadDto {
    /// Payload format version; clients treat any other than 1 as unknown.
    #[ts(optional = false)]
    pub v: u32,
    #[ts(optional = false)]
    pub kind: PushKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<i64>,
    /// Arrivals this push stands for; `from`/`subject` describe the newest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    /// Display name, else address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// App path to open on click; always starts with a single `/`.
    #[ts(optional = false)]
    pub url: String,
}

pub const PUSH_PAYLOAD_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn round_trip<T>(value: &T, expected: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_value(value).unwrap(), expected);
        assert_eq!(&serde_json::from_value::<T>(expected).unwrap(), value);
    }

    #[test]
    fn dtos_round_trip_in_camel_case() {
        round_trip(
            &PushSubscribeDto {
                endpoint: "https://push.example/x".into(),
                keys: PushKeysDto {
                    p256dh: "BP".into(),
                    auth: "au".into(),
                },
                preview: Some(false),
                account_ids: Some(vec![1, 2]),
                old_endpoint: Some("https://push.example/w".into()),
            },
            json!({"endpoint": "https://push.example/x", "keys": {"p256dh": "BP", "auth": "au"},
                   "preview": false, "accountIds": [1, 2],
                   "oldEndpoint": "https://push.example/w"}),
        );
        round_trip(
            &PushSubscriptionDto {
                id: 3,
                endpoint_host: "push.example".into(),
                preview: true,
                account_ids: None,
                user_agent: Some("UA".into()),
                created_at: 10,
                last_success_at: None,
                current: true,
            },
            json!({"id": 3, "endpointHost": "push.example", "preview": true, "accountIds": null,
                   "userAgent": "UA", "createdAt": 10, "lastSuccessAt": null, "current": true}),
        );
        round_trip(
            &PushKeyDto {
                public_key: "BK".into(),
            },
            json!({"publicKey": "BK"}),
        );
    }

    #[test]
    fn subscribe_accepts_the_browser_subscription_json() {
        let dto: PushSubscribeDto = serde_json::from_value(json!({
            "endpoint": "https://push.example/x", "expirationTime": null,
            "keys": {"p256dh": "BP", "auth": "au"}
        }))
        .unwrap();
        assert_eq!((dto.preview, dto.account_ids), (None, None));
        assert_eq!(dto.old_endpoint, None);
    }

    #[test]
    fn patch_tells_absent_account_ids_from_null() {
        let parse = |v: Value| serde_json::from_value::<PushSubscriptionPatchDto>(v).unwrap();
        assert_eq!(parse(json!({"preview": true})).account_ids, None);
        assert_eq!(parse(json!({"accountIds": null})).account_ids, Some(None));
        assert_eq!(
            parse(json!({"accountIds": [4]})).account_ids,
            Some(Some(vec![4]))
        );
        round_trip(
            &PushSubscriptionPatchDto {
                preview: None,
                account_ids: Some(Some(vec![4])),
            },
            json!({"accountIds": [4]}),
        );
        assert!(serde_json::from_value::<PushSubscriptionPatchDto>(json!({"x": 1})).is_err());
    }

    #[test]
    fn payload_omits_absent_fields() {
        round_trip(
            &PushPayloadDto {
                v: PUSH_PAYLOAD_VERSION,
                kind: PushKindDto::NewMail,
                account_id: Some(1),
                count: Some(1),
                thread_id: Some(4711),
                from: Some("Alice Example".into()),
                subject: Some("Lunch?".into()),
                url: "/thread/4711".into(),
            },
            json!({"v": 1, "kind": "newMail", "accountId": 1, "count": 1, "threadId": 4711,
                   "from": "Alice Example", "subject": "Lunch?", "url": "/thread/4711"}),
        );
        round_trip(
            &PushPayloadDto {
                v: PUSH_PAYLOAD_VERSION,
                kind: PushKindDto::Test,
                account_id: None,
                count: None,
                thread_id: None,
                from: None,
                subject: None,
                url: "/settings/notifications".into(),
            },
            json!({"v": 1, "kind": "test", "url": "/settings/notifications"}),
        );
    }
}
