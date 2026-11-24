//! Desktop compose (§5, §7): drafts, uploads and the outbox live in
//! `replica-durable.db` first, so composing and sending work offline; the
//! flush in [`crate::outbox`] carries them to the server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use tagmail_api_core::compose::{
    Compose, ComposeError, ComposeResult, DraftInput, DraftOutcome, ForwardedPart,
    MAX_ATTACHMENTS_TOTAL, MAX_UPLOAD, NewUpload, SendOutcome,
};
use tagmail_api_core::compose_dto::{
    AttachmentDto, DraftDto, IdentityDto, OutboxItemDto, OutboxStateDto, SendStatusDto,
};
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::identities::{IdentityPatch, NewIdentity};
use tagmail_api_core::store::BoxFuture;
use tagmail_store::changes::{self, ChangeOp, Entity, NewChange};

use crate::sync::{Replica, now};

/// Edits settle this long before a draft is saved on the server.
pub const DRAFT_DEBOUNCE: Duration = Duration::from_secs(2);
pub const NOT_READY: &str = "the replica is not ready yet";
const UPLOADS: &str = "uploads";

/// A draft as the client last wrote it, with IMAP-stable references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub identity_id: i64,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    /// Message-IDs without angle brackets.
    pub in_reply_to: Option<String>,
    pub forward_of: Option<String>,
    pub save_now: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: i64,
    pub client_id: String,
    pub server_draft_id: Option<i64>,
    pub account_id: i64,
    pub content: Content,
    pub version: i64,
    pub updated_at: i64,
    pub synced_version: Option<i64>,
    pub sync_error: Option<String>,
    pub deleted: bool,
}

impl Draft {
    pub fn dirty(&self) -> bool {
        self.synced_version != Some(self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub id: i64,
    pub client_id: Option<String>,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub path: PathBuf,
    pub server_upload_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    Queued,
    Uploading,
    /// The server accepted it; its own outbox sends it.
    Submitted,
    Sent,
    Failed,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Queued => "queued",
            OutboxState::Uploading => "uploading",
            OutboxState::Submitted => "submitted",
            OutboxState::Sent => "sent",
            OutboxState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<OutboxState> {
        Ok(match s {
            "queued" => OutboxState::Queued,
            "uploading" => OutboxState::Uploading,
            "submitted" => OutboxState::Submitted,
            "sent" => OutboxState::Sent,
            "failed" => OutboxState::Failed,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("outbox state {other:?}").into(),
                ));
            }
        })
    }

    pub fn dto(self) -> OutboxStateDto {
        match self {
            OutboxState::Queued | OutboxState::Uploading => OutboxStateDto::Queued,
            OutboxState::Submitted => OutboxStateDto::Sending,
            OutboxState::Sent => OutboxStateDto::Sent,
            OutboxState::Failed => OutboxStateDto::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub op_id: String,
    pub client_id: String,
    pub created_at: i64,
    pub state: OutboxState,
    pub attempts: u32,
    pub next_at: Option<i64>,
    pub error: Option<String>,
}

const DRAFT_COLUMNS: &str = "id, client_id, server_draft_id, account_id, content, version, \
     updated_at, synced_version, sync_error, deleted_at";

fn draft_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    let content: String = r.get(4)?;
    Ok(Draft {
        id: r.get(0)?,
        client_id: r.get(1)?,
        server_draft_id: r.get(2)?,
        account_id: r.get(3)?,
        content: serde_json::from_str(&content).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?,
        version: r.get(5)?,
        updated_at: r.get(6)?,
        synced_version: r.get(7)?,
        sync_error: r.get(8)?,
        deleted: r.get::<_, Option<i64>>(9)?.is_some(),
    })
}

const UPLOAD_COLUMNS: &str = "id, client_id, name, content_type, size, path, server_upload_id";

fn upload_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Upload> {
    Ok(Upload {
        id: r.get(0)?,
        client_id: r.get(1)?,
        name: r.get(2)?,
        content_type: r.get(3)?,
        size: r.get::<_, i64>(4)? as u64,
        path: PathBuf::from(r.get::<_, String>(5)?),
        server_upload_id: r.get(6)?,
    })
}

const OUTBOX_COLUMNS: &str = "id, op_id, client_id, created_at, state, attempts, next_at, error";

