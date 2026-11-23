//! Compose and send (plan §3.5): identities, drafts, attachments, the
//! outbox and the reply/forward helper. The handlers validate requests and
//! resolve cached messages into IMAP-stable references; a [`Compose`]
//! implementation (the server's engine) stores drafts, saves them to the
//! IMAP Drafts mailbox and sends through SMTP.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use tagmail_core::MessageRowId;

use crate::api::{Api, Response, bad_request, query_param};
use crate::compose_dto::{
    AttachmentDto, DraftDto, DraftInputDto, IdentitiesDto, IdentityDto, OutboxDto, OutboxItemDto,
    ReplyDraftDto, ReplyMode, SendDto, SendStatusDto,
};
use crate::dto::{AddressDto, ApiErrorDto};
use crate::mime;
use crate::store::{BoxFuture, MessageRecord, Store, StoreError};

/// Largest single upload (plan §3.8).
pub const MAX_UPLOAD: u64 = 25 * 1024 * 1024;
/// Largest total of a draft's attachments.
pub const MAX_ATTACHMENTS_TOTAL: u64 = 35 * 1024 * 1024;
/// Request body cap of draft create and update (bodies quote whole
/// threads, so above the general JSON cap).
pub const MAX_DRAFT_BODY: usize = 1024 * 1024;
pub const MAX_RECIPIENTS: usize = 500;
pub const MAX_ATTACHMENTS: usize = 100;
const MAX_ADDRESS_LEN: usize = 320;
const MAX_NAME_LEN: usize = 200;
const MAX_SUBJECT_LEN: usize = 998;
const MAX_FILE_NAME_LEN: usize = 255;
const MAX_OP_ID_LEN: usize = 100;

#[derive(Debug)]
pub enum ComposeError {
    NotFound(&'static str),
    Invalid {
        field: &'static str,
        message: String,
    },
    /// The draft or outbox item is in a state that forbids the change.
    Conflict(String),
    TooLarge(String),
    Unavailable(String),
    Internal(StoreError),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ComposeError::NotFound(what) => write!(f, "no such {what}"),
            ComposeError::Invalid { field, message } => write!(f, "{field}: {message}"),
            ComposeError::Conflict(m)
            | ComposeError::TooLarge(m)
            | ComposeError::Unavailable(m) => f.write_str(m),
            ComposeError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl<E: Into<StoreError>> From<E> for ComposeError {
    fn from(e: E) -> ComposeError {
        ComposeError::Internal(e.into())
    }
}

pub type ComposeResult<T> = Result<T, ComposeError>;

/// The message a new draft replies to, as IMAP-stable references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRefs {
    /// Message-ID without angle brackets.
    pub in_reply_to: String,
    /// The original's References followed by its Message-ID.
    pub references: Vec<String>,
}

/// An attachment of a forwarded message, copied into the new draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedPart {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A validated draft create or update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftInput {
    pub identity_id: i64,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachment_ids: Vec<i64>,
    /// Create only.
    pub reply: Option<ReplyRefs>,
    /// Message-ID of the forwarded message; create only.
    pub forward_of: Option<String>,
    /// Save to IMAP Drafts now instead of after the idle debounce.
    pub save_now: bool,
    /// Create only.
    pub forwarded: Vec<ForwardedPart>,
}

/// A file the server streamed into [`Compose::upload_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUpload {
    pub name: String,
    pub content_type: String,
    pub file: PathBuf,
    pub size: u64,
}

/// Whether `POST /api/send` queued the message now or had seen its op id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub status: SendStatusDto,
    pub created: bool,
}

