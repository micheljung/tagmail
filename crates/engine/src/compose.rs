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
    Compose, ComposeError, ComposeResult, DraftInput, DraftOutcome, ForwardedPart,
    MAX_ATTACHMENTS_TOTAL, MAX_UPLOAD, NewUpload, SendOutcome,
};
use tagmail_api_core::compose_dto::{
    AttachmentDto, DraftDto, IdentityDto, OutboxItemDto, OutboxStateDto, SendStatusDto,
};
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::identities::{IdentityPatch, NewIdentity};
use tagmail_store::rusqlite::{self, Connection, OptionalExtension, Transaction, params};

use crate::identities::{identities, identity};
use crate::outgoing::{Outgoing, OutgoingFile, Purpose, new_message_id};
use crate::supervisor::{ConnectorFactory, Engine, Stores};

/// Unfinished uploads, under the upload directory.
pub const UPLOAD_TMP: &str = "tmp";
/// Messages being built, under the outbox directory.
pub const OUTBOX_TMP: &str = "tmp";
/// Calendar objects of calendar ops, under the outbox directory.
pub(crate) const CAL_OP_DIR: &str = "cal";
/// Separates a client op id from the suffix of an outbox item it queued,
/// as in `<opId>#reply`; client op ids never contain it.
pub(crate) const DERIVED_OP_SEPARATOR: char = '#';
/// `op_seen.kind` of mail ops and sends.
pub(crate) const MAIL_OP: &str = "mail";

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
    /// Message-ID of the forwarded message.
    #[serde(default)]
    pub forward_of: Option<String>,
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
    let forward_of = input
        .forward_of
        .clone()
        .or_else(|| previous.and_then(|p| p.forward_of.clone()));
    DraftContent {
        to: input.to.clone(),
        cc: input.cc.clone(),
        bcc: input.bcc.clone(),
        subject: input.subject.clone(),
        text: input.text.clone(),
        html: input.html.clone(),
        in_reply_to,
        references,
        forward_of,
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
) -> Outgoing {
    let attachments = files
        .iter()
        .map(|a| OutgoingFile {
            name: a.name.clone(),
            content_type: a.content_type.clone(),
            path: uploads.join(&a.id),
        })
        .collect();
    let c = &row.content;
    Outgoing {
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
    }
}

/// Writes `message` to a new file in `<outbox>/tmp`; returns its path and
/// size. Blocking.
pub(crate) fn write_message(
    outbox: &Path,
    message: &Outgoing,
    purpose: Purpose,
) -> std::io::Result<(PathBuf, u64)> {
    let file = tempfile::NamedTempFile::new_in(outbox.join(OUTBOX_TMP))?;
    let mut out = std::io::BufWriter::new(file);
    message.write_to(purpose, &mut out)?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.as_file().sync_all()?;
    let size = file.as_file().metadata()?.len();
    let (_, path) = file.keep().map_err(|e| e.error)?;
    Ok((path, size))
}

/// An outbox row; its message is the file `tmp`.
pub(crate) struct OutboxRow<'a> {
    pub op_id: &'a str,
    pub account: i64,
    pub identity_id: Option<i64>,
    pub size: u64,
    pub msgid: &'a str,
    pub sender: &'a str,
    /// JSON list of [`AddressDto`].
    pub recipients: &'a str,
    pub subject: &'a str,
    pub draft_id: Option<i64>,
    /// The Message-ID flagged once sent, and how.
    pub flag: Option<(&'a str, &'a str)>,
}

/// Queues `row` and moves its message file into the outbox; `None` when
/// the op id is already queued.
fn insert_outbox(
    tx: &Transaction<'_>,
    outbox: &Path,
    tmp: &Path,
    row: &OutboxRow<'_>,
) -> ComposeResult<Option<i64>> {
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO outbox (op_id, account_id, identity_id, mime_size, \
             msgid_hdr, sender, recipients, subject, draft_id, flag_msgid, flag_kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.op_id,
                row.account,
                row.identity_id,
                i64::try_from(row.size).unwrap_or(i64::MAX),
                row.msgid,
                row.sender,
                row.recipients,
                row.subject,
                row.draft_id,
                row.flag.map(|(msgid, _)| msgid),
                row.flag.map(|(_, kind)| kind),
            ],
        )
        .map_err(sql)?;
    if inserted == 0 {
        return Ok(None);
    }
    let id = tx.last_insert_rowid();
    let name = outbox_file(id);
    fs::rename(tmp, outbox.join(&name)).map_err(store_err)?;
    tx.execute(
        "UPDATE outbox SET mime_file = ?2 WHERE id = ?1",
        params![id, name],
    )
    .map_err(sql)?;
    Ok(Some(id))
}

