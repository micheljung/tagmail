//! JSON shapes of the HTTP API. TypeScript bindings are generated into
//! `packages/api-types` (`cargo xtask gen-ts-types`); ids and dates are
//! exported as `number`, since cache ids and Unix seconds stay below 2^53.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

/// `GET /api/version`. A client whose own API version is older than
/// `minClient` must update before it syncs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub api: ApiVersion,
    pub min_client: ApiVersion,
    pub server: String,
    /// Optional features this server was built with, e.g. `calendar`;
    /// absent from older servers.
    #[serde(default)]
    pub features: Vec<String>,
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

/// `GET /api/status`, one entry per account. `degraded` codes:
/// `no_condstore`, `no_qresync`, `no_idle`, `no_modseq:<mailbox>`,
/// `no_keywords:<mailbox>`, `labels_degraded` (the server drops custom
/// keywords) and `no_sender_address` (the account has no `email`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatusDto {
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
    pub status: String,
    pub phase: SyncPhase,
    pub mailboxes_total: u32,
    pub mailboxes_live: u32,
    /// Messages whose text is not fetched (and searchable) yet.
    pub text_pending: u32,
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
    /// Mailboxes whose list shows the thread (pending moves included),
    /// ascending.
    pub mailbox_ids: Vec<i64>,
}

/// `GET /api/threads?ids=`: rows as of `cursor`. Ids without a row (deleted
/// or merged since the index was fetched) are absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ThreadsDto {
    pub cursor: String,
    pub threads: Vec<ThreadRowDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EntityDto {
    /// `upsert` carries an `AccountDto`; `delete` (no payload) once the
    /// account's data is purged.
    Account,
    Mailbox,
    Thread,
    Message,
    Label,
    /// Payload `{status, error?}`; see `OutboxItemDto`.
    Outbox,
    /// `upsert` carries an `IdentityDto`, `delete` none.
    Identity,
    /// A write's status: `upsert` carries an `OpDto`, `delete` (discarded)
    /// carries `{ opId }`; `id` is 0.
    Op,
    /// A label rename or delete job (`LabelJobDto.id`); deleted when done.
    LabelJob,
    /// A cached calendar; only with `include=calendar`.
    Calendar,
    /// A cached calendar object resource; only with `include=calendar`.
    Event,
    /// A calendar write's status: `upsert` carries an `OpDto`; `id` is the
    /// op's seq. Only with `include=calendar`.
    CalOp,
    /// A due reminder; only with `include=calendar`.
    Reminder,
    /// An entity this client does not know; skipped.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
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

/// `account.status`. Only `active` accounts sync; `deleting` is set by
/// `DELETE /api/accounts/{id}` until the purge finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AccountStateDto {
    Active,
    Disabled,
    Deleting,
}

impl AccountStateDto {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStateDto::Active => "active",
            AccountStateDto::Disabled => "disabled",
            AccountStateDto::Deleting => "deleting",
        }
    }

    pub fn parse(s: &str) -> Option<AccountStateDto> {
        [
            AccountStateDto::Active,
            AccountStateDto::Disabled,
            AccountStateDto::Deleting,
        ]
        .into_iter()
        .find(|state| state.as_str() == s)
    }
}

/// `POST /api/accounts` and `POST /api/accounts/test` body. `email` is the
/// address to send from; without it the username must be one.
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
    #[serde(default)]
    #[ts(optional)]
    pub email: Option<String>,
    /// Display name of the default identity.
    #[serde(default)]
    #[ts(optional)]
    pub display_name: Option<String>,
}

impl std::fmt::Debug for NewAccountDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccountDto")
            .field("name", &self.name)
            .field("email", &self.email)
            .field("imap_host", &self.imap_host)
            .field("smtp_host", &self.smtp_host)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// An account as returned by the API; never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
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
    /// Primary sender address; `null` until the owner enters one.
    pub email: Option<String>,
    pub status: AccountStateDto,
}

/// `GET /api/accounts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccountsDto {
    pub accounts: Vec<AccountDto>,
}

/// `PATCH /api/accounts/{id}` and `POST /api/accounts/{id}/test` body:
/// the fields to change; a test merges them over the stored account.
/// `status` may only be `active` or `disabled`.
#[derive(Clone, Default, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields)]
pub struct AccountPatchDto {
    pub name: Option<String>,
    pub email: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_tls: Option<TlsMode>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_tls: Option<TlsMode>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub max_connections: Option<u8>,
    pub status: Option<AccountStateDto>,
}

impl std::fmt::Debug for AccountPatchDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPatchDto")
            .field("name", &self.name)
            .field("email", &self.email)
            .field("imap_host", &self.imap_host)
            .field("smtp_host", &self.smtp_host)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// One step of an account test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TestStepDto {
    pub ok: bool,
    /// Never contains the password.
    pub error: Option<String>,
    pub ms: u64,
}

/// `POST /api/accounts/test` and `POST /api/accounts/{id}/test`: an IMAP
/// login with CAPABILITY and LIST, and an SMTP login, without saving
/// anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccountTestDto {
    pub imap: TestStepDto,
    pub smtp: TestStepDto,
    pub capabilities: Vec<String>,
    /// Mailboxes the IMAP server listed; `null` if the login failed.
    pub mailboxes: Option<u32>,
}