/// Drafts, attachments, the outbox and identities; the server implements
/// it with the engine.
pub trait Compose: Send + Sync + 'static {
    fn identities(&self) -> BoxFuture<ComposeResult<Vec<IdentityDto>>>;
    fn create_draft(&self, input: DraftInput) -> BoxFuture<ComposeResult<DraftDto>>;
    fn update_draft(&self, id: i64, input: DraftInput) -> BoxFuture<ComposeResult<DraftDto>>;
    fn draft(&self, id: i64) -> BoxFuture<ComposeResult<DraftDto>>;
    fn delete_draft(&self, id: i64) -> BoxFuture<ComposeResult<()>>;
    /// Directory for uploads in progress; [`Compose::add_upload`] takes
    /// over files written there.
    fn upload_dir(&self) -> PathBuf;
    fn add_upload(&self, upload: NewUpload) -> BoxFuture<ComposeResult<AttachmentDto>>;
    fn delete_upload(&self, id: i64) -> BoxFuture<ComposeResult<()>>;
    /// Queues the draft for sending under the client's `op_id`.
    fn send(&self, op_id: String, draft_id: i64) -> BoxFuture<ComposeResult<SendOutcome>>;
    /// Queued, sending, failed and sent-but-not-yet-copied items.
    fn outbox(&self) -> BoxFuture<ComposeResult<Vec<OutboxItemDto>>>;
    /// Removes a queued or failed item; its draft becomes editable again.
    fn cancel(&self, id: i64) -> BoxFuture<ComposeResult<()>>;
    /// Queues a failed item again.
    fn retry(&self, id: i64) -> BoxFuture<ComposeResult<()>>;
    /// Stores parts as unattached uploads (a forward's attachments).
    fn add_parts(&self, parts: Vec<ForwardedPart>) -> BoxFuture<ComposeResult<Vec<AttachmentDto>>>;
}

fn respond<T: serde::Serialize>(status: u16, result: ComposeResult<T>) -> Response {
    match result {
        Ok(value) => Response::json(status, &value),
        Err(e) => error_response(e),
    }
}

pub fn error_response(e: ComposeError) -> Response {
    match e {
        ComposeError::NotFound(what) => {
            Response::error(404, "not_found", format!("no such {what}"))
        }
        ComposeError::Invalid { field, message } => Response::json(
            422,
            &ApiErrorDto {
                error: "invalid".to_owned(),
                message,
                field: Some(field.to_owned()),
            },
        ),
        ComposeError::Conflict(m) => Response::error(409, "conflict", m),
        ComposeError::TooLarge(m) => Response::error(413, "too_large", m),
        ComposeError::Unavailable(m) => Response::error(503, "unavailable", m),
        ComposeError::Internal(e) => {
            tracing::error!(error = %e, "compose failure");
            Response::error(500, "internal", "internal error")
        }
    }
}

fn invalid(field: &'static str, message: impl Into<String>) -> ComposeError {
    ComposeError::Invalid {
        field,
        message: message.into(),
    }
}

fn printable(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

/// `local@domain` without spaces, brackets or control characters.
pub fn valid_address(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && email.len() <= MAX_ADDRESS_LEN
        && !email
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "<>,;\"".contains(c))
}

fn addresses(field: &'static str, list: Vec<AddressDto>) -> ComposeResult<Vec<AddressDto>> {
    list.into_iter()
        .map(|a| {
            let email = a.email.trim().to_owned();
            if !valid_address(&email) {
                return Err(invalid(field, format!("{email:?} is not an email address")));
            }
            let name = a
                .name
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty());
            if name
                .as_deref()
                .is_some_and(|n| n.chars().count() > MAX_NAME_LEN || !printable(n))
            {
                return Err(invalid(field, "names are 1 to 200 printable characters"));
            }
            Ok(AddressDto { name, email })
        })
        .collect()
}

/// Checks a draft body; reply and forward references are added by the
/// caller.
pub fn validate_draft(dto: DraftInputDto) -> ComposeResult<DraftInput> {
    let to = addresses("to", dto.to)?;
    let cc = addresses("cc", dto.cc)?;
    let bcc = addresses("bcc", dto.bcc)?;
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(invalid(
            "to",
            format!("at most {MAX_RECIPIENTS} recipients"),
        ));
    }
    let subject = dto.subject.trim().to_owned();
    if subject.chars().count() > MAX_SUBJECT_LEN || !printable(&subject) {
        return Err(invalid(
            "subject",
            format!("at most {MAX_SUBJECT_LEN} printable characters"),
        ));
    }
    if dto.attachment_ids.len() > MAX_ATTACHMENTS {
        return Err(invalid(
            "attachmentIds",
            format!("at most {MAX_ATTACHMENTS} attachments"),
        ));
    }
    let mut attachment_ids = dto
        .attachment_ids
        .iter()
        .map(|id| parse_row_id(id))
        .collect::<Option<Vec<i64>>>()
        .ok_or_else(|| invalid("attachmentIds", "attachment ids are numbers"))?;
    attachment_ids.sort_unstable();
    attachment_ids.dedup();
    Ok(DraftInput {
        identity_id: dto.identity_id,
        to,
        cc,
        bcc,
        subject,
        text: dto.text,
        html: dto.html,
        attachment_ids,
        reply: None,
        forward_of: None,
        save_now: dto.save_now.unwrap_or(false),
        forwarded: Vec::new(),
    })
}

