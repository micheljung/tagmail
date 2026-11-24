//! Carries local drafts and the outbox to the server (§5): uploads, then
//! `POST /api/drafts {clientId}` (idempotent), then `POST /api/send
//! {opId}` (deduplicated by `op_seen`), so a lost acknowledgement replays
//! without a second draft or a second message.

use std::time::Duration;

use reqwest::Method;
use reqwest::multipart::{Form, Part};
use rusqlite::params;
use serde_json::json;
use tagmail_api_core::compose::{ComposeError, ComposeResult};
use tagmail_api_core::compose_dto::{
    AttachmentDto, DraftDto, OutboxDto, OutboxItemDto, OutboxStateDto, SendStatusDto,
};

use crate::client::ClientError;
use crate::compose::{
    self, DRAFT_DEBOUNCE, Draft, OutboxRow, OutboxState, delete_draft_row, remove_files,
};
use crate::durable::FROM_OTHER_SERVER;
use crate::error::{Error, Result};
use crate::sync::{Replica, now};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const RETRY_BASE_SECS: i64 = 5;
const RETRY_MAX_SECS: i64 = 600;
pub const OFFLINE_CHANGE: &str = "Changing a sent message needs a connection";

fn backoff(attempts: u32) -> i64 {
    (RETRY_BASE_SECS << attempts.min(16)).min(RETRY_MAX_SECS)
}

/// Stops the flush: the server is unreachable or the device unpaired.
fn halts(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Offline(_) | ClientError::Unauthorized | ClientError::Gone
    )
}

fn rejected(e: &ClientError) -> Option<&tagmail_api_core::dto::ApiErrorDto> {
    match e {
        ClientError::Api { status, body } if (400..500).contains(status) && *status != 429 => {
            Some(body)
        }
        _ => None,
    }
}

fn offline() -> ComposeError {
    ComposeError::Unavailable(OFFLINE_CHANGE.to_owned())
}

impl Replica {
    fn server_error(&self, e: ClientError) -> ComposeError {
        match e {
            e if halts(&e) => offline(),
            ClientError::Api { status: 409, body } => ComposeError::Conflict(body.message),
            ClientError::Api { status: 404, .. } => ComposeError::NotFound("outbox item"),
            other => ComposeError::Internal(other.into()),
        }
    }

    async fn server_outbox(&self) -> std::result::Result<Vec<OutboxItemDto>, ClientError> {
        Ok(self
            .client()
            .get_json::<OutboxDto>("api/outbox")
            .await?
            .items)
    }

    async fn server_item(&self, op_id: &str) -> ComposeResult<Option<OutboxItemDto>> {
        let items = self
            .server_outbox()
            .await
            .map_err(|e| self.server_error(e))?;
        Ok(items.into_iter().find(|i| i.op_id == op_id))
    }

    pub(crate) async fn cancel_on_server(&self, op_id: &str) -> ComposeResult<()> {
        if let Some(item) = self.server_item(op_id).await? {
            let builder = self
                .client()
                .request(Method::DELETE, &format!("api/outbox/{}", item.id))
                .map_err(|e| self.server_error(e))?;
            self.client()
                .send(builder.timeout(crate::client::REQUEST_TIMEOUT))
                .await
                .map_err(|e| self.server_error(e))?;
        }
        Ok(())
    }