/// Builds a generated message from the sender, its Message-ID (no angle
/// brackets) and Date (Unix seconds).
#[cfg(feature = "calendar")]
pub(crate) type BuildMime<'a> =
    &'a (dyn Fn(&AddressDto, &str, i64) -> std::io::Result<Vec<u8>> + Sync);

/// A message the engine wrote itself, such as an iTIP REPLY, waiting in
/// `<outbox>/tmp` for [`queue_generated`].
#[cfg(feature = "calendar")]
#[derive(Debug, Clone)]
pub(crate) struct GeneratedMessage {
    pub op_id: String,
    pub account: i64,
    pub identity_id: i64,
    pub sender: String,
    pub recipients: String,
    pub subject: String,
    pub msgid: String,
    pub tmp: PathBuf,
    pub size: u64,
}

#[cfg(feature = "calendar")]
impl GeneratedMessage {
    /// Removes the file of a message that was not queued.
    pub(crate) fn discard(&self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

/// Writes a generated message from identity `identity_id` (`None`: the
/// account's default) of `account` before it is queued, so the queueing
/// transaction does no file IO but the rename. An identity that no longer
/// exists is an error, never replaced by another one.
#[cfg(feature = "calendar")]
pub(crate) async fn write_generated(
    stores: &Stores,
    op_id: String,
    account: i64,
    identity_id: Option<i64>,
    to: &[AddressDto],
    subject: String,
    mime: BuildMime<'_>,
) -> ComposeResult<GeneratedMessage> {
    let all = read(stores, identities).await?;
    let identity = match identity_id {
        Some(id) => all
            .iter()
            .find(|x| x.id == id && x.account_id == account)
            .ok_or_else(|| invalid("identityId", "the identity no longer exists"))?,
        None => all
            .iter()
            .find(|x| x.account_id == account && x.is_default)
            .or_else(|| all.iter().find(|x| x.account_id == account))
            .ok_or_else(|| invalid("identityId", "the account has no identity"))?,
    };
    let from = AddressDto {
        name: identity.name.clone(),
        email: identity.address.clone(),
    };
    let msgid = new_message_id(&from.email);
    let bytes = mime(&from, &msgid, now()).map_err(store_err)?;
    let outbox = stores.outbox.clone();
    let size = bytes.len() as u64;
    let tmp = blocking(move || -> std::io::Result<PathBuf> {
        let mut file = tempfile::NamedTempFile::new_in(outbox.join(OUTBOX_TMP))?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.as_file().sync_all()?;
        Ok(file.keep().map_err(|e| e.error)?.1)
    })
    .await?
    .map_err(store_err)?;
    Ok(GeneratedMessage {
        op_id,
        account,
        identity_id: identity.id,
        sender: from.email,
        recipients: serde_json::to_string(to).expect("addresses serialize"),
        subject,
        msgid,
        tmp,
        size,
    })
}

/// Queues a generated message in the caller's transaction, under a
/// derived op id that gets no `op_seen` row (the caller's op has it).
/// `None` when the op id is already queued; the caller then
/// [`GeneratedMessage::discard`]s it. The Sent copy follows
/// `account.server_saves_sent` like any sent message.
#[cfg(feature = "calendar")]
pub(crate) fn queue_generated(
    tx: &Transaction<'_>,
    outbox: &Path,
    m: &GeneratedMessage,
) -> ComposeResult<Option<i64>> {
    insert_outbox(
        tx,
        outbox,
        &m.tmp,
        &OutboxRow {
            op_id: &m.op_id,
            account: m.account,
            identity_id: Some(m.identity_id),
            size: m.size,
            msgid: &m.msgid,
            sender: &m.sender,
            recipients: &m.recipients,
            subject: &m.subject,
            draft_id: None,
            flag: None,
        },
    )
}

/// File name of a queued message in the outbox directory.
pub(crate) fn outbox_file(id: i64) -> String {
    format!("{id}.eml")
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
    pub(crate) fn stores(&self) -> &Stores {
        &self.inner.stores
    }

    fn uploads(&self) -> PathBuf {
        self.inner.stores.uploads.clone()
    }

    async fn create_draft_impl(&self, input: DraftInput) -> ComposeResult<DraftOutcome> {
        if let Some(client_id) = input.client_id.clone() {
            let existing = read(self.stores(), move |c| {
                c.query_row(
                    "SELECT id FROM draft WHERE client_id = ?1",
                    [client_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
                .map_err(sql)
            })
            .await?;
            if let Some(id) = existing {
                return Ok(DraftOutcome {
                    draft: self.repeat_create(id, input).await?,
                    created: false,
                });
            }
        }
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
            let save_now = input.save_now || !input.attachment_ids.is_empty() || !staged.is_empty();
            tx.execute(
                "INSERT INTO draft (account_id, identity_id, msgid_hdr, content, created_at, \
                 updated_at, next_save_at, client_id) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
                params![
                    identity.account_id,
                    identity.id,
                    msgid,
                    json(&content),
                    now,
                    save_now.then_some(now),
                    input.client_id
                ],
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
        result.map(|draft| DraftOutcome {
            draft,
            created: true,
        })
    }

    /// A create repeating a `client_id` (its answer was lost): the same
    /// content leaves the draft as it is, so IMAP gets no second copy;
    /// anything else is an update. Forwarded parts were copied the first
    /// time and stay.
    async fn repeat_create(&self, id: i64, mut input: DraftInput) -> ComposeResult<DraftDto> {
        let forwarded = !input.forwarded.is_empty();
        input.forwarded.clear();
        let (row, current) = read(self.stores(), move |c| {
            let row = draft_row(c, id)?.ok_or(ComposeError::NotFound("draft"))?;
            let current: Vec<i64> = attachments(c, id)?
                .iter()
                .filter_map(|a| a.id.parse().ok())
                .collect();
            Ok((row, current))
        })
        .await?;
        if forwarded && input.attachment_ids.is_empty() {
            input.attachment_ids = current.clone();
        }
        let unchanged = row.identity_id == Some(input.identity_id)
            && current == input.attachment_ids
            && content_of(&input, Some(&row.content)) == row.content;
        if !unchanged {
            return self.update_draft_impl(id, input).await;
        }
        let save_now = input.save_now;
        let dto = write(self.stores(), move |tx| {
            if save_now {
                tx.execute(
                    "UPDATE draft SET next_save_at = ?2 \
                     WHERE id = ?1 AND saved_version < version AND queued = 0",
                    params![id, now()],
                )
                .map_err(sql)?;
            }
            draft_dto(tx, id)
        })
        .await?;
        self.inner.outbox_wake.notify_one();
        Ok(dto)
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
            let before: Vec<i64> = attachments(tx, id)?
                .iter()
                .filter_map(|a| a.id.parse().ok())
                .collect();
            link_uploads(tx, id, &input.attachment_ids, 0)?;
            let save_now = input.save_now || before != input.attachment_ids;
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
                 version = version + 1, updated_at = ?5, save_attempts = 0, next_save_at = ?6 \
                 WHERE id = ?1",
                params![
                    id,
                    identity.account_id,
                    identity.id,
                    json(&content),
                    now(),
                    save_now.then(now)
                ],
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
                        "UPDATE draft SET version = version + 1, updated_at = ?2, \
                         next_save_at = ?2 WHERE id = (SELECT draft_id FROM upload WHERE id = ?1)",
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
            let seen: Option<String> = c
                .query_row("SELECT kind FROM op_seen WHERE op_id = ?1", [&op_id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(sql)?;
            match seen.as_deref() {
                None => Ok(None),
                Some(MAIL_OP) => Ok(Some(SendStatusDto {
                    op_id,
                    status: OutboxStateDto::Sent,
                })),
                Some(_) => Err(ComposeError::Conflict(
                    "this op id was used by a calendar op".to_owned(),
                )),
            }
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
        let message = outgoing(
            &self.uploads(),
            build.from.clone(),
            &build.row,
            &build.files,
            now(),
        );
        let outbox = self.inner.stores.outbox.clone();
        let (tmp, size) = blocking({
            let (outbox, message) = (outbox.clone(), message.clone());
            move || write_message(&outbox, &message, Purpose::Send)
        })
        .await?
        .map_err(store_err)?;
        let built = tmp.clone();
        let (flag_msgid, flag_kind) = match (
            &build.row.content.forward_of,
            &build.row.content.in_reply_to,
        ) {
            (Some(original), _) => (Some(original.clone()), Some("forwarded")),
            (None, Some(parent)) => (Some(parent.clone()), Some("answered")),
            (None, None) => (None, None),
        };
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
            let queued = insert_outbox(
                tx,
                &outbox,
                &tmp,
                &OutboxRow {
                    op_id: &op_id,
                    account: row.account_id,
                    identity_id: row.identity_id,
                    size,
                    msgid: &row.msgid_hdr,
                    sender: &message.from.email,
                    recipients: &recipients,
                    subject: &message.subject,
                    draft_id: Some(draft_id),
                    flag: flag_msgid.as_deref().zip(flag_kind),
                },
            )?;
            let Some(id) = queued else {
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
            };
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
        .await;
        let _ = fs::remove_file(&built);
        let (id, outcome) = outcome?;
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

    pub(crate) async fn cancel_impl(&self, id: i64) -> ComposeResult<()> {
        let (file, linked) = write(self.stores(), move |tx| {
            let row: Option<(String, String, Option<i64>, Option<String>)> = tx
                .query_row(
                    "SELECT op_id, state, draft_id, mime_file FROM outbox WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((op_id, state, draft, file)) = row else {
                return Err(ComposeError::NotFound("outbox item"));
            };
            if !matches!(state.as_str(), "queued" | "failed") {
                return Err(ComposeError::Conflict(format!(
                    "the message is {state} and can no longer be cancelled"
                )));
            }
            tx.execute("DELETE FROM outbox WHERE id = ?1", [id])
                .map_err(sql)?;
            let linked = match op_id.split_once(DERIVED_OP_SEPARATOR) {
                None => {
                    tx.execute("DELETE FROM op_seen WHERE op_id = ?1", [&op_id])
                        .map_err(sql)?;
                    None
                }
                Some((parent, _)) => Some(parent.to_owned()),
            };
            if let Some(draft) = draft {
                tx.execute("UPDATE draft SET queued = 0 WHERE id = ?1", [draft])
                    .map_err(sql)?;
            }
            Ok((file, linked))
        })
        .await?;
        if let Some(path) = file.map(|name| self.inner.stores.outbox.join(name)) {
            let _ = fs::remove_file(path);
        }
        self.emit_outbox(id, None).await;
        #[cfg(feature = "calendar")]
        if let Some(parent) = linked {
            self.calendar_reply_cancelled(parent).await;
        }
        #[cfg(not(feature = "calendar"))]
        let _ = linked;
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
        Box::pin(async move { read(&stores, identities).await })
    }

    fn create_identity(&self, identity: NewIdentity) -> BoxFuture<ComposeResult<IdentityDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.create_identity_impl(identity).await })
    }

    fn update_identity(
        &self,
        id: i64,
        patch: IdentityPatch,
    ) -> BoxFuture<ComposeResult<IdentityDto>> {
        let engine = self.clone();
        Box::pin(async move { engine.update_identity_impl(id, patch).await })
    }

    fn delete_identity(&self, id: i64) -> BoxFuture<ComposeResult<()>> {
        let engine = self.clone();
        Box::pin(async move { engine.delete_identity_impl(id).await })
    }

    fn create_draft(&self, input: DraftInput) -> BoxFuture<ComposeResult<DraftOutcome>> {
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