/// A file name safe for `Content-Disposition`: no path, no control
/// characters, at most 255 characters.
pub fn clean_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FILE_NAME_LEN)
        .collect();
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then_some(cleaned)
}

/// `type/subtype` in lowercase, else `application/octet-stream`.
pub fn clean_content_type(value: Option<&str>) -> String {
    let essence = value
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    match essence.split_once('/') {
        Some((t, s)) if token(t) && token(s) => essence,
        _ => "application/octet-stream".to_owned(),
    }
}

/// The identity a reply is sent from: the one whose address the original
/// was sent to (To or Cc), else the account's default.
pub fn choose_identity<'a>(
    identities: &'a [IdentityDto],
    account_id: i64,
    recipients: &[AddressDto],
) -> Option<&'a IdentityDto> {
    let own: Vec<&IdentityDto> = identities
        .iter()
        .filter(|i| i.account_id == account_id)
        .collect();
    own.iter()
        .find(|i| {
            recipients
                .iter()
                .any(|r| r.email.eq_ignore_ascii_case(&i.address))
        })
        .or_else(|| own.iter().find(|i| i.is_default))
        .or_else(|| own.first())
        .copied()
}

fn has_prefix(subject: &str, prefixes: &[&str]) -> bool {
    let lower = subject.trim_start().to_lowercase();
    prefixes.iter().any(|p| {
        lower
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(':'))
    })
}

fn reply_subject(subject: &str, mode: ReplyMode) -> String {
    let subject = subject.trim();
    match mode {
        ReplyMode::Forward if has_prefix(subject, &["fwd", "fw"]) => subject.to_owned(),
        ReplyMode::Forward => format!("Fwd: {subject}"),
        _ if has_prefix(subject, &["re"]) => subject.to_owned(),
        _ => format!("Re: {subject}"),
    }
}

fn display(a: &AddressDto) -> String {
    match &a.name {
        Some(name) => format!("{name} <{}>", a.email),
        None => a.email.clone(),
    }
}

fn display_list(list: &[AddressDto]) -> String {
    list.iter().map(display).collect::<Vec<_>>().join(", ")
}

fn push_unique(out: &mut Vec<AddressDto>, a: &AddressDto, skip: &[&str]) {
    let taken = out.iter().any(|o| o.email.eq_ignore_ascii_case(&a.email))
        || skip.iter().any(|s| s.eq_ignore_ascii_case(&a.email));
    if !taken {
        out.push(a.clone());
    }
}

/// What the reply helper reads from the original besides its record.
pub struct Original {
    pub record: MessageRecord,
    pub reply_to: Vec<AddressDto>,
    pub text: String,
}