    /// Whether the server had the failed item and queued it again.
    pub(crate) async fn retry_on_server(&self, op_id: &str) -> ComposeResult<bool> {
        if !self.client().is_online() {
            return Ok(false);
        }
        let item = match self.server_item(op_id).await {
            Ok(item) => item,
            Err(ComposeError::Unavailable(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let Some(item) = item.filter(|i| i.status == OutboxStateDto::Failed) else {
            return Ok(false);
        };
        let builder = self
            .client()
            .request(Method::POST, &format!("api/outbox/{}/retry", item.id))
            .map_err(|e| self.server_error(e))?;
        self.client()
            .send(builder.timeout(crate::client::REQUEST_TIMEOUT))
            .await
            .map_err(|e| self.server_error(e))?;
        Ok(true)
    }

    /// Deleted drafts, settled edits, queued sends and submitted sends,
    /// in that order. Stops quietly when the server is unreachable.
    pub(crate) async fn flush_compose(&self) -> Result<()> {
        let drafts = compose::drafts(&self.durable().conn())?;
        for d in drafts.iter().filter(|d| d.deleted) {
            self.delete_server_draft(d).await?;
        }
        let queued: Vec<String> = compose::outbox_rows(&self.durable().conn())?
            .into_iter()
            .map(|o| o.client_id)
            .collect();
        let settled = now() - DRAFT_DEBOUNCE.as_secs() as i64;
        for d in drafts.iter().filter(|d| {
            !d.deleted
                && !d.other_server
                && d.dirty()
                && !queued.contains(&d.client_id)
                && (d.updated_at <= settled || d.content.save_now)
        }) {
            self.sync_draft(d).await?;
        }
        let rows = compose::outbox_rows(&self.durable().conn())?;
        let t = now();
        for o in rows.iter().filter(|o| {
            matches!(o.state, OutboxState::Queued | OutboxState::Uploading)
                && o.next_at.is_none_or(|at| at <= t)
        }) {
            self.submit(o).await?;
        }
        let submitted: Vec<OutboxRow> = compose::outbox_rows(&self.durable().conn())?
            .into_iter()
            .filter(|o| o.state == OutboxState::Submitted)
            .collect();
        if !submitted.is_empty() {
            self.settle(submitted).await?;
        }
        Ok(())
    }

    async fn delete_server_draft(&self, d: &Draft) -> Result<()> {
        let current = compose::draft_by_client_id(&self.durable().conn(), &d.client_id)?;
        if let Some(sid) = current
            .filter(|d| !d.other_server)
            .and_then(|d| d.server_draft_id)
        {
            let builder = self
                .client()
                .request(Method::DELETE, &format!("api/drafts/{sid}"))?
                .timeout(crate::client::REQUEST_TIMEOUT);
            match self.client().send(builder).await {
                Ok(_) => {}
                Err(ClientError::Api { status: 404, .. }) => {}
                Err(e) if halts(&e) => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(draft = d.id, error = %e, "cannot delete a draft on the server");
                    return Ok(());
                }
            }
        }
        let files = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            let files = delete_draft_row(&tx, &d.client_id)?;
            tx.commit()?;
            files
        };
        remove_files(files);
        Ok(())
    }

    async fn upload(&self, u: &compose::Upload) -> Result<i64> {
        let bytes = std::fs::read(&u.path)?;
        let part = Part::bytes(bytes)
            .file_name(u.name.clone())
            .mime_str(&u.content_type)
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        let builder = self
            .client()
            .request(Method::POST, "api/attachments")?
            .timeout(UPLOAD_TIMEOUT)
            .multipart(Form::new().part("file", part));
        let resp = self.client().send(builder).await?;
        let dto: AttachmentDto = resp
            .json()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        let id = dto
            .id
            .parse()
            .map_err(|_| ClientError::Decode(format!("upload id {:?}", dto.id)))?;
        self.durable().conn().execute(
            "UPDATE draft_attachment SET server_upload_id = ?2 WHERE id = ?1",
            params![u.id, id],
        )?;
        Ok(id)
    }

    /// The server's message id for a Message-ID the replica knows.
    async fn message_id_of(&self, msgid: Option<&str>) -> Option<i64> {
        let msgid = msgid?.to_owned();
        let store = self.store()?;
        store
            .cache()
            .read(move |c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT id FROM message WHERE msgid_hdr = ?1 OR msgid_hdr = '<' || ?1 || '>'
                     ORDER BY id LIMIT 1",
                    [&msgid],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .await
            .ok()
            .flatten()
    }

    /// Saves the draft on the server; `None` if the server refused it
    /// (recorded as the draft's save error).
    async fn sync_draft(&self, d: &Draft) -> Result<Option<i64>> {
        let uploads = compose::uploads_of(&self.durable().conn(), &d.client_id)?;
        let mut ids = Vec::with_capacity(uploads.len());
        for u in &uploads {
            let id = match u.server_upload_id {
                Some(id) => id,
                None => match self.upload(u).await {
                    Ok(id) => id,
                    Err(Error::Client(e)) => match rejected(&e) {
                        Some(body) => {
                            let message = body.message.clone();
                            self.record_sync(d, None, Some(&message))?;
                            return Ok(None);
                        }
                        None => return Err(e.into()),
                    },
                    Err(e) => return Err(e),
                },
            };
            ids.push(id.to_string());
        }
        let c = &d.content;
        let mut body = json!({
            "clientId": d.client_id,
            "identityId": c.identity_id,
            "to": c.to,
            "cc": c.cc,
            "bcc": c.bcc,
            "subject": c.subject,
            "text": c.text,
            "html": c.html,
            "attachmentIds": ids,
            "saveNow": c.save_now,
        });
        if d.server_draft_id.is_none() {
            body["inReplyTo"] = json!(self.message_id_of(c.in_reply_to.as_deref()).await);
            body["forwardOf"] = json!(self.message_id_of(c.forward_of.as_deref()).await);
        }
        match self
            .client()
            .post_json::<_, DraftDto>("api/drafts", &body)
            .await
        {
            Ok(server) => {
                self.record_sync(d, Some(server.id), None)?;
                Ok(Some(server.id))
            }
            Err(e) => match rejected(&e) {
                Some(body) => {
                    if body.field.as_deref() == Some("attachmentIds") {
                        self.durable()
                            .conn()
                            .execute(
                                "UPDATE draft_attachment SET server_upload_id = NULL WHERE client_id = ?1",
                                [&d.client_id],
                            )
                            ?;
                    }
                    let message = body.message.clone();
                    self.record_sync(d, None, Some(&message))?;
                    Ok(None)
                }
                None => Err(e.into()),
            },
        }
    }

    fn record_sync(&self, d: &Draft, server_id: Option<i64>, error: Option<&str>) -> Result<()> {
        self.durable().conn().execute(
            "UPDATE draft SET server_draft_id = coalesce(?2, server_draft_id),
                     synced_version = ?3, sync_error = ?4
                 WHERE id = ?1 AND other_server = 0",
            params![d.id, server_id, d.version, error],
        )?;
        Ok(())
    }

    async fn set_outbox(
        &self,
        o: &OutboxRow,
        state: OutboxState,
        error: Option<&str>,
        next_at: Option<i64>,
    ) -> Result<()> {
        let changed = self.durable().conn().execute(
            "UPDATE outbox SET state = ?2, error = ?3, next_at = ?4
             WHERE id = ?1 AND NOT (state = 'failed' AND error IS ?5)",
            params![o.id, state.as_str(), error, next_at, FROM_OTHER_SERVER],
        )?;
        if changed == 0 {
            return Ok(());
        }
        self.publish_outbox(o.id, Some((state, error.map(str::to_owned))))
            .await;
        Ok(())
    }

    /// A sent message leaves no draft behind, as on the server.
    async fn finish(&self, o: &OutboxRow) -> Result<()> {
        if self.quarantined(o)? {
            return Ok(());
        }
        let files = {
            let mut conn = self.durable().conn();
            let tx = conn.transaction()?;
            let files = delete_draft_row(&tx, &o.client_id)?;
            tx.commit()?;
            files
        };
        remove_files(files);
        self.publish_outbox(o.id, Some((OutboxState::Sent, None)))
            .await;
        self.publish_outbox(o.id, None).await;
        Ok(())
    }

    async fn submit(&self, o: &OutboxRow) -> Result<()> {
        let Some(d) = compose::draft_by_client_id(&self.durable().conn(), &o.client_id)? else {
            return Ok(());
        };
        if d.other_server {
            return self
                .set_outbox(o, OutboxState::Failed, Some(FROM_OTHER_SERVER), None)
                .await;
        }
        self.durable().conn().execute(
            "UPDATE outbox SET attempts = attempts + 1 WHERE id = ?1",
            [o.id],
        )?;
        let server_draft = if d.dirty() || d.server_draft_id.is_none() {
            self.set_outbox(o, OutboxState::Uploading, None, None)
                .await?;
            match self.sync_draft(&d).await {
                Ok(Some(id)) => Some(id),
                Ok(None) => None,
                Err(Error::Client(e)) if halts(&e) => {
                    self.set_outbox(o, OutboxState::Queued, None, None).await?;
                    return Err(e.into());
                }
                Err(e) => {
                    let at = now() + backoff(o.attempts);
                    return self
                        .set_outbox(o, OutboxState::Queued, Some(&e.to_string()), Some(at))
                        .await;
                }
            }
        } else {
            d.server_draft_id
        };
        let Some(draft_id) = server_draft else {
            let error = compose::draft_by_client_id(&self.durable().conn(), &o.client_id)?
                .and_then(|d| d.sync_error)
                .unwrap_or_else(|| "the server refused the draft".to_owned());
            return self
                .set_outbox(o, OutboxState::Failed, Some(&error), None)
                .await;
        };
        let _sending = self.send_lock().lock().await;
        if self.send_target(o)? != Some(draft_id) {
            return Ok(());
        }
        let body = json!({ "opId": o.op_id, "draftId": draft_id });
        match self
            .client()
            .post_json::<_, SendStatusDto>("api/send", &body)
            .await
        {
            Ok(status) => self.apply_status(o, status.status, None).await,
            Err(e) if halts(&e) => {
                self.set_outbox(o, OutboxState::Queued, None, None).await?;
                Err(e.into())
            }
            Err(e) => match rejected(&e) {
                Some(body) => {
                    let message = body.message.clone();
                    self.set_outbox(o, OutboxState::Failed, Some(&message), None)
                        .await
                }
                None => {
                    let at = now() + backoff(o.attempts);
                    self.set_outbox(o, OutboxState::Queued, Some(&e.to_string()), Some(at))
                        .await
                }
            },
        }
    }

    /// The server draft `o` sends, unless the item is gone or belongs to
    /// another server.
    fn send_target(&self, o: &OutboxRow) -> Result<Option<i64>> {
        if self.quarantined(o)? {
            return Ok(None);
        }
        Ok(
            compose::draft_by_client_id(&self.durable().conn(), &o.client_id)?
                .and_then(|d| d.server_draft_id),
        )
    }

    /// Gone, or left behind by pairing with another server.
    fn quarantined(&self, o: &OutboxRow) -> Result<bool> {
        let conn = self.durable().conn();
        let live = compose::outbox_by_id(&conn, o.id)?.is_some_and(|o| {
            !(o.state == OutboxState::Failed && o.error.as_deref() == Some(FROM_OTHER_SERVER))
        });
        Ok(!live
            || compose::draft_by_client_id(&conn, &o.client_id)?.is_none_or(|d| d.other_server))
    }

    async fn apply_status(
        &self,
        o: &OutboxRow,
        status: OutboxStateDto,
        error: Option<String>,
    ) -> Result<()> {
        match status {
            OutboxStateDto::Queued | OutboxStateDto::Sending => {
                if o.state != OutboxState::Submitted {
                    self.set_outbox(o, OutboxState::Submitted, None, None)
                        .await?;
                }
                Ok(())
            }
            OutboxStateDto::Sent => self.finish(o).await,
            OutboxStateDto::Failed => {
                let error = error.unwrap_or_else(|| "sending failed".to_owned());
                self.set_outbox(o, OutboxState::Failed, Some(&error), None)
                    .await
            }
        }
    }

    /// Follows submitted sends to `sent` or `failed` on the server.
    async fn settle(&self, submitted: Vec<OutboxRow>) -> Result<()> {
        let items = self.server_outbox().await?;
        for o in submitted {
            match items.iter().find(|i| i.op_id == o.op_id) {
                Some(item) => {
                    self.apply_status(&o, item.status, item.error.clone())
                        .await?
                }
                None => {
                    let _sending = self.send_lock().lock().await;
                    let Some(draft_id) = self.send_target(&o)? else {
                        continue;
                    };
                    let body = json!({ "opId": o.op_id, "draftId": draft_id });
                    match self
                        .client()
                        .post_json::<_, SendStatusDto>("api/send", &body)
                        .await
                    {
                        Ok(status) => self.apply_status(&o, status.status, None).await?,
                        Err(e) if halts(&e) => return Err(e.into()),
                        Err(e) => tracing::debug!(op = %o.op_id, error = %e, "send status unknown"),
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), 5);
        assert_eq!(backoff(30), RETRY_MAX_SECS);
    }
}
