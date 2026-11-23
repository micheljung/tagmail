//! DTOs of compose and send: identities, drafts, attachments, the outbox
//! and the reply/forward helper.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::dto::AddressDto;

/// A From identity. Every account has at least one and exactly one
/// default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct IdentityDto {
    pub id: i64,
    pub account_id: i64,
    pub address: String,
    pub name: Option<String>,
    /// Sanitized HTML.
    pub signature: Option<String>,
    pub is_default: bool,
}

/// `GET /api/identities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct IdentitiesDto {
    pub identities: Vec<IdentityDto>,
}

/// `POST /api/identities` body. `signature` is HTML, sanitized on save
/// (at most 16 KiB; images only as `data:` URIs).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewIdentityDto {
    pub account_id: i64,
    pub address: String,
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub signature: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub is_default: Option<bool>,
}

/// `PATCH /api/identities/{id}` body: the fields to change. An empty
/// `name` or `signature` clears it; `isDefault: false` is refused (make
/// another identity the default instead).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(optional_fields)]
pub struct IdentityPatchDto {
    pub address: Option<String>,
    pub name: Option<String>,
    pub signature: Option<String>,
    pub is_default: Option<bool>,
}

/// An uploaded attachment: `POST /api/attachments` as
/// `multipart/form-data` with one field `file` (its file name and type
/// are taken from the part), at most 25 MB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub content_type: String,
}

/// `POST /api/drafts` and `PUT /api/drafts/{id}` body. `inReplyTo` and
/// `forwardOf` (message ids) only count on create: they fix the reply
/// references, and a forward without `attachmentIds` copies the
/// original's attachments into the draft (the reply helper's attachments
/// are already copies). A `PUT` replaces every other field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftInputDto {
    pub identity_id: i64,
    #[serde(default)]
    pub to: Vec<AddressDto>,
    #[serde(default)]
    pub cc: Vec<AddressDto>,
    #[serde(default)]
    pub bcc: Vec<AddressDto>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    #[ts(optional)]
    pub text: Option<String>,
    /// Sanitized again when the draft is saved or sent.
    #[serde(default)]
    #[ts(optional)]
    pub html: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub in_reply_to: Option<i64>,
    #[serde(default)]
    #[ts(optional)]
    pub forward_of: Option<i64>,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    /// Save to the IMAP Drafts mailbox now (compose closed) rather than
    /// after 30 s without edits.
    #[serde(default)]
    #[ts(optional)]
    pub save_now: Option<bool>,
    /// Client-generated id (create only): a create repeating it updates
    /// and returns that draft instead of making a second one.
    #[serde(default)]
    #[ts(optional)]
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DraftDto {
    pub id: i64,
    pub account_id: i64,
    pub identity_id: Option<i64>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    /// Message-ID header of the message this draft replies to.
    pub in_reply_to_message_id: Option<String>,
    pub attachment_ids: Vec<String>,
    pub attachments: Vec<AttachmentDto>,
    /// Bumped by every update.
    pub version: i64,
    /// The current version is in the IMAP Drafts mailbox.
    pub saved: bool,
    /// Why the last save to IMAP failed; it is retried.
    pub save_error: Option<String>,
    /// In the outbox; updates are refused until it is sent or cancelled.
    pub queued: bool,
}

/// `POST /api/send` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendDto {
    /// Client-generated; a repeated request with the same id is answered
    /// with the first one's current state and never sends twice.
    pub op_id: String,
    pub draft_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStateDto {
    Queued,
    /// Handed to the SMTP server; after a crash in this state the server
    /// checks Sent before sending again (at-least-once).
    Sending,
    /// Accepted by the SMTP server; the copy in Sent is still pending.
    Sent,
    /// Retry with `POST /api/outbox/{opId}/retry`.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SendStatusDto {
    pub op_id: String,
    pub status: OutboxStateDto,
}

/// An outbox item; sent items stay listed for an hour. Changes arrive as
/// change events of entity `outbox` with payload `{status, error?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OutboxItemDto {
    pub id: i64,
    pub op_id: String,
    pub account_id: i64,
    pub draft_id: Option<i64>,
    pub subject: String,
    /// Every envelope recipient (To, Cc and Bcc).
    pub to: Vec<AddressDto>,
    pub status: OutboxStateDto,
    pub attempts: u32,
    pub error: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    pub sent_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OutboxDto {
    pub items: Vec<OutboxItemDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    Reply,
    All,
    Forward,
}

/// `GET /api/messages/{id}/reply?mode=reply|all|forward`: the fields of a
/// new draft, ready for `POST /api/drafts`. A forward's attachments are
/// fresh uploads; pass their ids as `attachmentIds`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplyDraftDto {
    /// The identity of the original's account it was addressed to (To,
    /// Cc, Delivered-To, X-Original-To, or own From), else the account
    /// default.
    pub identity_id: Option<i64>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    /// The quoted original (or the forwarded message) as HTML.
    pub html: String,
    pub in_reply_to: Option<i64>,
    pub forward_of: Option<i64>,
    pub attachments: Vec<AttachmentDto>,
}
