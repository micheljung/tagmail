//! Write handlers for ops and labels over a [`WriteService`] (the server's
//! sync engine; the desktop's replica intents later).

use std::fmt;

use serde::de::DeserializeOwned;
use tagmail_core::ops::OpKind;
use tagmail_core::{MailboxRowId, ThreadId};

use crate::api::{Api, Response, bad_request, query_param};
use crate::dto::{ApiErrorDto, LabelDto};
use crate::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelJobsDto, LabelPatchDto, NewLabelDto, NewOpDto, OpDto,
    OpStatusDto, OpsDto,
};
use crate::store::{BoxFuture, Store, StoreError};

/// Client op ids: UUIDs or similar opaque tokens.
pub const MAX_OP_ID_LEN: usize = 64;
/// Targets per op.
pub const MAX_OP_TARGETS: usize = 10_000;

/// A validated `POST /api/ops`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpRequest {
    pub op_id: String,
    pub kind: OpKind,
    pub thread_ids: Vec<ThreadId>,
    pub email_ids: Vec<String>,
    pub label: Option<String>,
    pub mailbox: Option<MailboxRowId>,
}

/// Whether `submit_op` created the op or found it already submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    Created(OpDto),
    Existing(OpDto),
}

#[derive(Debug)]
pub enum WriteError {
    NotFound(String),
    Invalid {
        field: String,
        message: String,
    },
    /// The request conflicts with the current state (e.g. a label exists).
    Conflict(String),
    Internal(StoreError),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::NotFound(m) | WriteError::Conflict(m) => f.write_str(m),
            WriteError::Invalid { field, message } => write!(f, "{field}: {message}"),
            WriteError::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl WriteError {
    pub fn invalid(field: &str, message: impl Into<String>) -> WriteError {
        WriteError::Invalid {
            field: field.to_owned(),
            message: message.into(),
        }
    }

    pub fn internal(e: impl Into<StoreError>) -> WriteError {
        WriteError::Internal(e.into())
    }
}

pub type WriteResult<T> = Result<T, WriteError>;

/// Durable writes: ops are queued and applied optimistically, labels go to
/// the registry and re-flag jobs.
pub trait WriteService: Send + Sync + 'static {
    fn submit_op(&self, req: OpRequest) -> BoxFuture<WriteResult<Submitted>>;
    /// Open and failed ops, oldest first, optionally of one status.
    fn ops(&self, status: Option<OpStatusDto>) -> BoxFuture<WriteResult<Vec<OpDto>>>;
    fn op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>>;
    /// Puts a failed op back in the queue.
    fn retry_op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>>;
    /// Drops a pending or failed op and its overlay.
    fn discard_op(&self, op_id: String) -> BoxFuture<WriteResult<()>>;
    fn create_label(&self, label: NewLabelDto) -> BoxFuture<WriteResult<LabelDto>>;
    fn update_label(
        &self,
        key: String,
        patch: LabelPatchDto,
    ) -> BoxFuture<WriteResult<LabelChangeDto>>;
    fn delete_label(&self, key: String) -> BoxFuture<WriteResult<LabelChangeDto>>;
    fn label_jobs(&self) -> BoxFuture<WriteResult<Vec<LabelJobDto>>>;
}

fn write_error(e: WriteError) -> Response {
    match e {
        WriteError::NotFound(m) => Response::error(404, "not_found", m),
        WriteError::Invalid { field, message } => Response::json(
            422,
            &ApiErrorDto {
                error: "invalid".to_owned(),
                message,
                field: Some(field),
            },
        ),
        WriteError::Conflict(m) => Response::error(409, "conflict", m),
        WriteError::Internal(e) => {
            tracing::error!(error = %e, "write failed");
            Response::error(500, "internal", "internal error")
        }
    }
}

fn respond<T: serde::Serialize>(status: u16, r: WriteResult<T>) -> Response {
    match r {
        Ok(v) => Response::json(status, &v),
        Err(e) => write_error(e),
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| bad_request(format!("invalid JSON: {e}")))
}

