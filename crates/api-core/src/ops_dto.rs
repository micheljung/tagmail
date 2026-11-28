//! JSON shapes of the write API for ops and labels (`/api/ops*`,
//! `/api/labels` writes, `/api/label-jobs`).

use serde::{Deserialize, Serialize};
use tagmail_core::ops::{OpKind, OpState};
use ts_rs::TS;

use crate::dto::{LabelDto, SpecialUseDto};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum OpKindDto {
    #[serde(rename = "label.add")]
    LabelAdd,
    #[serde(rename = "label.remove")]
    LabelRemove,
    #[serde(rename = "archive")]
    Archive,
    /// Back to INBOX.
    #[serde(rename = "unarchive")]
    Unarchive,
    /// To `mailboxId` or `mailbox`.
    #[serde(rename = "move")]
    Move,
    #[serde(rename = "markRead")]
    MarkRead,
    #[serde(rename = "markUnread")]
    MarkUnread,
    #[serde(rename = "star")]
    Star,
    #[serde(rename = "unstar")]
    Unstar,
    #[serde(rename = "trash")]
    Trash,
    #[serde(rename = "spam")]
    Spam,
    /// Permanently, from Trash or Junk.
    #[serde(rename = "delete")]
    Delete,
    /// Calendar ops (`domain: "cal"`); never accepted by `POST /api/ops`.
    #[serde(rename = "cal.rsvp")]
    CalRsvp,
    #[serde(rename = "cal.import")]
    CalImport,
    #[serde(rename = "cal.apply")]
    CalApply,
    #[serde(rename = "cal.put")]
    CalPut,
    #[serde(rename = "cal.delete")]
    CalDelete,
    #[serde(rename = "cal.move")]
    CalMove,
}

/// A calendar kind, which is no mail op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAMailOp;

impl TryFrom<OpKindDto> for OpKind {
    type Error = NotAMailOp;

    fn try_from(k: OpKindDto) -> Result<OpKind, NotAMailOp> {
        Ok(match k {
            OpKindDto::LabelAdd => OpKind::LabelAdd,
            OpKindDto::LabelRemove => OpKind::LabelRemove,
            OpKindDto::Archive => OpKind::Archive,
            OpKindDto::Unarchive => OpKind::Unarchive,
            OpKindDto::Move => OpKind::Move,
            OpKindDto::MarkRead => OpKind::MarkRead,
            OpKindDto::MarkUnread => OpKind::MarkUnread,
            OpKindDto::Star => OpKind::Star,
            OpKindDto::Unstar => OpKind::Unstar,
            OpKindDto::Trash => OpKind::Trash,
            OpKindDto::Spam => OpKind::Spam,
            OpKindDto::Delete => OpKind::Delete,
            OpKindDto::CalRsvp
            | OpKindDto::CalImport
            | OpKindDto::CalApply
            | OpKindDto::CalPut
            | OpKindDto::CalDelete
            | OpKindDto::CalMove => return Err(NotAMailOp),
        })
    }
}

impl From<OpKind> for OpKindDto {
    fn from(k: OpKind) -> OpKindDto {
        match k {
            OpKind::LabelAdd => OpKindDto::LabelAdd,
            OpKind::LabelRemove => OpKindDto::LabelRemove,
            OpKind::Archive => OpKindDto::Archive,
            OpKind::Unarchive => OpKindDto::Unarchive,
            OpKind::Move => OpKindDto::Move,
            OpKind::MarkRead => OpKindDto::MarkRead,
            OpKind::MarkUnread => OpKindDto::MarkUnread,
            OpKind::Star => OpKindDto::Star,
            OpKind::Unstar => OpKindDto::Unstar,
            OpKind::Trash => OpKindDto::Trash,
            OpKind::Spam => OpKindDto::Spam,
            OpKind::Delete => OpKindDto::Delete,
        }
    }
}

/// `POST /api/ops`. Thread ids expand to every message of the threads at
/// action time; the targets must belong to one account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewOpDto {
    /// Client-generated UUID; re-posting it returns the op's status.
    pub op_id: String,
    pub kind: OpKindDto,
    #[serde(default)]
    #[ts(optional)]
    pub thread_ids: Option<Vec<i64>>,
    #[serde(default)]
    #[ts(optional)]
    pub email_ids: Option<Vec<String>>,
    /// Label key, for `label.add` / `label.remove`.
    #[serde(default)]
    #[ts(optional)]
    pub label: Option<String>,
    /// Destination mailbox id, for `move`; exclusive with `mailbox`.
    #[serde(default)]
    #[ts(optional)]
    pub mailbox_id: Option<i64>,
    /// Destination of `move` by name, for clients whose mailbox ids may be
    /// stale (the desktop replica); exclusive with `mailboxId`.
    #[serde(default)]
    #[ts(optional)]
    pub mailbox: Option<MailboxRefDto>,
}

