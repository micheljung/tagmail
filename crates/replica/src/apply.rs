//! Writes replica records into the local `cache.db` / `config.db` with the
//! server's ids, and derives what the server does not ship.

use std::collections::BTreeSet;
use std::io::Read;

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use tagmail_api_core::compose_dto::IdentityDto;
use tagmail_api_core::dto::{AccountDto, AddressDto};
use tagmail_api_core::replica_dto::{
    ImageSenderRecord, LabelRecord, LabelRegistryRecord, MailboxRecord, MessageRowRecord,
    TextRecord, ThreadRecord,
};
use tagmail_core::{AccountId, EmailId, MessageRowId};
use tagmail_store::fts::{self, FtsDocument};
use tagmail_store::{BlobKey, BlobStore, Error, Result, TEXT_CAP};

pub fn put_account(conn: &Connection, a: &AccountDto) -> Result<()> {
    conn.execute(
        "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, smtp_port, \
         smtp_tls, username, secret_ct, secret_nonce, max_connections, email, status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, x'', x'', ?10, ?11, ?12) \
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, imap_host = excluded.imap_host, \
         imap_port = excluded.imap_port, imap_tls = excluded.imap_tls, \
         smtp_host = excluded.smtp_host, smtp_port = excluded.smtp_port, \
         smtp_tls = excluded.smtp_tls, username = excluded.username, \
         max_connections = excluded.max_connections, email = excluded.email, \
         status = excluded.status",
        params![
            a.id,
            a.name,
            a.imap_host,
            a.imap_port,
            a.imap_tls.as_str(),
            a.smtp_host,
            a.smtp_port,
            a.smtp_tls.as_str(),
            a.username,
            a.max_connections,
            a.email,
            a.status.as_str(),
        ],
    )?;
    Ok(())
}

pub fn put_identity(conn: &Connection, i: &IdentityDto) -> Result<()> {
    if i.is_default {
        conn.execute(
            "UPDATE identity SET is_default = 0 WHERE account_id = ?1 AND id != ?2",
            params![i.account_id, i.id],
        )?;
    }
    conn.execute(
        "INSERT INTO identity (id, account_id, address, display_name, signature_html, is_default) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, \
         address = excluded.address, display_name = excluded.display_name, \
         signature_html = excluded.signature_html, is_default = excluded.is_default",
        params![
            i.id,
            i.account_id,
            i.address,
            i.name,
            i.signature,
            i.is_default
        ],
    )?;
    Ok(())
}

pub fn put_label_registry(conn: &Connection, r: &LabelRegistryRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO label_registry (account_id, label_key, display_name, color, sort) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (account_id, label_key) DO UPDATE SET display_name = excluded.display_name, \
         color = excluded.color, sort = excluded.sort",
        params![r.account_id, r.label_key, r.display_name, r.color, r.sort],
    )?;
    Ok(())
}

pub fn put_image_sender(conn: &Connection, r: &ImageSenderRecord) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO image_sender (address) VALUES (?1)",
        [&r.address],
    )?;
    Ok(())
}

pub fn delete_account_config(conn: &Connection, account: i64) -> Result<()> {
    conn.execute("DELETE FROM account WHERE id = ?1", [account])?;
    Ok(())
}

pub fn delete_identity(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM identity WHERE id = ?1", [id])?;
    Ok(())
}

pub fn put_mailbox(conn: &Connection, m: &MailboxRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO mailbox (id, account_id, name, delimiter, special_use, uidvalidity, \
         keyword_capable, sync_state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, name = excluded.name, \
         delimiter = excluded.delimiter, special_use = excluded.special_use, \
         uidvalidity = excluded.uidvalidity, keyword_capable = excluded.keyword_capable, \
         sync_state = excluded.sync_state",
        params![
            m.id,
            m.account_id,
            m.name,
            m.delimiter,
            m.special_use,
            m.uidvalidity,
            m.keyword_capable,
            m.sync_state
        ],
    )?;
    Ok(())
}

pub fn put_label(conn: &Connection, l: &LabelRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO label (id, account_id, keyword, label_key, kind, mailbox_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, \
         keyword = excluded.keyword, label_key = excluded.label_key, kind = excluded.kind, \
         mailbox_id = excluded.mailbox_id",
        params![
            l.id,
            l.account_id,
            l.keyword,
            l.label_key,
            l.kind,
            l.mailbox_id
        ],
    )?;
    Ok(())
}

pub fn put_thread(conn: &Connection, t: &ThreadRecord) -> Result<()> {
    if let Some(target) = t.merged_into {
        ensure_thread(conn, target, t.account_id)?;
    }
    conn.execute(
        "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, \
         starred, merged_into) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, \
         subject_norm = excluded.subject_norm, last_date = excluded.last_date, \
         msg_count = excluded.msg_count, unread_count = excluded.unread_count, \
         starred = excluded.starred, merged_into = excluded.merged_into",
        params![
            t.id,
            t.account_id,
            t.subject_norm,
            t.last_date,
            t.msg_count,
            t.unread_count,
            t.starred,
            t.merged_into
        ],
    )?;
    Ok(())
}

