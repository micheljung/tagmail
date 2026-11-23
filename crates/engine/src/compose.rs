//! The engine's side of compose and send ([`Compose`]): identities, drafts
//! and their attachments in `config.db` and `<data>/uploads`, and queueing
//! into the outbox. Saving drafts to IMAP and sending happen in
//! [`crate::outbox`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tagmail_api_core::BoxFuture;
use tagmail_api_core::compose::{
    Compose, ComposeError, ComposeResult, DraftInput, ForwardedPart, MAX_ATTACHMENTS_TOTAL,
    MAX_UPLOAD, NewUpload, SendOutcome,
};
use tagmail_api_core::compose_dto::{
    AttachmentDto, DraftDto, IdentityDto, OutboxItemDto, OutboxStateDto, SendStatusDto,
};
use tagmail_api_core::dto::AddressDto;
use tagmail_store::rusqlite::{self, Connection, OptionalExtension, Transaction, params};

use crate::outgoing::{Outgoing, OutgoingFile, Purpose, new_message_id};
use crate::supervisor::{ConnectorFactory, Engine, Stores};

/// Unfinished uploads, under the upload directory.
pub const UPLOAD_TMP: &str = "tmp";

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The fields of `draft.content`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DraftContent {
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

pub(crate) fn upload_path(dir: &Path, id: i64) -> PathBuf {
    dir.join(id.to_string())
}

fn store_err(e: impl Into<tagmail_store::Error>) -> ComposeError {
    ComposeError::Internal(Box::new(e.into()))
}

fn invalid(field: &'static str, message: impl Into<String>) -> ComposeError {
    ComposeError::Invalid {
        field,
        message: message.into(),
    }
}

/// Runs `f` in one `config.db` transaction, rolled back when `f` fails.
pub(crate) async fn write<T, F>(stores: &Stores, f: F) -> ComposeResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>) -> ComposeResult<T> + Send + 'static,
{
    let failure = Arc::new(Mutex::new(None));
    let slot = failure.clone();
    let result = stores
        .config
        .write(move |tx| {
            f(tx).map_err(|e| {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                tagmail_store::Error::Corrupt("compose transaction failed".to_owned())
            })
        })
        .await;
    result.map_err(|e| {
        failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| store_err(e))
    })
}

pub(crate) async fn read<T, F>(stores: &Stores, f: F) -> ComposeResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> ComposeResult<T> + Send + 'static,
{
    stores
        .config
        .read(move |c| Ok(f(c)))
        .await
        .map_err(store_err)?
}

fn sql(e: rusqlite::Error) -> ComposeError {
    store_err(e)
}

/// Adds the default identity of every account that has none (M3: the
/// account's address) and returns all identities.
fn identities(tx: &Transaction<'_>) -> ComposeResult<Vec<IdentityDto>> {
    tx.execute(
        "INSERT INTO identity (account_id, address, is_default) \
         SELECT a.id, CASE WHEN instr(a.username, '@') > 0 THEN a.username \
                      ELSE a.username || '@' || a.imap_host END, 1 \
         FROM account a WHERE NOT EXISTS (SELECT 1 FROM identity i WHERE i.account_id = a.id)",
        [],
    )
    .map_err(sql)?;
    let mut stmt = tx
        .prepare(
            "SELECT id, account_id, address, display_name, signature_html, is_default \
             FROM identity ORDER BY account_id, is_default DESC, id",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(IdentityDto {
                id: r.get(0)?,
                account_id: r.get(1)?,
                address: r.get(2)?,
                name: r.get(3)?,
                signature: r.get(4)?,
                is_default: r.get(5)?,
            })
        })
        .map_err(sql)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql)?;
    Ok(rows)
}

fn identity(tx: &Transaction<'_>, id: i64) -> ComposeResult<IdentityDto> {
    identities(tx)?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| invalid("identityId", "no such identity"))
}

fn attachments(c: &Connection, draft: i64) -> ComposeResult<Vec<AttachmentDto>> {
    let mut stmt = c
        .prepare_cached(
            "SELECT id, name, size, content_type FROM upload WHERE draft_id = ?1 ORDER BY id",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([draft], |r| {
            Ok(AttachmentDto {
                id: r.get::<_, i64>(0)?.to_string(),
                name: r.get(1)?,
                size: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                content_type: r.get(3)?,
            })
        })
        .map_err(sql)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sql)?;
    Ok(rows)
}