/// A mailbox by what survives a cache rebuild, resolved by special-use,
/// then name, then a unique `uidvalidity`. An op whose mailbox does not
/// resolve fails with "mailbox not found".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxRefDto {
    /// IMAP name as stored by the server (modified UTF-7).
    pub name: String,
    #[serde(default)]
    #[ts(optional)]
    pub special_use: Option<SpecialUseDto>,
    #[serde(default)]
    #[ts(optional)]
    pub uidvalidity: Option<u32>,
}

/// `running` ops report `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum OpStatusDto {
    Pending,
    Done,
    Failed,
}

impl From<OpState> for OpStatusDto {
    fn from(s: OpState) -> OpStatusDto {
        match s {
            OpState::Pending | OpState::Running => OpStatusDto::Pending,
            OpState::Done => OpStatusDto::Done,
            OpState::Failed => OpStatusDto::Failed,
        }
    }
}

/// Which queue an op is in: mail ops act on IMAP, calendar ops on CalDAV.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum OpDomainDto {
    #[default]
    Mail,
    Cal,
}

/// Why a calendar op failed without retrying by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CalConflictDto {
    /// The server copy changed; it was kept and the rejected edit is held
    /// for re-apply (retry) or discard.
    ServerWon,
}

/// The iTIP REPLY an RSVP queued in the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum CalReplyStateDto {
    Queued,
    Sending,
    Sent,
    Failed,
    /// Cancelled in the outbox.
    Cancelled,
}

/// What a calendar op (`domain: "cal"`) writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CalOpInfoDto {
    /// The account whose calendar is written; `accountId` is the account
    /// the invitation arrived in.
    pub target_account_id: i64,
    pub calendar_href: Option<String>,
    pub uid: String,
    pub conflict: Option<CalConflictDto>,
    /// The outbox op id of the REPLY, `<opId>#reply`.
    pub reply_op_id: Option<String>,
    /// `null` while no REPLY is queued. A failed op with `reply: "sent"`
    /// is "reply sent, not saved to calendar".
    pub reply: Option<CalReplyStateDto>,
}

/// One op; also the payload of `op` change events (whose id is the op's
/// numeric `seq`) and, for calendar ops, of `cal_op` change events. Done
/// ops are not listed but stay readable by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OpDto {
    pub op_id: String,
    /// Stable numeric id of the op, the `id` of its change events.
    pub seq: i64,
    pub account_id: i64,
    pub kind: OpKindDto,
    pub status: OpStatusDto,
    /// Why it failed, or why the last attempt did not go through.
    pub error: Option<String>,
    pub attempts: u32,
    /// Current threads of the op's messages; empty once it is done.
    pub thread_ids: Vec<i64>,
    /// Label key of label ops.
    pub label: Option<String>,
    /// Destination of `move`, while the mailbox is known.
    pub mailbox_id: Option<i64>,
    /// Unix seconds.
    pub created_at: i64,
    #[serde(default)]
    pub domain: OpDomainDto,
    /// Set for calendar ops only.
    #[serde(default)]
    pub calendar: Option<CalOpInfoDto>,
}

/// `GET /api/ops?status=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OpsDto {
    pub ops: Vec<OpDto>,
}

/// `POST /api/labels`: creates the label in every account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewLabelDto {
    pub name: String,
    /// `#rrggbb`.
    #[serde(default)]
    #[ts(optional)]
    pub color: Option<String>,
}

/// `PATCH /api/labels/{key}`. A name with another key is a rename, run as
/// a resumable job; an empty `color` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelPatchDto {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub color: Option<String>,
}

/// Answer of `PATCH` and `DELETE /api/labels/{key}`: the label afterwards
/// (`None` when deleted; a rename changes the key) and the jobs re-flagging
/// messages, one per account carrying the label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelChangeDto {
    pub label: Option<LabelDto>,
    /// The first of `jobIds`.
    pub job_id: Option<i64>,
    pub job_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum LabelJobKindDto {
    Rename,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum LabelJobStatusDto {
    Running,
    /// The last step failed; the job retries.
    Failed,
    Done,
}

/// A label rename or delete; it survives restarts. Listed until done; the
/// last `label_job` change event says `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelJobDto {
    pub id: i64,
    pub account_id: i64,
    pub kind: LabelJobKindDto,
    /// Key of the label being renamed or deleted.
    pub label: String,
    /// New key of a rename.
    pub to: Option<String>,
    /// Mailboxes processed so far.
    pub done: u32,
    pub total: u32,
    pub status: LabelJobStatusDto,
    pub error: Option<String>,
}

/// `GET /api/label-jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LabelJobsDto {
    pub jobs: Vec<LabelJobDto>,
}