fn outbox_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    Ok(OutboxRow {
        id: r.get(0)?,
        op_id: r.get(1)?,
        client_id: r.get(2)?,
        created_at: r.get(3)?,
        state: OutboxState::parse(&r.get::<_, String>(4)?)?,
        attempts: r.get(5)?,
        next_at: r.get(6)?,
        error: r.get(7)?,
    })
}

pub fn draft_by_id(c: &Connection, id: i64) -> rusqlite::Result<Option<Draft>> {
    c.query_row(
        &format!("SELECT {DRAFT_COLUMNS} FROM draft WHERE id = ?1 AND deleted_at IS NULL"),
        [id],
        draft_row,
    )
    .optional()
}

pub fn draft_by_client_id(c: &Connection, client_id: &str) -> rusqlite::Result<Option<Draft>> {
    c.query_row(
        &format!("SELECT {DRAFT_COLUMNS} FROM draft WHERE client_id = ?1"),
        [client_id],
        draft_row,
    )
    .optional()
}

/// Every draft, deleted ones included.
pub fn drafts(c: &Connection) -> rusqlite::Result<Vec<Draft>> {
    let mut stmt = c.prepare(&format!("SELECT {DRAFT_COLUMNS} FROM draft ORDER BY id"))?;
    stmt.query_map([], draft_row)?.collect()
}

pub fn uploads_of(c: &Connection, client_id: &str) -> rusqlite::Result<Vec<Upload>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {UPLOAD_COLUMNS} FROM draft_attachment WHERE client_id = ?1 ORDER BY id"
    ))?;
    stmt.query_map([client_id], upload_row)?.collect()
}

fn upload(c: &Connection, id: i64) -> rusqlite::Result<Option<Upload>> {
    c.query_row(
        &format!("SELECT {UPLOAD_COLUMNS} FROM draft_attachment WHERE id = ?1"),
        [id],
        upload_row,
    )
    .optional()
}

pub fn outbox_rows(c: &Connection) -> rusqlite::Result<Vec<OutboxRow>> {
    let mut stmt = c.prepare(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox ORDER BY id"))?;
    stmt.query_map([], outbox_row)?.collect()
}

fn outbox_by_op(c: &Connection, op_id: &str) -> rusqlite::Result<Option<OutboxRow>> {
    c.query_row(
        &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE op_id = ?1"),
        [op_id],
        outbox_row,
    )
    .optional()
}

fn outbox_by_id(c: &Connection, id: i64) -> rusqlite::Result<Option<OutboxRow>> {
    c.query_row(
        &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"),
        [id],
        outbox_row,
    )
    .optional()
}

fn outbox_of_draft(c: &Connection, client_id: &str) -> rusqlite::Result<Option<OutboxRow>> {
    c.query_row(
        &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE client_id = ?1"),
        [client_id],
        outbox_row,
    )
    .optional()
}

/// Removes upload files; a file already gone is fine.
pub fn remove_files(paths: impl IntoIterator<Item = PathBuf>) {
    for p in paths {
        if let Err(e) = std::fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %p.display(), error = %e, "cannot remove an upload");
        }
    }
}

/// Deletes the draft with its uploads and outbox row; returns the files.
pub fn delete_draft_row(tx: &Transaction<'_>, client_id: &str) -> rusqlite::Result<Vec<PathBuf>> {
    let files = uploads_of(tx, client_id)?
        .into_iter()
        .map(|u| u.path)
        .collect();
    tx.execute("DELETE FROM outbox WHERE client_id = ?1", [client_id])?;
    tx.execute("DELETE FROM draft WHERE client_id = ?1", [client_id])?;
    Ok(files)
}