fn valid_op_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_OP_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub(crate) fn op_request(dto: NewOpDto) -> Result<OpRequest, Response> {
    let invalid = |field: &str, m: &str| Err(write_error(WriteError::invalid(field, m)));
    if !valid_op_id(&dto.op_id) {
        return invalid(
            "opId",
            "1 to 64 characters of letters, digits, '-', '_' or '.'",
        );
    }
    let kind = OpKind::from(dto.kind);
    let thread_ids = dto.thread_ids.unwrap_or_default();
    let email_ids = dto.email_ids.unwrap_or_default();
    if thread_ids.is_empty() && email_ids.is_empty() {
        return invalid("threadIds", "name at least one thread or email id");
    }
    if thread_ids.len() + email_ids.len() > MAX_OP_TARGETS {
        return invalid("threadIds", "too many targets");
    }
    if thread_ids.iter().any(|t| *t < 0) {
        return invalid("threadIds", "thread ids are non-negative");
    }
    if email_ids
        .iter()
        .any(|e| tagmail_core::EmailId::parse(e).is_err())
    {
        return invalid("emailIds", "not an email id");
    }
    match (kind.needs_label(), &dto.label) {
        (true, None) => return invalid("label", "required for label ops"),
        (false, Some(_)) => return invalid("label", "only for label ops"),
        (true, Some(l)) if l.trim().is_empty() => return invalid("label", "empty label"),
        _ => {}
    }
    match (kind.needs_mailbox(), dto.mailbox_id) {
        (true, None) => return invalid("mailboxId", "required for move"),
        (false, Some(_)) => return invalid("mailboxId", "only for move"),
        _ => {}
    }
    Ok(OpRequest {
        op_id: dto.op_id,
        kind,
        thread_ids: thread_ids.into_iter().map(ThreadId).collect(),
        email_ids,
        label: dto.label,
        mailbox: dto.mailbox_id.map(MailboxRowId),
    })
}

/// `#rrggbb`, lowercased.
pub fn normalize_color(color: &str) -> Option<String> {
    let hex = color.strip_prefix('#')?;
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_ascii_lowercase()))
}

impl<S: Store> Api<S> {
    fn writes(&self) -> Result<&dyn WriteService, Response> {
        self.writes.as_deref().ok_or_else(|| {
            Response::error(
                503,
                "engine_unavailable",
                "no sync engine is running; changes cannot be saved",
            )
        })
    }

    pub(crate) async fn submit_op(&self, body: &[u8]) -> Result<Response, StoreError> {
        let w = match self.writes() {
            Ok(w) => w,
            Err(r) => return Ok(r),
        };
        let req = match parse::<NewOpDto>(body).and_then(op_request) {
            Ok(r) => r,
            Err(r) => return Ok(r),
        };
        Ok(match w.submit_op(req).await {
            Ok(Submitted::Created(op)) => Response::json(202, &op),
            Ok(Submitted::Existing(op)) => Response::json(200, &op),
            Err(e) => write_error(e),
        })
    }

    pub(crate) async fn list_ops(&self, query: &str) -> Result<Response, StoreError> {
        let w = match self.writes() {
            Ok(w) => w,
            Err(r) => return Ok(r),
        };
        let status = match query_param(query, "status").as_deref() {
            None => None,
            Some("pending") => Some(OpStatusDto::Pending),
            Some("failed") => Some(OpStatusDto::Failed),
            Some(_) => return Ok(bad_request("status must be pending or failed")),
        };
        Ok(respond(200, w.ops(status).await.map(|ops| OpsDto { ops })))
    }

    pub(crate) async fn get_op(&self, id: &str) -> Result<Response, StoreError> {
        Ok(match self.writes() {
            Ok(w) => respond(200, w.op(id.to_owned()).await),
            Err(r) => r,
        })
    }

    pub(crate) async fn retry_op(&self, id: &str) -> Result<Response, StoreError> {
        Ok(match self.writes() {
            Ok(w) => respond(202, w.retry_op(id.to_owned()).await),
            Err(r) => r,
        })
    }

