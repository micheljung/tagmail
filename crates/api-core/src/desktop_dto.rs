//! DTOs of the desktop app's local routes (`/api/desktop/*`) and of the
//! events it emits to the SPA; the server never serves them.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::dto::ApiVersion;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DesktopPhaseDto {
    Unpaired,
    /// Importing the server's snapshot; `done` of `total` records.
    Bootstrapping {
        done: u64,
        total: u64,
    },
    Live,
    /// The server and this app cannot talk; local reads keep working.
    #[serde(rename_all = "camelCase")]
    UpdateRequired {
        server: ApiVersion,
        min_client: ApiVersion,
    },
}

/// `GET /api/desktop/status`, also the `desktop:status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DesktopStatusDto {
    pub paired: bool,
    /// Server URL.
    pub server: Option<String>,
    /// The server answered within the last 30 s.
    pub online: bool,
    pub phase: DesktopPhaseDto,
    /// Unix seconds of the last applied server change.
    pub last_sync: Option<i64>,
    /// E.g. the token is kept in a file because no keychain is available.
    pub warnings: Vec<String>,
}

/// `POST /api/desktop/pair` and the `desktop:pair` event (a
/// `tagmail://pair` link).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopPairDto {
    pub server: String,
    pub code: String,
}

/// `GET /api/desktop/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSettingsDto {
    /// Messages this recent (and starred ones) are searchable offline.
    pub window_days: u32,
    /// Disk cap of downloaded full messages.
    pub raw_cap_mb: u64,
    pub notify: bool,
    /// Show sender and subject in notifications.
    pub notify_content: bool,
    /// Calendar reminders on this device; the server sends none when off.
    pub reminders: bool,
    pub autostart: bool,
    pub close_to_tray: bool,
}

/// `PATCH /api/desktop/settings`: the fields to change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields)]
pub struct DesktopSettingsPatchDto {
    pub window_days: Option<u32>,
    pub raw_cap_mb: Option<u64>,
    pub notify: Option<bool>,
    pub notify_content: Option<bool>,
    pub reminders: Option<bool>,
    pub autostart: Option<bool>,
    pub close_to_tray: Option<bool>,
}

/// The `desktop:update-required` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRequiredDto {
    pub server: ApiVersion,
    pub min_client: ApiVersion,
}

/// The `desktop:mailto` event: a `mailto:` link to open in compose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MailtoDto {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn phases_are_tagged_by_kind() {
        let v = ApiVersion { major: 1, minor: 1 };
        for (phase, value) in [
            (DesktopPhaseDto::Unpaired, json!({"kind": "unpaired"})),
            (
                DesktopPhaseDto::Bootstrapping { done: 3, total: 9 },
                json!({"kind": "bootstrapping", "done": 3, "total": 9}),
            ),
            (DesktopPhaseDto::Live, json!({"kind": "live"})),
            (
                DesktopPhaseDto::UpdateRequired {
                    server: v,
                    min_client: v,
                },
                json!({"kind": "updateRequired", "server": {"major": 1, "minor": 1},
                       "minClient": {"major": 1, "minor": 1}}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&phase).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<DesktopPhaseDto>(value).unwrap(),
                phase
            );
        }
    }
}