pub fn random_id() -> ComposeResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| ComposeError::Internal(format!("random: {e}").into()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn invalid(field: &'static str, message: &str) -> ComposeError {
    ComposeError::Invalid {
        field,
        message: message.to_owned(),
    }
}

fn content_of(input: &DraftInput) -> Content {
    Content {
        identity_id: input.identity_id,
        to: input.to.clone(),
        cc: input.cc.clone(),
        bcc: input.bcc.clone(),
        subject: input.subject.clone(),
        text: input.text.clone(),
        html: input.html.clone(),
        in_reply_to: input.reply.as_ref().map(|r| r.in_reply_to.clone()),
        forward_of: input.forward_of.clone(),
        save_now: input.save_now,
    }
}

/// Points the draft's uploads at exactly `ids`; returns removed files.
fn attach(tx: &Transaction<'_>, client_id: &str, ids: &[i64]) -> ComposeResult<Vec<PathBuf>> {
    let mut total = 0u64;
    for &id in ids {
        match upload(tx, id)? {
            Some(u) if u.client_id.as_deref().is_none_or(|c| c == client_id) => total += u.size,
            _ => return Err(invalid("attachmentIds", "no such attachment")),
        }
    }
    if total > MAX_ATTACHMENTS_TOTAL {
        return Err(ComposeError::TooLarge(format!(
            "attachments are at most {MAX_ATTACHMENTS_TOTAL} bytes in total"
        )));
    }
    let mut removed = Vec::new();
    for u in uploads_of(tx, client_id)? {
        if !ids.contains(&u.id) {
            tx.execute("DELETE FROM draft_attachment WHERE id = ?1", [u.id])?;
            removed.push(u.path);
        }
    }
    for &id in ids {
        tx.execute(
            "UPDATE draft_attachment SET client_id = ?2 WHERE id = ?1",
            params![id, client_id],
        )?;
    }
    Ok(removed)
}

fn attachment_dto(u: &Upload) -> AttachmentDto {
    AttachmentDto {
        id: u.id.to_string(),
        name: u.name.clone(),
        size: u.size,
        content_type: u.content_type.clone(),
    }
}

fn draft_dto(c: &Connection, d: &Draft) -> rusqlite::Result<DraftDto> {
    let uploads = uploads_of(c, &d.client_id)?;
    let queued = outbox_of_draft(c, &d.client_id)?.is_some();
    Ok(DraftDto {
        id: d.id,
        account_id: d.account_id,
        identity_id: Some(d.content.identity_id),
        to: d.content.to.clone(),
        cc: d.content.cc.clone(),
        bcc: d.content.bcc.clone(),
        subject: d.content.subject.clone(),
        text: d.content.text.clone(),
        html: d.content.html.clone(),
        in_reply_to_message_id: d.content.in_reply_to.clone(),
        attachment_ids: uploads.iter().map(|u| u.id.to_string()).collect(),
        attachments: uploads.iter().map(attachment_dto).collect(),
        version: d.version,
        saved: !d.dirty() && d.server_draft_id.is_some(),
        save_error: d.sync_error.clone(),
        queued,
    })
}

fn outbox_item(c: &Connection, o: &OutboxRow) -> rusqlite::Result<Option<OutboxItemDto>> {
    let Some(d) = draft_by_client_id(c, &o.client_id)? else {
        return Ok(None);
    };
    Ok(Some(OutboxItemDto {
        id: o.id,
        op_id: o.op_id.clone(),
        account_id: d.account_id,
        draft_id: Some(d.id),
        subject: d.content.subject.clone(),
        to: d.content.to.clone(),
        status: o.state.dto(),
        attempts: o.attempts,
        error: o.error.clone(),
        created_at: o.created_at,
        sent_at: None,
    }))
}

fn identity_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityDto> {
    Ok(IdentityDto {
        id: r.get(0)?,
        account_id: r.get(1)?,
        address: r.get(2)?,
        name: r.get(3)?,
        signature: r.get(4)?,
        is_default: r.get(5)?,
    })
}

const IDENTITY_COLUMNS: &str = "id, account_id, address, display_name, signature_html, is_default";

impl Replica {
    pub fn upload_dir(&self) -> PathBuf {
        self.layout().root().join(UPLOADS)
    }

    async fn identities(&self) -> ComposeResult<Vec<IdentityDto>> {
        let store = self.store().ok_or_else(not_ready)?;
        Ok(store
            .config()
            .read(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {IDENTITY_COLUMNS} FROM identity ORDER BY account_id, is_default DESC, id"
                ))?;
                let rows = stmt.query_map([], identity_row)?.collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?)
    }

    async fn identity(&self, id: i64) -> ComposeResult<IdentityDto> {
        let store = self.store().ok_or_else(not_ready)?;
        store
            .config()
            .read(move |c| {
                Ok(c.query_row(
                    &format!("SELECT {IDENTITY_COLUMNS} FROM identity WHERE id = ?1"),
                    [id],
                    identity_row,
                )
                .optional()?)
            })
            .await?
            .ok_or_else(|| invalid("identityId", "no such identity"))
    }

    /// Wakes the flush once the debounce has passed.
    fn schedule_draft_sync(self: &Arc<Self>, now_: bool) {
        if now_ {
            self.kick_flush();
            return;
        }
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(DRAFT_DEBOUNCE + Duration::from_secs(1)).await;
            if let Some(me) = me.upgrade() {
                me.kick_flush();
            }
        });
    }

    /// Logs an outbox change locally so the SPA refreshes its list.
    pub(crate) async fn publish_outbox(
        &self,
        id: i64,
        state: Option<(OutboxState, Option<String>)>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        let (op, payload) = match state {
            Some((s, error)) => (
                ChangeOp::Upsert,
                Some(serde_json::json!({ "status": s.dto(), "error": error }).to_string()),
            ),
            None => (ChangeOp::Delete, None),
        };
        let change = NewChange {
            entity: Entity::Outbox,
            entity_id: id,
            op,
            payload,
        };
        let epoch = store.cache().epoch();
        match store
            .cache()
            .write(move |tx| {
                changes::append(tx, &change)?;
                changes::head(tx, epoch)
            })
            .await
        {
            Ok(head) => self.feed().publish(head),
            Err(e) => tracing::warn!(outbox = id, error = %e, "cannot log an outbox change"),
        }
    }

    fn store_parts(&self, parts: Vec<ForwardedPart>) -> ComposeResult<Vec<i64>> {
        let dir = self.upload_dir();
        std::fs::create_dir_all(&dir)?;
        let mut ids = Vec::with_capacity(parts.len());
        for part in parts {
            let path = dir.join(format!("part-{}", random_id()?));
            std::fs::write(&path, &part.bytes)?;
            ids.push(self.insert_upload(
                &part.name,
                &part.content_type,
                part.bytes.len() as u64,
                &path,
            )?);
        }
        Ok(ids)
    }

    fn insert_upload(
        &self,
        name: &str,
        content_type: &str,
        size: u64,
        path: &Path,
    ) -> ComposeResult<i64> {
        let conn = self.durable().conn();
        conn.execute(
            "INSERT INTO draft_attachment (name, content_type, size, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                name,
                content_type,
                size as i64,
                path.to_string_lossy(),
                now()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn local_draft(&self, id: i64) -> ComposeResult<DraftDto> {
        let conn = self.durable().conn();
        let d = draft_by_id(&conn, id)?.ok_or(ComposeError::NotFound("draft"))?;
        Ok(draft_dto(&conn, &d)?)
    }

    async fn create_draft(self: &Arc<Self>, mut input: DraftInput) -> ComposeResult<DraftOutcome> {
        let identity = self.identity(input.identity_id).await?;
        let client_id = match input.client_id.take() {
            Some(c) => c,
            None => random_id()?,
        };
        let existing = draft_by_client_id(&self.durable().conn(), &client_id)?;
        if let Some(d) = existing {
            if d.deleted {
                return Err(ComposeError::Conflict("the draft was deleted".into()));
            }
            let draft = self.update_draft(d.id, input).await?;
            return Ok(DraftOutcome {
                draft,
                created: false,
            });
        }
        if !input.forwarded.is_empty() {
            let parts = std::mem::take(&mut input.forwarded);
            input.attachment_ids.extend(self.store_parts(parts)?);
        }
        let content = serde_json::to_string(&content_of(&input))
            .map_err(|e| ComposeError::Internal(e.into()))?;
        let (id, removed) = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO draft (client_id, account_id, identity_id, content, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![client_id, identity.account_id, identity.id, content, now()],
            )?;
            let id = tx.last_insert_rowid();
            let removed = attach(&tx, &client_id, &input.attachment_ids)?;
            tx.commit()?;
            (id, removed)
        };
        remove_files(removed);
        self.schedule_draft_sync(input.save_now);
        Ok(DraftOutcome {
            draft: self.local_draft(id)?,
            created: true,
        })
    }

    async fn update_draft(self: &Arc<Self>, id: i64, input: DraftInput) -> ComposeResult<DraftDto> {
        let identity = self.identity(input.identity_id).await?;
        let removed = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            let d = draft_by_id(&tx, id)?.ok_or(ComposeError::NotFound("draft"))?;
            if outbox_of_draft(&tx, &d.client_id)?.is_some() {
                return Err(ComposeError::Conflict(
                    "the draft is queued for sending".into(),
                ));
            }
            let mut content = content_of(&input);
            content.in_reply_to = d.content.in_reply_to.clone();
            content.forward_of = d.content.forward_of.clone();
            let content =
                serde_json::to_string(&content).map_err(|e| ComposeError::Internal(e.into()))?;
            tx.execute(
                "UPDATE draft SET account_id = ?2, identity_id = ?3, content = ?4,
                     version = version + 1, updated_at = ?5
                 WHERE id = ?1",
                params![id, identity.account_id, identity.id, content, now()],
            )?;
            let removed = attach(&tx, &d.client_id, &input.attachment_ids)?;
            tx.commit()?;
            removed
        };
        remove_files(removed);
        self.schedule_draft_sync(input.save_now);
        self.local_draft(id)
    }

    fn delete_draft(&self, id: i64) -> ComposeResult<()> {
        let files = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            let d = draft_by_id(&tx, id)?.ok_or(ComposeError::NotFound("draft"))?;
            if outbox_of_draft(&tx, &d.client_id)?.is_some() {
                return Err(ComposeError::Conflict(
                    "the draft is queued for sending".into(),
                ));
            }
            let files = if d.server_draft_id.is_some() {
                tx.execute(
                    "UPDATE draft SET deleted_at = ?2 WHERE id = ?1",
                    params![id, now()],
                )?;
                Vec::new()
            } else {
                delete_draft_row(&tx, &d.client_id)?
            };
            tx.commit()?;
            files
        };
        remove_files(files);
        self.kick_flush();
        Ok(())
    }

    fn add_upload(&self, u: NewUpload) -> ComposeResult<AttachmentDto> {
        if u.size > MAX_UPLOAD {
            remove_files([u.file]);
            return Err(ComposeError::TooLarge(format!(
                "attachments are at most {MAX_UPLOAD} bytes"
            )));
        }
        let id = self.insert_upload(&u.name, &u.content_type, u.size, &u.file)?;
        Ok(AttachmentDto {
            id: id.to_string(),
            name: u.name,
            size: u.size,
            content_type: u.content_type,
        })
    }

    fn delete_upload(&self, id: i64) -> ComposeResult<()> {
        let path = {
            let conn = self.durable().conn();
            let u = upload(&conn, id)?.ok_or(ComposeError::NotFound("attachment"))?;
            if let Some(client_id) = &u.client_id
                && outbox_of_draft(&conn, client_id)?.is_some()
            {
                return Err(ComposeError::Conflict(
                    "the draft is queued for sending".into(),
                ));
            }
            conn.execute("DELETE FROM draft_attachment WHERE id = ?1", [id])?;
            u.path
        };
        remove_files([path]);
        Ok(())
    }

    async fn send(&self, op_id: String, draft_id: i64) -> ComposeResult<SendOutcome> {
        let id = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            if let Some(o) = outbox_by_op(&tx, &op_id)? {
                return Ok(SendOutcome {
                    status: SendStatusDto {
                        op_id,
                        status: o.state.dto(),
                    },
                    created: false,
                });
            }
            let d = draft_by_id(&tx, draft_id)?.ok_or(ComposeError::NotFound("draft"))?;
            if d.content.to.is_empty() && d.content.cc.is_empty() && d.content.bcc.is_empty() {
                return Err(invalid("to", "the draft has no recipients"));
            }
            if outbox_of_draft(&tx, &d.client_id)?.is_some() {
                return Err(ComposeError::Conflict("the draft is already queued".into()));
            }
            tx.execute(
                "INSERT INTO outbox (op_id, client_id, created_at, state) VALUES (?1, ?2, ?3, 'queued')",
                params![op_id, d.client_id, now()],
            )?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            id
        };
        self.publish_outbox(id, Some((OutboxState::Queued, None)))
            .await;
        self.kick_flush();
        Ok(SendOutcome {
            status: SendStatusDto {
                op_id,
                status: OutboxStateDto::Queued,
            },
            created: true,
        })
    }

    fn local_outbox(&self) -> ComposeResult<Vec<OutboxItemDto>> {
        let conn = self.durable().conn();
        let mut out = Vec::new();
        for o in outbox_rows(&conn)? {
            if o.state != OutboxState::Sent
                && let Some(item) = outbox_item(&conn, &o)?
            {
                out.push(item);
            }
        }
        Ok(out)
    }

    async fn cancel(&self, id: i64) -> ComposeResult<()> {
        let o = outbox_by_id(&self.durable().conn(), id)?
            .ok_or(ComposeError::NotFound("outbox item"))?;
        if o.state == OutboxState::Sent {
            return Err(ComposeError::Conflict("the message was sent".into()));
        }
        let reached_server = o.attempts > 0 || o.state == OutboxState::Submitted;
        if reached_server {
            self.cancel_on_server(&o.op_id).await?;
        }
        self.durable()
            .conn()
            .execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        self.publish_outbox(id, None).await;
        Ok(())
    }

    async fn retry(&self, id: i64) -> ComposeResult<()> {
        let o = outbox_by_id(&self.durable().conn(), id)?
            .ok_or(ComposeError::NotFound("outbox item"))?;
        if o.state != OutboxState::Failed {
            return Err(ComposeError::Conflict(
                "only failed items can be retried".into(),
            ));
        }
        let state = if self.retry_on_server(&o.op_id).await? {
            OutboxState::Submitted
        } else {
            OutboxState::Queued
        };
        self.durable().conn().execute(
            "UPDATE outbox SET state = ?2, next_at = NULL, error = NULL WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        self.publish_outbox(id, Some((state, None))).await;
        self.kick_flush();
        Ok(())
    }
}

