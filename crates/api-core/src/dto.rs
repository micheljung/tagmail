//! JSON shapes of the HTTP API. TypeScript bindings are generated into
//! `packages/api-types` (`cargo xtask gen-ts-types`); ids and dates are
//! exported as `number`, since cache ids and Unix seconds stay below 2^53.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

/// `GET /api/version`. A client whose own API version is older than
/// `minClient` must update before it syncs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub api: ApiVersion,
    pub min_client: ApiVersion,
    pub server: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    /// No mailbox listed yet.
    Pending,
    Initial,
    Catchup,
    Live,
    /// The account is not `active` (e.g. disabled or failing auth).
    Stopped,
}

/// `GET /api/status`, one entry per account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusDto {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub phase: SyncPhase,
    pub mailboxes_total: u32,
    pub mailboxes_live: u32,
    pub last_error: Option<String>,
    pub capabilities: Vec<String>,
    pub degraded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub accounts: Vec<AccountStatusDto>,
}

/// One address of `message.from_json`, which the engine writes as a JSON
/// array of these objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AddressDto {
    #[serde(default)]
    pub name: Option<String>,
    pub email: String,
}

/// One thread-list row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRowDto {
    pub id: i64,
    pub account_id: i64,
    /// Subject of the newest message.
    pub subject: String,
    pub snippet: String,
    /// Unix seconds.
    pub last_date: i64,
    pub message_count: u32,
    pub unread_count: u32,
    pub starred: bool,
    pub has_attachment: bool,
    /// Up to four distinct senders, oldest first.
    pub senders: Vec<AddressDto>,
    /// Label keys, sorted.
    pub labels: Vec<String>,
}

/// `GET /api/threads?ids=`: rows as of `cursor`. Ids without a row (deleted
/// or merged since the index was fetched) are absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ThreadsDto {
    pub cursor: String,
    pub threads: Vec<ThreadRowDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EntityDto {
    Account,
    Mailbox,
    Thread,
    Message,
    Label,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ChangeOpDto {
    Upsert,
    Delete,
    /// Merged into the thread `target`.
    Merged {
        target: i64,
    },
    /// Many entities of `mailbox` changed: refetch views containing it.
    Bulk {
        mailbox: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ChangeDto {
    pub seq: i64,
    pub entity: EntityDto,
    pub id: i64,
    pub op: ChangeOpDto,
    /// Entity-specific details, e.g. a thread's `lastDate` and view ids.
    #[ts(type = "unknown")]
    pub payload: Option<serde_json::Value>,
}

/// `GET /api/changes?since=` and the data of an SSE `changes` event.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ChangesDto {
    pub changes: Vec<ChangeDto>,
    /// Cursor to resume from.
    pub next: String,
    /// More changes are waiting beyond this page.
    pub more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Tls,
    Starttls,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Tls => "tls",
            TlsMode::Starttls => "starttls",
        }
    }
}

/// `POST /api/accounts` body.
#[derive(Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewAccountDto {
    pub name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: TlsMode,
    pub username: String,
    pub password: String,
    #[serde(default)]
    #[ts(optional)]
    pub max_connections: Option<u8>,
}

impl std::fmt::Debug for NewAccountDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccountDto")
            .field("name", &self.name)
            .field("imap_host", &self.imap_host)
            .field("smtp_host", &self.smtp_host)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// An account as returned by the API; never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub id: i64,
    pub name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: TlsMode,
    pub username: String,
    pub max_connections: u8,
}

/// Body of every error response and of the SSE `reset` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorDto {
    /// Stable machine-readable code, e.g. `gone`, `invalid`, `not_found`.
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub field: Option<String>,
}
