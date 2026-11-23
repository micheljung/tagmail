//! JSON shapes of the write API for ops and labels (`/api/ops*`,
//! `/api/labels` writes, `/api/label-jobs`).

use serde::{Deserialize, Serialize};
use tagmail_core::ops::{OpKind, OpState};
use ts_rs::TS;

use crate::dto::LabelDto;

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
    /// To `mailboxId`.
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
}

impl From<OpKindDto> for OpKind {
    fn from(k: OpKindDto) -> OpKind {
        match k {
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
        }
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
    /// Destination mailbox id, for `move`.
    #[serde(default)]
    #[ts(optional)]
    pub mailbox_id: Option<i64>,
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

/// One op; also the payload of `op` change events (whose id is the op's
/// numeric `seq`). Done ops are not listed but stay readable by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
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
}

/// `GET /api/ops?status=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
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