fn not_ready() -> ComposeError {
    ComposeError::Unavailable(NOT_READY.into())
}

fn server_only<T: Send + 'static>() -> BoxFuture<ComposeResult<T>> {
    Box::pin(async {
        Err(ComposeError::Unavailable(
            "identities are managed on the server".into(),
        ))
    })
}

/// [`Compose`] of the desktop over `replica-durable.db`. Identity changes
/// need the server and are passed through before they get here.
pub struct DesktopCompose(pub Arc<Replica>);

impl Compose for DesktopCompose {
    fn identities(&self) -> BoxFuture<ComposeResult<Vec<IdentityDto>>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.identities().await })
    }

    fn create_identity(&self, _: NewIdentity) -> BoxFuture<ComposeResult<IdentityDto>> {
        server_only()
    }

    fn update_identity(&self, _: i64, _: IdentityPatch) -> BoxFuture<ComposeResult<IdentityDto>> {
        server_only()
    }

    fn delete_identity(&self, _: i64) -> BoxFuture<ComposeResult<()>> {
        server_only()
    }

    fn create_draft(&self, input: DraftInput) -> BoxFuture<ComposeResult<DraftOutcome>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.create_draft(input).await })
    }

    fn update_draft(&self, id: i64, input: DraftInput) -> BoxFuture<ComposeResult<DraftDto>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.update_draft(id, input).await })
    }

    fn draft(&self, id: i64) -> BoxFuture<ComposeResult<DraftDto>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.local_draft(id) })
    }

    fn delete_draft(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.delete_draft(id) })
    }

    fn upload_dir(&self) -> PathBuf {
        let dir = self.0.upload_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "cannot create the upload directory");
        }
        dir
    }

    fn add_upload(&self, upload: NewUpload) -> BoxFuture<ComposeResult<AttachmentDto>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.add_upload(upload) })
    }

    fn delete_upload(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.delete_upload(id) })
    }

    fn send(&self, op_id: String, draft_id: i64) -> BoxFuture<ComposeResult<SendOutcome>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.send(op_id, draft_id).await })
    }

    fn outbox(&self) -> BoxFuture<ComposeResult<Vec<OutboxItemDto>>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.local_outbox() })
    }

    fn cancel(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.cancel(id).await })
    }

    fn retry(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.retry(id).await })
    }

    fn add_parts(&self, parts: Vec<ForwardedPart>) -> BoxFuture<ComposeResult<Vec<AttachmentDto>>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move {
            let ids = r.store_parts(parts)?;
            let conn = r.durable().conn();
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(u) = upload(&conn, id)? {
                    out.push(attachment_dto(&u));
                }
            }
            Ok(out)
        })
    }
}