pub(crate) struct DraftRow {
    pub id: i64,
    pub account_id: i64,
    pub identity_id: Option<i64>,
    pub msgid_hdr: String,
    pub content: DraftContent,
    pub version: i64,
    pub saved_version: i64,
    pub saved: Option<SavedAt>,
    pub save_error: Option<String>,
    pub queued: bool,
}

/// Where a draft version is stored in IMAP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedAt {
    pub mailbox: String,
    pub uidvalidity: u32,
    pub uid: u32,
}

pub(crate) fn draft_row(c: &Connection, id: i64) -> ComposeResult<Option<DraftRow>> {
    let row = c
        .query_row(
            "SELECT id, account_id, identity_id, msgid_hdr, content, version, saved_version, \
             saved_mailbox, saved_uidvalidity, saved_uid, save_error, queued \
             FROM draft WHERE id = ?1",
            [id],
            |r| {
                let mailbox: Option<String> = r.get(7)?;
                Ok((
                    DraftRow {
                        id: r.get(0)?,
                        account_id: r.get(1)?,
                        identity_id: r.get(2)?,
                        msgid_hdr: r.get(3)?,
                        content: DraftContent::default(),
                        version: r.get(5)?,
                        saved_version: r.get(6)?,
                        saved: match mailbox {
                            Some(mailbox) => Some(SavedAt {
                                mailbox,
                                uidvalidity: r.get(8)?,
                                uid: r.get(9)?,
                            }),
                            None => None,
                        },
                        save_error: r.get(10)?,
                        queued: r.get(11)?,
                    },
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql)?;
    row.map(|(mut row, content)| {
        row.content = serde_json::from_str(&content)
            .map_err(|e| store_err(tagmail_store::Error::Corrupt(format!("draft {id}: {e}"))))?;
        Ok(row)
    })
    .transpose()
}

fn draft_dto(c: &Connection, id: i64) -> ComposeResult<DraftDto> {
    let row = draft_row(c, id)?.ok_or(ComposeError::NotFound("draft"))?;
    let files = attachments(c, id)?;
    Ok(DraftDto {
        id: row.id,
        account_id: row.account_id,
        identity_id: row.identity_id,
        to: row.content.to,
        cc: row.content.cc,
        bcc: row.content.bcc,
        subject: row.content.subject,
        text: row.content.text,
        html: row.content.html,
        in_reply_to_message_id: row.content.in_reply_to,
        attachment_ids: files.iter().map(|a| a.id.clone()).collect(),
        attachments: files,
        version: row.version,
        saved: row.saved_version == row.version && row.saved.is_some(),
        save_error: row.save_error,
        queued: row.queued,
    })
}

/// Queues the expunge of a saved draft copy.
pub(crate) fn purge_later(tx: &Transaction<'_>, account: i64, at: &SavedAt) -> ComposeResult<()> {
    tx.execute(
        "INSERT INTO draft_purge (account_id, mailbox, uidvalidity, uid) VALUES (?1, ?2, ?3, ?4)",
        params![account, at.mailbox, at.uidvalidity, at.uid],
    )
    .map_err(sql)?;
    Ok(())
}

/// Points exactly `ids` (and no others) at `draft`; ids must be orphans or
/// already the draft's, and all of them together within the size cap.
fn link_uploads(tx: &Transaction<'_>, draft: i64, ids: &[i64], extra: u64) -> ComposeResult<()> {
    let mut total = extra;
    for &id in ids {
        let row: Option<(Option<i64>, i64)> = tx
            .query_row(
                "SELECT draft_id, size FROM upload WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql)?;
        match row {
            Some((owner, size)) if owner.is_none_or(|o| o == draft) => {
                total += u64::try_from(size).unwrap_or(0);
            }
            _ => return Err(invalid("attachmentIds", format!("no such attachment {id}"))),
        }
    }
    if total > MAX_ATTACHMENTS_TOTAL {
        return Err(ComposeError::TooLarge(format!(
            "attachments total {total} bytes; at most {MAX_ATTACHMENTS_TOTAL}"
        )));
    }
    tx.execute(
        "UPDATE upload SET draft_id = NULL WHERE draft_id = ?1",
        [draft],
    )
    .map_err(sql)?;
    for &id in ids {
        tx.execute("UPDATE upload SET draft_id = ?1 WHERE id = ?2", [draft, id])
            .map_err(sql)?;
    }
    Ok(())
}

fn content_of(input: &DraftInput, previous: Option<&DraftContent>) -> DraftContent {
    let (in_reply_to, references) = match (&input.reply, previous) {
        (Some(r), _) => (Some(r.in_reply_to.clone()), r.references.clone()),
        (None, Some(p)) => (p.in_reply_to.clone(), p.references.clone()),
        (None, None) => (None, Vec::new()),
    };
    DraftContent {
        to: input.to.clone(),
        cc: input.cc.clone(),
        bcc: input.bcc.clone(),
        subject: input.subject.clone(),
        text: input.text.clone(),
        html: input.html.clone(),
        in_reply_to,
        references,
    }
}

fn json(content: &DraftContent) -> String {
    serde_json::to_string(content).expect("draft content serializes")
}

/// The message of a draft with its attachments read from disk. Blocking.
pub(crate) fn outgoing(
    uploads: &Path,
    from: AddressDto,
    row: &DraftRow,
    files: &[AttachmentDto],
    date: i64,
) -> std::io::Result<Outgoing> {
    let attachments = files
        .iter()
        .map(|a| {
            Ok(OutgoingFile {
                name: a.name.clone(),
                content_type: a.content_type.clone(),
                bytes: fs::read(uploads.join(&a.id))?,
            })
        })
        .collect::<std::io::Result<_>>()?;
    let c = &row.content;
    Ok(Outgoing {
        from,
        to: c.to.clone(),
        cc: c.cc.clone(),
        bcc: c.bcc.clone(),
        subject: c.subject.clone(),
        text: c.text.clone(),
        html: c.html.clone(),
        message_id: row.msgid_hdr.clone(),
        in_reply_to: c.in_reply_to.clone(),
        references: c.references.clone(),
        date,
        attachments,
    })
}

/// A draft with what building its message needs.
pub(crate) struct DraftBuild {
    pub row: DraftRow,
    pub from: AddressDto,
    pub files: Vec<AttachmentDto>,
}

pub(crate) fn draft_build(tx: &Transaction<'_>, id: i64) -> ComposeResult<Option<DraftBuild>> {
    let Some(row) = draft_row(tx, id)? else {
        return Ok(None);
    };
    let all = identities(tx)?;
    let identity = row
        .identity_id
        .and_then(|i| all.iter().find(|x| x.id == i))
        .or_else(|| {
            all.iter()
                .find(|x| x.account_id == row.account_id && x.is_default)
        })
        .ok_or_else(|| invalid("identityId", "the draft's account has no identity"))?;
    let from = AddressDto {
        name: identity.name.clone(),
        email: identity.address.clone(),
    };
    let files = attachments(tx, id)?;
    Ok(Some(DraftBuild { row, from, files }))
}

pub(crate) fn state_of(s: &str) -> OutboxStateDto {
    match s {
        "queued" => OutboxStateDto::Queued,
        "sending" => OutboxStateDto::Sending,
        "sent" => OutboxStateDto::Sent,
        _ => OutboxStateDto::Failed,
    }
}

pub(crate) fn remove_upload_files(dir: &Path, ids: &[i64]) {
    for &id in ids {
        if let Err(e) = fs::remove_file(upload_path(dir, id))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(upload = id, error = %e, "cannot remove an upload");
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ComposeResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ComposeError::Internal(Box::new(e)))
}

/// Writes forwarded parts to temp files in the upload directory.
fn stage(dir: &Path, parts: Vec<(String, String, Vec<u8>)>) -> std::io::Result<Vec<Staged>> {
    parts
        .into_iter()
        .map(|(name, content_type, bytes)| {
            let mut file = tempfile::NamedTempFile::new_in(dir.join(UPLOAD_TMP))?;
            std::io::Write::write_all(&mut file, &bytes)?;
            let (_, path) = file.keep().map_err(|e| e.error)?;
            Ok(Staged {
                name,
                content_type,
                size: bytes.len() as u64,
                path,
            })
        })
        .collect()
}

struct Staged {
    name: String,
    content_type: String,
    size: u64,
    path: PathBuf,
}

/// Inserts the upload row and moves its file into place.
fn adopt(
    tx: &Transaction<'_>,
    dir: &Path,
    draft: Option<i64>,
    name: &str,
    content_type: &str,
    size: u64,
    file: &Path,
) -> ComposeResult<i64> {
    tx.execute(
        "INSERT INTO upload (draft_id, name, content_type, size) VALUES (?1, ?2, ?3, ?4)",
        params![
            draft,
            name,
            content_type,
            i64::try_from(size).unwrap_or(i64::MAX)
        ],
    )
    .map_err(sql)?;
    let id = tx.last_insert_rowid();
    fs::rename(file, upload_path(dir, id)).map_err(store_err)?;
    Ok(id)
}

impl<F: ConnectorFactory> Engine<F> {
    fn stores(&self) -> &Stores {
        &self.inner.stores
    }

    fn uploads(&self) -> PathBuf {
        self.inner.stores.uploads.clone()
    }

    async fn create_draft_impl(&self, input: DraftInput) -> ComposeResult<DraftDto> {
        let dir = self.uploads();
        let parts = input
            .forwarded
            .iter()
            .map(|p| (p.name.clone(), p.content_type.clone(), p.bytes.clone()))
            .collect();
        let staged = {
            let dir = dir.clone();
            blocking(move || stage(&dir, parts))
                .await?
                .map_err(store_err)?
        };
        let cleanup: Vec<PathBuf> = staged.iter().map(|s| s.path.clone()).collect();
        let result = write(self.stores(), move |tx| {
            let identity = identity(tx, input.identity_id)?;
            let extra: u64 = staged.iter().map(|s| s.size).sum();
            let content = content_of(&input, None);
            let msgid = new_message_id(&identity.address);
            let now = now();
            tx.execute(
                "INSERT INTO draft (account_id, identity_id, msgid_hdr, content, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![identity.account_id, identity.id, msgid, json(&content), now],
            )
            .map_err(sql)?;
            let id = tx.last_insert_rowid();
            link_uploads(tx, id, &input.attachment_ids, extra)?;
            for s in &staged {
                adopt(
                    tx,
                    &dir,
                    Some(id),
                    &s.name,
                    &s.content_type,
                    s.size,
                    &s.path,
                )?;
            }
            draft_dto(tx, id)
        })
        .await;
        for path in cleanup {
            let _ = fs::remove_file(path);
        }
        self.inner.outbox_wake.notify_one();
        result
    }

    async fn update_draft_impl(&self, id: i64, input: DraftInput) -> ComposeResult<DraftDto> {
        let result = write(self.stores(), move |tx| {
            let row = draft_row(tx, id)?.ok_or(ComposeError::NotFound("draft"))?;
            if row.queued {
                return Err(ComposeError::Conflict(
                    "the draft is being sent; cancel it in the outbox first".to_owned(),
                ));
            }
            let identity = identity(tx, input.identity_id)?;
            link_uploads(tx, id, &input.attachment_ids, 0)?;
            if identity.account_id != row.account_id
                && let Some(saved) = &row.saved
            {
                purge_later(tx, row.account_id, saved)?;
                tx.execute(
                    "UPDATE draft SET saved_mailbox = NULL, saved_uidvalidity = NULL, \
                     saved_uid = NULL WHERE id = ?1",
                    [id],
                )
                .map_err(sql)?;
            }
            let content = content_of(&input, Some(&row.content));
            tx.execute(
                "UPDATE draft SET account_id = ?2, identity_id = ?3, content = ?4, \
                 version = version + 1, updated_at = ?5, save_attempts = 0, next_save_at = NULL \
                 WHERE id = ?1",
                params![id, identity.account_id, identity.id, json(&content), now()],
            )
            .map_err(sql)?;
            draft_dto(tx, id)
        })
        .await;
        self.inner.outbox_wake.notify_one();
        result
    }

    async fn delete_draft_impl(&self, id: i64) -> ComposeResult<()> {
        let removed = write(self.stores(), move |tx| {
            let row = draft_row(tx, id)?.ok_or(ComposeError::NotFound("draft"))?;
            if row.queued {
                return Err(ComposeError::Conflict(
                    "the draft is being sent; cancel it in the outbox first".to_owned(),
                ));
            }
            delete_draft_row(tx, &row)
        })
        .await?;
        remove_upload_files(&self.uploads(), &removed);
        self.inner.outbox_wake.notify_one();
        Ok(())
    }

    async fn add_upload_impl(&self, upload: NewUpload) -> ComposeResult<AttachmentDto> {
        if upload.size > MAX_UPLOAD {
            let _ = fs::remove_file(&upload.file);
            return Err(ComposeError::TooLarge(format!(
                "attachments are at most {MAX_UPLOAD} bytes"
            )));
        }
        let dir = self.uploads();
        let file = upload.file.clone();
        let result = write(self.stores(), move |tx| {
            let id = adopt(
                tx,
                &dir,
                None,
                &upload.name,
                &upload.content_type,
                upload.size,
                &upload.file,
            )?;
            Ok(AttachmentDto {
                id: id.to_string(),
                name: upload.name,
                size: upload.size,
                content_type: upload.content_type,
            })
        })
        .await;
        if result.is_err() {
            let _ = fs::remove_file(file);
        }
        result
    }

    async fn delete_upload_impl(&self, id: i64) -> ComposeResult<()> {
        write(self.stores(), move |tx| {
            let queued: Option<bool> = tx
                .query_row(
                    "SELECT coalesce(d.queued, 0) FROM upload u \
                     LEFT JOIN draft d ON d.id = u.draft_id WHERE u.id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            match queued {
                None => Err(ComposeError::NotFound("attachment")),
                Some(true) => Err(ComposeError::Conflict(
                    "the attachment's draft is being sent".to_owned(),
                )),
                Some(false) => {
                    tx.execute(
                        "UPDATE draft SET version = version + 1, updated_at = ?2 \
                                WHERE id = (SELECT draft_id FROM upload WHERE id = ?1)",
                        params![id, now()],
                    )
                    .map_err(sql)?;
                    tx.execute("DELETE FROM upload WHERE id = ?1", [id])
                        .map_err(sql)?;
                    Ok(())
                }
            }
        })
        .await?;
        remove_upload_files(&self.uploads(), &[id]);
        self.inner.outbox_wake.notify_one();
        Ok(())
    }

    async fn existing_send(&self, op_id: String) -> ComposeResult<Option<SendStatusDto>> {
        read(self.stores(), move |c| {
            let state: Option<String> = c
                .query_row("SELECT state FROM outbox WHERE op_id = ?1", [&op_id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(sql)?;
            if let Some(state) = state {
                return Ok(Some(SendStatusDto {
                    op_id,
                    status: state_of(&state),
                }));
            }
            let seen = c
                .query_row("SELECT 1 FROM op_seen WHERE op_id = ?1", [&op_id], |_| {
                    Ok(())
                })
                .optional()
                .map_err(sql)?;
            Ok(seen.map(|()| SendStatusDto {
                op_id,
                status: OutboxStateDto::Sent,
            }))
        })
        .await
    }

    async fn send_impl(&self, op_id: String, draft_id: i64) -> ComposeResult<SendOutcome> {
        if let Some(status) = self.existing_send(op_id.clone()).await? {
            return Ok(SendOutcome {
                status,
                created: false,
            });
        }
        let build = write(self.stores(), move |tx| {
            let build = draft_build(tx, draft_id)?.ok_or(ComposeError::NotFound("draft"))?;
            if build.row.queued {
                return Err(ComposeError::Conflict(
                    "the draft is already queued".to_owned(),
                ));
            }
            Ok(build)
        })
        .await?;
        if build.row.content.to.is_empty()
            && build.row.content.cc.is_empty()
            && build.row.content.bcc.is_empty()
        {
            return Err(invalid("to", "the draft has no recipients"));
        }
        let dir = self.uploads();
        let (build, message) = blocking(move || {
            let message = outgoing(&dir, build.from.clone(), &build.row, &build.files, now())?;
            Ok::<_, std::io::Error>((build, message))
        })
        .await?
        .map_err(store_err)?;
        let mime = blocking({
            let message = message.clone();
            move || message.build(Purpose::Send)
        })
        .await?;
        let recipients =
            serde_json::to_string(&message.recipient_list()).expect("addresses serialize");
        let version = build.row.version;
        let outcome = write(self.stores(), move |tx| {
            let row = draft_row(tx, draft_id)?.ok_or(ComposeError::NotFound("draft"))?;
            if row.queued || row.version != version {
                return Err(ComposeError::Conflict(
                    "the draft changed while it was queued; send again".to_owned(),
                ));
            }
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO outbox (op_id, account_id, identity_id, mime_blob, \
                     msgid_hdr, sender, recipients, subject, draft_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        op_id,
                        row.account_id,
                        row.identity_id,
                        mime,
                        row.msgid_hdr,
                        message.from.email,
                        recipients,
                        message.subject,
                        draft_id
                    ],
                )
                .map_err(sql)?;
            if inserted == 0 {
                let state: String = tx
                    .query_row("SELECT state FROM outbox WHERE op_id = ?1", [&op_id], |r| {
                        r.get(0)
                    })
                    .map_err(sql)?;
                return Ok((
                    None,
                    SendOutcome {
                        status: SendStatusDto {
                            op_id,
                            status: state_of(&state),
                        },
                        created: false,
                    },
                ));
            }
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT OR IGNORE INTO op_seen (op_id, account_id) VALUES (?1, ?2)",
                params![op_id, row.account_id],
            )
            .map_err(sql)?;
            tx.execute("UPDATE draft SET queued = 1 WHERE id = ?1", [draft_id])
                .map_err(sql)?;
            Ok((
                Some(id),
                SendOutcome {
                    status: SendStatusDto {
                        op_id,
                        status: OutboxStateDto::Queued,
                    },
                    created: true,
                },
            ))
        })
        .await?;
        let (id, outcome) = outcome;
        if let Some(id) = id {
            self.emit_outbox(id, Some((OutboxStateDto::Queued, None)))
                .await;
        }
        self.inner.outbox_wake.notify_one();
        Ok(outcome)
    }

    async fn outbox_impl(&self) -> ComposeResult<Vec<OutboxItemDto>> {
        read(self.stores(), |c| {
            let mut stmt = c
                .prepare(
                    "SELECT op_id, account_id, draft_id, subject, recipients, state, attempts, \
                     error, created_at, sent_at, id FROM outbox ORDER BY id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    let recipients: String = r.get(4)?;
                    let state: String = r.get(5)?;
                    Ok(OutboxItemDto {
                        id: r.get(10)?,
                        op_id: r.get(0)?,
                        account_id: r.get(1)?,
                        draft_id: r.get(2)?,
                        subject: r.get(3)?,
                        to: serde_json::from_str(&recipients).unwrap_or_default(),
                        status: state_of(&state),
                        attempts: r.get(6)?,
                        error: r.get(7)?,
                        created_at: r.get(8)?,
                        sent_at: r.get(9)?,
                    })
                })
                .map_err(sql)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql)?;
            Ok(rows)
        })
        .await
    }

    async fn cancel_impl(&self, id: i64) -> ComposeResult<()> {
        write(self.stores(), move |tx| {
            let row: Option<(String, String, Option<i64>)> = tx
                .query_row(
                    "SELECT op_id, state, draft_id FROM outbox WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((op_id, state, draft)) = row else {
                return Err(ComposeError::NotFound("outbox item"));
            };
            if !matches!(state.as_str(), "queued" | "failed") {
                return Err(ComposeError::Conflict(format!(
                    "the message is {state} and can no longer be cancelled"
                )));
            }
            tx.execute("DELETE FROM outbox WHERE id = ?1", [id])
                .map_err(sql)?;
            tx.execute("DELETE FROM op_seen WHERE op_id = ?1", [&op_id])
                .map_err(sql)?;
            if let Some(draft) = draft {
                tx.execute("UPDATE draft SET queued = 0 WHERE id = ?1", [draft])
                    .map_err(sql)?;
            }
            Ok(())
        })
        .await?;
        self.emit_outbox(id, None).await;
        Ok(())
    }

    async fn retry_impl(&self, id: i64) -> ComposeResult<()> {
        write(self.stores(), move |tx| {
            let state: Option<String> = tx
                .query_row("SELECT state FROM outbox WHERE id = ?1", [id], |r| r.get(0))
                .optional()
                .map_err(sql)?;
            match state.as_deref() {
                None => Err(ComposeError::NotFound("outbox item")),
                Some("failed") => {
                    tx.execute(
                        "UPDATE outbox SET state = 'queued', attempts = 0, next_at = NULL, \
                         error = NULL WHERE id = ?1",
                        [id],
                    )
                    .map_err(sql)?;
                    Ok(())
                }
                Some(other) => Err(ComposeError::Conflict(format!(
                    "only failed messages can be retried; this one is {other}"
                ))),
            }
        })
        .await?;
        self.emit_outbox(id, Some((OutboxStateDto::Queued, None)))
            .await;
        self.inner.outbox_wake.notify_one();
        Ok(())
    }

    async fn add_parts_impl(&self, parts: Vec<ForwardedPart>) -> ComposeResult<Vec<AttachmentDto>> {
        let dir = self.uploads();
        let parts = parts
            .into_iter()
            .map(|p| (p.name, p.content_type, p.bytes))
            .collect();
        let staged = {
            let dir = dir.clone();
            blocking(move || stage(&dir, parts))
                .await?
                .map_err(store_err)?
        };
        let cleanup: Vec<PathBuf> = staged.iter().map(|s| s.path.clone()).collect();
        let result = write(self.stores(), move |tx| {
            staged
                .iter()
                .map(|s| {
                    let id = adopt(tx, &dir, None, &s.name, &s.content_type, s.size, &s.path)?;
                    Ok(AttachmentDto {
                        id: id.to_string(),
                        name: s.name.clone(),
                        size: s.size,
                        content_type: s.content_type.clone(),
                    })
                })
                .collect()
        })
        .await;
        for path in cleanup {
            let _ = fs::remove_file(path);
        }
        result
    }
}

/// Deletes a draft, queues the expunge of its IMAP copy and drops its
/// uploads; returns the upload ids whose files to remove after commit.
pub(crate) fn delete_draft_row(tx: &Transaction<'_>, row: &DraftRow) -> ComposeResult<Vec<i64>> {
    if let Some(saved) = &row.saved {
        purge_later(tx, row.account_id, saved)?;
    }
    let ids: Vec<i64> = {
        let mut stmt = tx
            .prepare("SELECT id FROM upload WHERE draft_id = ?1")
            .map_err(sql)?;
        stmt.query_map([row.id], |r| r.get(0))
            .map_err(sql)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql)?
    };
    tx.execute("DELETE FROM upload WHERE draft_id = ?1", [row.id])
        .map_err(sql)?;
    tx.execute("DELETE FROM draft WHERE id = ?1", [row.id])
        .map_err(sql)?;
    Ok(ids)
}

impl<F: ConnectorFactory> Compose for Engine<F> {
    fn identities(&self) -> BoxFuture<ComposeResult<Vec<IdentityDto>>> {
        let stores = self.stores().clone();
        Box::pin(async move { write(&stores, identities).await })
    }

    fn create_draft(&self, input: DraftInput) -> BoxFuture<ComposeResult<DraftDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.create_draft_impl(input).await })
    }

    fn update_draft(&self, id: i64, input: DraftInput) -> BoxFuture<ComposeResult<DraftDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.update_draft_impl(id, input).await })
    }

    fn draft(&self, id: i64) -> BoxFuture<ComposeResult<DraftDto>> {
        let stores = self.stores().clone();
        Box::pin(async move { read(&stores, move |c| draft_dto(c, id)).await })
    }

    fn delete_draft(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.delete_draft_impl(id).await })
    }

    fn upload_dir(&self) -> PathBuf {
        self.uploads().join(UPLOAD_TMP)
    }

    fn add_upload(&self, upload: NewUpload) -> BoxFuture<ComposeResult<AttachmentDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.add_upload_impl(upload).await })
    }

    fn delete_upload(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.delete_upload_impl(id).await })
    }

    fn send(&self, op_id: String, draft_id: i64) -> BoxFuture<ComposeResult<SendOutcome>> {
        let engine = self.clone();
        Box::pin(async move { engine.send_impl(op_id, draft_id).await })
    }

    fn outbox(&self) -> BoxFuture<ComposeResult<Vec<OutboxItemDto>>> {
        let engine = self.clone();
        Box::pin(async move { engine.outbox_impl().await })
    }

    fn cancel(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.cancel_impl(id).await })
    }

    fn retry(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.retry_impl(id).await })
    }

    fn add_parts(&self, parts: Vec<ForwardedPart>) -> BoxFuture<ComposeResult<Vec<AttachmentDto>>> {
        let engine = self.clone();
        Box::pin(async move { engine.add_parts_impl(parts).await })
    }
}