/// A placeholder row a reference can point at until its record arrives.
fn ensure_thread(conn: &Connection, id: i64, account: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO thread (id, account_id) VALUES (?1, ?2)",
        params![id, account],
    )?;
    Ok(())
}

/// Upserts a message with its locations and labels. Returns whether it was
/// new, and the thread it left, if it moved.
pub fn put_message(conn: &Connection, m: &MessageRowRecord) -> Result<PutMessage> {
    let before: Option<Option<i64>> = conn
        .query_row("SELECT thread_id FROM message WHERE id = ?1", [m.id], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(thread) = m.thread_id {
        ensure_thread(conn, thread, m.account_id)?;
    }
    conn.execute(
        "INSERT INTO message (id, account_id, email_id, msgid_hdr, in_reply_to, thread_id, date, \
         subject_norm, from_json, to_json, cc_json, subject, snippet, size, has_attachment, \
         tombstoned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
         ON CONFLICT (id) DO UPDATE SET account_id = excluded.account_id, \
         email_id = excluded.email_id, msgid_hdr = excluded.msgid_hdr, \
         in_reply_to = excluded.in_reply_to, thread_id = excluded.thread_id, \
         date = excluded.date, subject_norm = excluded.subject_norm, \
         from_json = excluded.from_json, to_json = excluded.to_json, cc_json = excluded.cc_json, \
         subject = excluded.subject, snippet = excluded.snippet, size = excluded.size, \
         has_attachment = excluded.has_attachment, tombstoned_at = excluded.tombstoned_at",
        params![
            m.id,
            m.account_id,
            m.email_id,
            m.msgid_hdr,
            m.in_reply_to,
            m.thread_id,
            m.date,
            m.subject_norm,
            m.from_json,
            m.to_json,
            m.cc_json,
            m.subject,
            m.snippet,
            m.size,
            m.has_attachment,
            m.tombstoned_at
        ],
    )?;
    conn.execute("DELETE FROM location WHERE message_id = ?1", [m.id])?;
    let mut loc = conn.prepare_cached(
        "INSERT OR REPLACE INTO location (mailbox_id, uid, message_id, flags, keywords, modseq, \
         srv_flags, srv_keywords, hidden) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for l in &m.locations {
        loc.execute(params![
            l.mailbox_id,
            l.uid,
            m.id,
            l.flags,
            l.keywords,
            l.modseq,
            l.srv_flags,
            l.srv_keywords,
            l.hidden
        ])?;
    }
    conn.execute("DELETE FROM message_label WHERE message_id = ?1", [m.id])?;
    let mut label = conn.prepare_cached(
        "INSERT OR IGNORE INTO message_label (message_id, label_id) \
         SELECT ?1, id FROM label WHERE id = ?2",
    )?;
    for id in &m.labels {
        label.execute(params![m.id, id])?;
    }
    if text_of(conn, m.id)?.is_none() {
        index(conn, m, "")?;
    }
    Ok(PutMessage {
        new: before.is_none(),
        left_thread: before.flatten().filter(|t| Some(*t) != m.thread_id),
    })
}

pub struct PutMessage {
    pub new: bool,
    pub left_thread: Option<i64>,
}

fn text_of(conn: &Connection, message: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT text_blob FROM message WHERE id = ?1",
            [message],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

fn index(conn: &Connection, m: &MessageRowRecord, body: &str) -> Result<()> {
    fts::index(
        conn,
        MessageRowId(m.id),
        &FtsDocument {
            subject: m.subject.as_deref().unwrap_or_default(),
            from: &address_text(&[&parse_addresses(m.from_json.as_deref())]),
            to: &address_text(&[
                &parse_addresses(m.to_json.as_deref()),
                &parse_addresses(m.cc_json.as_deref()),
            ]),
            body,
        },
    )
}

/// Re-indexes a stored message with `body`.
pub fn reindex(conn: &Connection, message: i64, body: &str) -> Result<()> {
    let row: Option<[Option<String>; 4]> = conn
        .query_row(
            "SELECT subject, from_json, to_json, cc_json FROM message WHERE id = ?1",
            [message],
            |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]),
        )
        .optional()?;
    let Some([subject, from, to, cc]) = row else {
        return Ok(());
    };
    fts::index(
        conn,
        MessageRowId(message),
        &FtsDocument {
            subject: subject.as_deref().unwrap_or_default(),
            from: &address_text(&[&parse_addresses(from.as_deref())]),
            to: &address_text(&[
                &parse_addresses(to.as_deref()),
                &parse_addresses(cc.as_deref()),
            ]),
            body,
        },
    )
}

fn parse_addresses(json: Option<&str>) -> Vec<AddressDto> {
    json.and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default()
}