/// The prefilled fields of a reply, reply-all or forward of `original`.
pub fn reply_fields(
    original: &Original,
    mode: ReplyMode,
    identities: &[IdentityDto],
) -> ReplyDraftDto {
    let r = &original.record;
    let recipients: Vec<AddressDto> = r.to.iter().chain(&r.cc).cloned().collect();
    let identity = choose_identity(identities, r.account_id.0, &recipients);
    let own: Vec<&str> = identities
        .iter()
        .filter(|i| i.account_id == r.account_id.0)
        .map(|i| i.address.as_str())
        .collect();
    let from_me = r
        .from
        .iter()
        .any(|f| own.iter().any(|o| o.eq_ignore_ascii_case(&f.email)));
    let date = mail_parser::DateTime::from_timestamp(r.date).to_rfc822();
    let sender = display_list(&r.from);
    let subject = reply_subject(r.subject.as_deref().unwrap_or_default(), mode);
    let (mut to, mut cc) = (Vec::new(), Vec::new());
    if mode == ReplyMode::Forward {
        let header = format!(
            "---------- Forwarded message ---------\nFrom: {sender}\nDate: {date}\n\
             Subject: {}\nTo: {}{}",
            r.subject.as_deref().unwrap_or_default(),
            display_list(&r.to),
            if r.cc.is_empty() {
                String::new()
            } else {
                format!("\nCc: {}", display_list(&r.cc))
            },
        );
        let html = format!(
            "<p><br></p>{}<br>{}",
            mime::text_to_html(&header),
            mime::text_to_html(&original.text)
        );
        return ReplyDraftDto {
            identity_id: identity.map(|i| i.id),
            to,
            cc,
            bcc: Vec::new(),
            subject,
            html,
            in_reply_to: None,
            forward_of: Some(r.id.0),
            attachments: Vec::new(),
        };
    }
    let primary: &[AddressDto] = if from_me {
        &r.to
    } else if !original.reply_to.is_empty() {
        &original.reply_to
    } else {
        &r.from
    };
    for a in primary {
        push_unique(&mut to, a, &[]);
    }
    if mode == ReplyMode::All {
        for a in r.to.iter().chain(&r.cc) {
            if !to.iter().any(|t| t.email.eq_ignore_ascii_case(&a.email)) {
                push_unique(&mut cc, a, &own);
            }
        }
    }
    let html = format!(
        "<p><br></p>{}<blockquote>{}</blockquote>",
        mime::text_to_html(&format!("On {date}, {sender} wrote:")),
        mime::text_to_html(&original.text)
    );
    ReplyDraftDto {
        identity_id: identity.map(|i| i.id),
        to,
        cc,
        bcc: Vec::new(),
        subject,
        html,
        in_reply_to: Some(r.id.0),
        forward_of: None,
        attachments: Vec::new(),
    }
}

/// Reply references and forwarded attachments of a raw message.
fn references_of(raw: &[u8]) -> Option<ReplyRefs> {
    let message = mime::parse(raw)?;
    let id = message.message_id()?.to_owned();
    let mut references: Vec<String> = message
        .references()
        .map(|l| l.iter().map(str::to_owned).collect())
        .unwrap_or_default();
    if references.is_empty()
        && let Some(parent) = message.in_reply_to().and_then(|l| l.last())
    {
        references.push(parent.to_owned());
    }
    references.push(id.clone());
    Some(ReplyRefs {
        in_reply_to: id,
        references,
    })
}

fn forwarded_parts(raw: &[u8]) -> Vec<ForwardedPart> {
    let Some(message) = mime::parse(raw) else {
        return Vec::new();
    };
    message
        .attachments()
        .filter_map(|part| {
            let name = clean_file_name(part.attachment_name()?)?;
            Some(ForwardedPart {
                name,
                content_type: mime::content_type(&part),
                bytes: part.decoded().into_owned(),
            })
        })
        .collect()
}

fn json<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| bad_request(format!("invalid JSON: {e}")))
}

fn parse_row_id(s: &str) -> Option<i64> {
    s.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| s.parse().ok())
        .flatten()
}