/// Body of every error response and of the SSE `reset` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorDto {
    /// Stable machine-readable code, e.g. `gone`, `invalid`, `not_found`.
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SpecialUseDto {
    All,
    Archive,
    Drafts,
    Flagged,
    Important,
    Junk,
    Sent,
    Trash,
}

impl From<SpecialUseDto> for tagmail_core::SpecialUse {
    fn from(u: SpecialUseDto) -> tagmail_core::SpecialUse {
        use tagmail_core::SpecialUse;
        match u {
            SpecialUseDto::All => SpecialUse::All,
            SpecialUseDto::Archive => SpecialUse::Archive,
            SpecialUseDto::Drafts => SpecialUse::Drafts,
            SpecialUseDto::Flagged => SpecialUse::Flagged,
            SpecialUseDto::Important => SpecialUse::Important,
            SpecialUseDto::Junk => SpecialUse::Junk,
            SpecialUseDto::Sent => SpecialUse::Sent,
            SpecialUseDto::Trash => SpecialUse::Trash,
        }
    }
}

/// `GET /api/mailboxes`, one entry per mailbox of every account. The view
/// of a mailbox is `mailbox:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MailboxDto {
    pub id: i64,
    pub account_id: i64,
    /// Display name: decoded from modified UTF-7, hierarchy separated by
    /// `delimiter`.
    pub name: String,
    pub delimiter: Option<String>,
    pub special_use: Option<SpecialUseDto>,
}

/// `GET /api/labels`, one entry per label key across accounts. The view of
/// a label is `label:<key>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelDto {
    /// Casefolded identity.
    pub key: String,
    pub name: String,
    pub color: Option<String>,
    /// Accounts whose registry or cache has the key, ascending.
    pub account_ids: Vec<i64>,
}

/// One label of a [`LabelExportDto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelExportEntryDto {
    pub name: String,
    pub color: Option<String>,
    pub sort: i64,
}

/// `GET /api/labels/export` and the body of `POST /api/labels/import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelExportDto {
    /// Always `tagmail-labels`.
    pub format: String,
    /// Always 1.
    pub version: u32,
    pub labels: Vec<LabelExportEntryDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelImportErrorDto {
    pub name: String,
    pub message: String,
}

/// `POST /api/labels/import`: labels missing everywhere are created, the
/// colour of existing ones updated; nothing is renamed or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelImportResultDto {
    pub created: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub errors: Vec<LabelImportErrorDto>,
}

/// What a message body was written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    Html,
    /// Plain text, rendered as escaped HTML with links.
    Text,
    None,
}

/// A downloadable or inline part of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PartDto {
    /// Part number within the message.
    pub id: u32,
    pub content_type: String,
    pub name: Option<String>,
    /// Decoded size in bytes.
    pub size: u64,
    pub content_id: Option<String>,
    /// Shown inside the body rather than listed as an attachment.
    pub inline: bool,
    /// Signed `/api/parts/{token}` URL, valid for 10 minutes; append
    /// `?download=1` to force a download.
    pub url: String,
}

/// `GET /api/messages/{id}`. The body itself is served by
/// `GET /api/messages/{id}/render`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: i64,
    pub thread_id: i64,
    pub account_id: i64,
    pub subject: Option<String>,
    pub from: Vec<AddressDto>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub reply_to: Vec<AddressDto>,
    /// Unix seconds.
    pub date: i64,
    /// `Message-ID` without angle brackets.
    pub message_id: Option<String>,
    pub unread: bool,
    pub starred: bool,
    pub body: BodyKind,
    /// Remote images in the body; they load only through the image proxy
    /// when `imagesAllowed` or the render URL has `images=1`.
    pub remote_images: u32,
    /// A sender is on the "always show images" list.
    pub images_allowed: bool,
    pub parts: Vec<PartDto>,
    /// An iCalendar part was seen at sync; `GET /api/messages/{id}/invitation`
    /// reads it.
    pub has_invitation: bool,
}

/// One message of `GET /api/threads/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMessageDto {
    pub id: i64,
    pub from: Option<AddressDto>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    /// Unix seconds.
    pub date: i64,
    pub subject: Option<String>,
    pub snippet: String,
    pub unread: bool,
    pub starred: bool,
    pub has_attachment: bool,
    /// `null` until the raw message is cached (first open).
    pub has_remote_images: Option<bool>,
}

/// `GET /api/threads/{id}`: messages oldest first. A merged thread id
/// resolves to the thread it was merged into (`thread.id` differs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailDto {
    pub thread: ThreadRowDto,
    pub messages: Vec<ThreadMessageDto>,
}

/// `GET /api/image-senders` and the response of `POST`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ImageSendersDto {
    /// Lowercased addresses whose remote images always load.
    pub addresses: Vec<String>,
}

/// `POST /api/image-senders` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageSenderDto {
    pub address: String,
    pub allow: bool,
}