/// Same text as the server's FTS columns, so both find the same messages.
fn address_text(lists: &[&[AddressDto]]) -> String {
    let mut out = String::new();
    for a in lists.iter().copied().flatten() {
        if !out.is_empty() {
            out.push(' ');
        }
        if let Some(name) = &a.name {
            out.push_str(name);
            out.push(' ');
        }
        out.push_str(&a.email);
    }
    out
}

/// Stores a message's text blob and indexes its body.
pub fn put_text(conn: &Connection, blobs: &BlobStore, t: &TextRecord) -> Result<()> {
    let Some(key) = blob_key(conn, t.message_id)? else {
        return Ok(());
    };
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(&t.zstd)
        .map_err(|e| Error::Corrupt(format!("text of message {}: {e}", t.message_id)))?;
    blobs.put_text_zstd(&key, &compressed)?;
    let mut text = String::new();
    zstd::Decoder::new(compressed.as_slice())?
        .take(TEXT_CAP as u64)
        .read_to_string(&mut text)?;
    conn.execute(
        "UPDATE message SET text_blob = ?2 WHERE id = ?1",
        params![t.message_id, key.as_str()],
    )?;
    reindex(conn, t.message_id, &text)
}

pub fn blob_key(conn: &Connection, message: i64) -> Result<Option<BlobKey>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT account_id, email_id FROM message WHERE id = ?1",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(account, email_id)| {
        Ok(BlobKey::for_message(
            AccountId(account),
            &EmailId::parse(&email_id)?,
        ))
    })
    .transpose()
}

/// Recomputes `thread_mailbox` of these threads from their messages'
/// visible locations, as the server derives it.
pub fn derive_threads(conn: &Connection, threads: &BTreeSet<i64>) -> Result<()> {
    let mut clear = conn.prepare_cached("DELETE FROM thread_mailbox WHERE thread_id = ?1")?;
    let mut fill = conn.prepare_cached(
        "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) \
         SELECT ?1, l.mailbox_id, max(m.date) FROM message m \
         JOIN location l ON l.message_id = m.id \
         WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL AND l.hidden = 0 \
         GROUP BY l.mailbox_id",
    )?;
    for thread in threads {
        clear.execute([thread])?;
        fill.execute([thread])?;
    }
    Ok(())
}

/// [`derive_threads`] for the whole replica after a bootstrap.
pub fn derive_all_threads(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM thread_mailbox;
         INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date)
         SELECT m.thread_id, l.mailbox_id, max(m.date) FROM message m
         JOIN location l ON l.message_id = m.id
         WHERE m.thread_id IS NOT NULL AND m.tombstoned_at IS NULL AND l.hidden = 0
         GROUP BY m.thread_id, l.mailbox_id;",
    )?;
    Ok(())
}

/// Deletes a message with its index row and blobs. Returns its thread.
pub fn delete_message(conn: &Connection, blobs: &BlobStore, id: i64) -> Result<Option<i64>> {
    let Some(key) = blob_key(conn, id)? else {
        return Ok(None);
    };
    let thread: Option<i64> =
        conn.query_row("SELECT thread_id FROM message WHERE id = ?1", [id], |r| {
            r.get(0)
        })?;
    fts::delete(conn, MessageRowId(id))?;
    conn.execute("DELETE FROM message WHERE id = ?1", [id])?;
    blobs.delete_text(&key)?;
    blobs.delete_raw(&key)?;
    Ok(thread)
}

pub fn delete_thread(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE thread SET merged_into = NULL WHERE merged_into = ?1",
        [id],
    )?;
    conn.execute(
        "UPDATE message SET thread_id = NULL WHERE thread_id = ?1",
        [id],
    )?;
    conn.execute("DELETE FROM thread WHERE id = ?1", [id])?;
    Ok(())
}

/// Points `source`'s messages at `target`. Returns both for derivation.
pub fn merge_thread(conn: &Connection, source: i64, target: i64) -> Result<[i64; 2]> {
    let account: Option<i64> = conn
        .query_row(
            "SELECT account_id FROM thread WHERE id = ?1",
            [source],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(account) = account {
        ensure_thread(conn, target, account)?;
        conn.execute(
            "UPDATE message SET thread_id = ?2 WHERE thread_id = ?1",
            params![source, target],
        )?;
        conn.execute(
            "UPDATE thread SET merged_into = ?2 WHERE id = ?1",
            params![source, target],
        )?;
    }
    Ok([source, target])
}

pub fn delete_mailbox(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM mailbox WHERE id = ?1", [id])?;
    Ok(())
}

pub fn delete_label(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM label WHERE id = ?1", [id])?;
    Ok(())
}

/// Removes an account's cache rows and blobs.
pub fn delete_account_cache(conn: &Connection, blobs: &BlobStore, account: i64) -> Result<()> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM message WHERE account_id = ?1")?;
        stmt.query_map([account], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in ids {
        delete_message(conn, blobs, id)?;
    }
    conn.execute(
        "UPDATE thread SET merged_into = NULL WHERE account_id = ?1",
        [account],
    )?;
    for table in ["thread", "label", "mailbox"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE account_id = ?1"),
            [account],
        )?;
    }
    Ok(())
}