fn valid_op_id(op_id: &str) -> bool {
    !op_id.is_empty()
        && op_id.len() <= MAX_OP_ID_LEN
        && op_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

type Handled = Result<Response, StoreError>;

impl<S: Store> Api<S> {
    /// Serves the compose routes through `compose`; without it they answer
    /// `503`.
    pub fn with_compose(mut self, compose: Arc<dyn Compose>) -> Api<S> {
        self.compose = Some(compose);
        self
    }

    pub fn compose(&self) -> Option<&Arc<dyn Compose>> {
        self.compose.as_ref()
    }

    fn composer(&self) -> Result<&Arc<dyn Compose>, Response> {
        self.compose.as_ref().ok_or_else(|| {
            Response::error(503, "compose_unavailable", "sending is not available here")
        })
    }

    pub(crate) async fn identities(&self) -> Handled {
        Ok(match self.composer() {
            Ok(c) => respond(
                200,
                c.identities()
                    .await
                    .map(|identities| IdentitiesDto { identities }),
            ),
            Err(resp) => resp,
        })
    }

    /// The original's references and, for a forward, its attachments.
    async fn resolve_original(
        &self,
        input: &mut DraftInput,
        in_reply_to: Option<i64>,
        forward_of: Option<i64>,
    ) -> Result<Result<(), Response>, StoreError> {
        for (id, forward) in [(in_reply_to, false), (forward_of, true)] {
            let Some(id) = id else { continue };
            let Some(record) = self.store.message(MessageRowId(id)).await? else {
                return Ok(Err(error_response(invalid(
                    if forward { "forwardOf" } else { "inReplyTo" },
                    "no such message",
                ))));
            };
            let raw = match self.raw(&record).await? {
                Ok(raw) => raw,
                Err(resp) => return Ok(Err(resp)),
            };
            if forward {
                let copy = input.attachment_ids.is_empty();
                let (id, parts) = crate::messages::blocking(move || {
                    let id = mime::parse(&raw).and_then(|m| m.message_id().map(str::to_owned));
                    (
                        id,
                        if copy {
                            forwarded_parts(&raw)
                        } else {
                            Vec::new()
                        },
                    )
                })
                .await?;
                input.forward_of = id;
                input.forwarded = parts;
            } else {
                input.reply = crate::messages::blocking(move || references_of(&raw)).await?;
            }
        }
        Ok(Ok(()))
    }

    pub(crate) async fn create_draft(&self, body: &[u8]) -> Handled {
        let dto: DraftInputDto = match json(body) {
            Ok(dto) => dto,
            Err(resp) => return Ok(resp),
        };
        let (in_reply_to, forward_of) = (dto.in_reply_to, dto.forward_of);
        let mut input = match validate_draft(dto) {
            Ok(input) => input,
            Err(e) => return Ok(error_response(e)),
        };
        if let Err(resp) = self.composer() {
            return Ok(resp);
        }
        if let Err(resp) = self
            .resolve_original(&mut input, in_reply_to, forward_of)
            .await?
        {
            return Ok(resp);
        }
        Ok(match self.composer() {
            Ok(c) => respond(201, c.create_draft(input).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn update_draft(&self, id: &str, body: &[u8]) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("draft")));
        };
        let dto: DraftInputDto = match json(body) {
            Ok(dto) => dto,
            Err(resp) => return Ok(resp),
        };
        let input = match validate_draft(dto) {
            Ok(input) => input,
            Err(e) => return Ok(error_response(e)),
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.update_draft(id, input).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn get_draft(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("draft")));
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.draft(id).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn delete_draft(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("draft")));
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.delete_draft(id).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn delete_attachment(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("attachment")));
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.delete_upload(id).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn send(&self, body: &[u8]) -> Handled {
        let dto: SendDto = match json(body) {
            Ok(dto) => dto,
            Err(resp) => return Ok(resp),
        };
        if !valid_op_id(&dto.op_id) {
            return Ok(error_response(invalid(
                "opId",
                "1 to 100 characters of A-Z, a-z, 0-9, - and _",
            )));
        }
        let compose = match self.composer() {
            Ok(c) => c,
            Err(resp) => return Ok(resp),
        };
        Ok(match compose.send(dto.op_id, dto.draft_id).await {
            Ok(out) => Response::json(if out.created { 202 } else { 200 }, &out.status),
            Err(e) => error_response(e),
        })
    }

    pub(crate) async fn outbox(&self) -> Handled {
        Ok(match self.composer() {
            Ok(c) => respond(200, c.outbox().await.map(|items| OutboxDto { items })),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn cancel_outbox(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("outbox item")));
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.cancel(id).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn retry_outbox(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("outbox item")));
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.retry(id).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn reply(&self, id: &str, query: &str) -> Handled {
        let mode = match query_param(query, "mode").as_deref() {
            None | Some("reply") => ReplyMode::Reply,
            Some("all") => ReplyMode::All,
            Some("forward") => ReplyMode::Forward,
            Some(_) => return Ok(bad_request("mode must be reply, all or forward")),
        };
        let compose = match self.composer() {
            Ok(c) => c.clone(),
            Err(resp) => return Ok(resp),
        };
        let record = match self.record(id).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let raw = match self.raw(&record).await? {
            Ok(raw) => raw,
            Err(resp) => return Ok(resp),
        };
        let identities = match compose.identities().await {
            Ok(i) => i,
            Err(e) => return Ok(error_response(e)),
        };
        let forward = mode == ReplyMode::Forward;
        let (original, parts) = crate::messages::blocking(move || {
            let message = mime::parse(&raw);
            let parts = if forward {
                forwarded_parts(&raw)
            } else {
                Vec::new()
            };
            let original = Original {
                reply_to: message
                    .as_ref()
                    .map(|m| mime::addresses(m.reply_to()))
                    .unwrap_or_default(),
                text: message
                    .as_ref()
                    .and_then(|m| m.body_text(0))
                    .map(|t| t.into_owned())
                    .unwrap_or_default(),
                record,
            };
            (original, parts)
        })
        .await?;
        let mut fields = reply_fields(&original, mode, &identities);
        if !parts.is_empty() {
            fields.attachments = match compose.add_parts(parts).await {
                Ok(attachments) => attachments,
                Err(e) => return Ok(error_response(e)),
            };
        }
        Ok(Response::json(200, &fields))
    }
}

#[cfg(test)]
mod tests {
    use tagmail_core::{AccountId, ThreadId};

    use super::*;

    fn addr(email: &str) -> AddressDto {
        AddressDto {
            name: None,
            email: email.to_owned(),
        }
    }

    fn identity(id: i64, account_id: i64, address: &str, is_default: bool) -> IdentityDto {
        IdentityDto {
            id,
            account_id,
            address: address.to_owned(),
            name: None,
            signature: None,
            is_default,
        }
    }

    fn original(from: &str, to: &[&str], cc: &[&str]) -> Original {
        Original {
            record: MessageRecord {
                id: MessageRowId(7),
                account_id: AccountId(1),
                thread_id: ThreadId(3),
                date: 1_790_000_000,
                subject: Some("Plans".to_owned()),
                snippet: String::new(),
                from: vec![addr(from)],
                to: to.iter().map(|a| addr(a)).collect(),
                cc: cc.iter().map(|a| addr(a)).collect(),
                unread: false,
                starred: false,
                has_attachment: false,
            },
            reply_to: Vec::new(),
            text: "line one\n\nline two".to_owned(),
        }
    }

    #[test]
    fn identity_matches_the_address_the_mail_was_sent_to() {
        let ids = [
            identity(1, 1, "me@a.example", true),
            identity(2, 1, "alias@a.example", false),
            identity(3, 2, "other@b.example", true),
        ];
        let pick = |account, rcpt: &[&str]| {
            let rcpt: Vec<AddressDto> = rcpt.iter().map(|a| addr(a)).collect();
            choose_identity(&ids, account, &rcpt).map(|i| i.id)
        };
        assert_eq!(pick(1, &["x@y", "ALIAS@a.example"]), Some(2));
        assert_eq!(pick(1, &["list@lists.example"]), Some(1));
        assert_eq!(
            pick(1, &["other@b.example"]),
            Some(1),
            "never another account's identity"
        );
        assert_eq!(pick(2, &[]), Some(3));
        assert_eq!(pick(9, &["me@a.example"]), None);
    }

    #[test]
    fn reply_all_addresses_everyone_but_me_and_quotes() {
        let ids = [identity(1, 1, "me@a.example", true)];
        let o = original("ann@x", &["me@a.example", "bob@x"], &["cat@x", "ann@x"]);
        let r = reply_fields(&o, ReplyMode::All, &ids);
        assert_eq!(r.identity_id, Some(1));
        assert_eq!(r.to, [addr("ann@x")]);
        assert_eq!(r.cc, [addr("bob@x"), addr("cat@x")]);
        assert_eq!(r.subject, "Re: Plans");
        assert!(r.html.contains("ann@x wrote:"), "{}", r.html);
        assert!(r.html.contains("<blockquote>"), "{}", r.html);
        assert!(r.html.contains("line one"), "{}", r.html);
        assert_eq!((r.in_reply_to, r.forward_of), (Some(7), None));

        let r = reply_fields(&o, ReplyMode::Reply, &ids);
        assert_eq!((r.to, r.cc), (vec![addr("ann@x")], vec![]));

        let mine = original("me@a.example", &["bob@x"], &[]);
        assert_eq!(
            reply_fields(&mine, ReplyMode::Reply, &ids).to,
            [addr("bob@x")]
        );

        let mut listed = original("ann@x", &["me@a.example"], &[]);
        listed.reply_to = vec![addr("list@x")];
        assert_eq!(
            reply_fields(&listed, ReplyMode::Reply, &ids).to,
            [addr("list@x")]
        );
    }

    #[test]
    fn forward_carries_the_original_without_recipients() {
        let ids = [identity(1, 1, "me@a.example", true)];
        let mut o = original("ann@x", &["me@a.example"], &[]);
        o.record.subject = Some("Fwd: Plans".to_owned());
        let r = reply_fields(&o, ReplyMode::Forward, &ids);
        assert!(r.to.is_empty() && r.cc.is_empty());
        assert_eq!(r.subject, "Fwd: Plans");
        assert!(r.html.contains("From: ann@x"), "{}", r.html);
        assert!(r.html.contains("line two"), "{}", r.html);
        assert_eq!((r.in_reply_to, r.forward_of), (None, Some(7)));
        assert_eq!(reply_subject("RE: x", ReplyMode::Reply), "RE: x");
        assert_eq!(reply_subject("Read me", ReplyMode::Reply), "Re: Read me");
    }

    #[test]
    fn references_continue_the_chain() {
        let raw = b"Message-ID: <c@x>\r\nIn-Reply-To: <b@x>\r\nReferences: <a@x> <b@x>\r\n\
                    Subject: s\r\n\r\nbody";
        assert_eq!(
            references_of(raw),
            Some(ReplyRefs {
                in_reply_to: "c@x".to_owned(),
                references: vec!["a@x".into(), "b@x".into(), "c@x".into()],
            })
        );
        let raw = b"Message-ID: <c@x>\r\nIn-Reply-To: <b@x>\r\n\r\nbody";
        assert_eq!(references_of(raw).unwrap().references, ["b@x", "c@x"]);
    }

    #[test]
    fn forwarded_attachments_are_extracted() {
        let raw = b"Message-ID: <f@x>\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\
            Content-Type: text/plain\r\n\r\nhi\r\n--b\r\n\
            Content-Type: application/pdf\r\n\
            Content-Disposition: attachment; filename=\"../r.pdf\"\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--b--\r\n";
        assert_eq!(
            forwarded_parts(raw),
            [ForwardedPart {
                name: "r.pdf".into(),
                content_type: "application/pdf".into(),
                bytes: b"%PDF".to_vec(),
            }]
        );
    }

    #[test]
    fn drafts_are_validated() {
        let dto = |to: &str, subject: &str| DraftInputDto {
            identity_id: 1,
            to: vec![AddressDto {
                name: Some("  Ann ".into()),
                email: to.into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            text: None,
            html: None,
            in_reply_to: None,
            forward_of: None,
            attachment_ids: vec!["3".into(), "1".into(), "3".into()],
            save_now: None,
        };
        let ok = validate_draft(dto(" ann@x.example ", " Hi ")).unwrap();
        assert_eq!(ok.to[0].email, "ann@x.example");
        assert_eq!(ok.to[0].name.as_deref(), Some("Ann"));
        assert_eq!(ok.subject, "Hi");
        assert_eq!(ok.attachment_ids, [1, 3]);
        for bad in ["ann", "a b@x", "<a@x>", "a@x,b@x", "@x", "a@"] {
            assert!(
                matches!(
                    validate_draft(dto(bad, "s")),
                    Err(ComposeError::Invalid { field: "to", .. })
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            validate_draft(dto("a@x", "line\r\nBcc: evil@x")),
            Err(ComposeError::Invalid {
                field: "subject",
                ..
            })
        ));
    }

    #[test]
    fn upload_names_and_types_are_cleaned() {
        assert_eq!(clean_file_name("C:\\tmp\\a.txt").as_deref(), Some("a.txt"));
        assert_eq!(
            clean_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(clean_file_name("x\r\ny").as_deref(), Some("xy"));
        assert_eq!(clean_file_name(".."), None);
        assert_eq!(clean_file_name(""), None);
        assert_eq!(clean_content_type(Some("Image/PNG; x=1")), "image/png");
        assert_eq!(
            clean_content_type(Some("text/html\r\nX: y")),
            "application/octet-stream"
        );
        assert_eq!(clean_content_type(None), "application/octet-stream");
    }
}