    pub(crate) async fn discard_op(&self, id: &str) -> Result<Response, StoreError> {
        Ok(match self.writes() {
            Ok(w) => match w.discard_op(id.to_owned()).await {
                Ok(()) => Response::new(204, crate::api::JSON, Vec::new()),
                Err(e) => write_error(e),
            },
            Err(r) => r,
        })
    }

    pub(crate) async fn create_label(&self, body: &[u8]) -> Result<Response, StoreError> {
        let w = match self.writes() {
            Ok(w) => w,
            Err(r) => return Ok(r),
        };
        Ok(match parse::<NewLabelDto>(body) {
            Ok(dto) => respond(201, w.create_label(dto).await),
            Err(r) => r,
        })
    }

    pub(crate) async fn update_label(
        &self,
        key: &str,
        body: &[u8],
    ) -> Result<Response, StoreError> {
        let w = match self.writes() {
            Ok(w) => w,
            Err(r) => return Ok(r),
        };
        Ok(match parse::<LabelPatchDto>(body) {
            Ok(dto) => respond(200, w.update_label(key.to_owned(), dto).await),
            Err(r) => r,
        })
    }

    pub(crate) async fn delete_label(&self, key: &str) -> Result<Response, StoreError> {
        Ok(match self.writes() {
            Ok(w) => respond(202, w.delete_label(key.to_owned()).await),
            Err(r) => r,
        })
    }

    pub(crate) async fn label_jobs(&self) -> Result<Response, StoreError> {
        Ok(match self.writes() {
            Ok(w) => respond(200, w.label_jobs().await.map(|jobs| LabelJobsDto { jobs })),
            Err(r) => r,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops_dto::OpKindDto;

    fn dto(kind: OpKindDto) -> NewOpDto {
        NewOpDto {
            op_id: "4b1c2d3e-0000-4000-8000-000000000001".into(),
            kind,
            thread_ids: Some(vec![3]),
            email_ids: None,
            label: None,
            mailbox_id: None,
        }
    }

    fn field(r: Response) -> String {
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(r.status, 422);
        v["field"].as_str().unwrap().to_owned()
    }

    #[test]
    fn op_requests_are_validated() {
        let ok = op_request(dto(OpKindDto::Archive)).unwrap();
        assert_eq!(ok.kind, OpKind::Archive);
        assert_eq!(ok.thread_ids, vec![ThreadId(3)]);

        let mut bad = dto(OpKindDto::Archive);
        bad.op_id = "a b".into();
        assert_eq!(field(op_request(bad).unwrap_err()), "opId");
        let mut none = dto(OpKindDto::Star);
        none.thread_ids = None;
        assert_eq!(field(op_request(none).unwrap_err()), "threadIds");
        assert_eq!(
            field(op_request(dto(OpKindDto::LabelAdd)).unwrap_err()),
            "label"
        );
        let mut extra = dto(OpKindDto::Star);
        extra.label = Some("x".into());
        assert_eq!(field(op_request(extra).unwrap_err()), "label");
        assert_eq!(
            field(op_request(dto(OpKindDto::Move)).unwrap_err()),
            "mailboxId"
        );
        let mut email = dto(OpKindDto::Star);
        email.email_ids = Some(vec!["nope".into()]);
        assert_eq!(field(op_request(email).unwrap_err()), "emailIds");
    }

    #[test]
    fn kind_names_match_the_contract() {
        let parsed: NewOpDto = serde_json::from_str(
            r#"{"opId":"x","kind":"label.add","threadIds":[1],"label":"work"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, OpKindDto::LabelAdd);
        for k in OpKind::ALL {
            let json = serde_json::to_string(&OpKindDto::from(k)).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(OpKind::from(OpKindDto::from(k)), k);
        }
    }

    #[test]
    fn colors_are_normalized() {
        assert_eq!(normalize_color("#A0b1C2").as_deref(), Some("#a0b1c2"));
        for bad in ["a0b1c2", "#abc", "#gggggg", ""] {
            assert_eq!(normalize_color(bad), None, "{bad}");
        }
    }
}
