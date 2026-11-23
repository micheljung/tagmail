//! The local overlay: open intents projected over the replica's server
//! state, the way the server projects its own pending ops, so a write is
//! visible before it reached the server.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use tagmail_core::ops::{self, ArchiveMode, MailboxView, Overlay, Role};
use tagmail_core::{EmailId, Flags, KeywordClass, KeywordSet, SpecialUse, classify_keyword};
use tagmail_store::Result;
use tagmail_store::changes::{ChangeOp, Entity, NewChange};

/// `(id, name, role, is_archive)` of the account's mailboxes.
type Views = Vec<(i64, String, Role, bool)>;

fn views(conn: &Connection, account: i64) -> Result<(Views, ArchiveMode)> {
    let rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt =
            conn.prepare_cached("SELECT id, name, special_use FROM mailbox WHERE account_id = ?1")?;
        stmt.query_map([account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let uses: Vec<(String, Option<SpecialUse>)> = rows
        .iter()
        .map(|(_, n, s)| (n.clone(), s.as_deref().and_then(|s| s.parse().ok())))
        .collect();
    let has_all = uses.iter().any(|(_, u)| *u == Some(SpecialUse::All));
    let mode = ArchiveMode::detect(None, has_all);
    let archive =
        ops::archive_mailbox(mode, uses.iter().map(|(n, u)| (n.as_str(), *u))).map(str::to_owned);
    let views = rows
        .into_iter()
        .zip(uses)
        .map(|((id, name, _), (_, special))| {
            let is_archive = archive.as_deref() == Some(name.as_str());
            let role = Role::of(&name, special);
            (id, name, role, is_archive)
        })
        .collect();
    Ok((views, mode))
}

/// `(mailbox_id, uid, flags, keywords, srv_flags, srv_keywords, hidden)`.
type LocationRow = (i64, i64, u8, String, Option<u8>, Option<String>, bool);

/// Re-projects `overlay` (one account's open intents) over the locations
/// of `messages`, then refreshes their labels and threads. Returns the
/// local changes to log.
pub fn reproject(
    conn: &Connection,
    account: i64,
    overlay: &Overlay,
    messages: &BTreeSet<i64>,
) -> Result<Vec<NewChange>> {
    let (views, mode) = views(conn, account)?;
    let mut threads = BTreeSet::new();
    let mut log = Vec::new();
    for &message in messages {
        let row: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT email_id, thread_id FROM message WHERE id = ?1 AND account_id = ?2",
                params![message, account],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((email_id, thread)) = row else {
            continue;
        };
        let email_id = EmailId::parse(&email_id)?;
        let rows: Vec<LocationRow> = {
            let mut stmt = conn.prepare_cached(
                "SELECT mailbox_id, uid, flags, keywords, srv_flags, srv_keywords, hidden \
                 FROM location WHERE message_id = ?1",
            )?;
            stmt.query_map([message], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        for (mailbox, uid, flags, keywords, srv_flags, srv_keywords, hidden) in rows {
            let base_flags = Flags::from_bits(srv_flags.unwrap_or(flags));
            let base_keywords =
                KeywordSet::from_storage(srv_keywords.as_deref().unwrap_or(&keywords))?;
            // The server's own hidden state is only known before the first
            // local projection over a location.
            let base_hidden = srv_flags.is_none() && hidden;
            let view = views
                .iter()
                .find(|v| v.0 == mailbox)
                .map(|(_, name, role, is_archive)| MailboxView {
                    name,
                    role: *role,
                    archive_mode: mode,
                    is_archive: *is_archive,
                });
            let (f, k, sf, sk, h) = match view.filter(|_| overlay.touches(&email_id)) {
                Some(view) => {
                    let p = ops::project(
                        overlay.ops_for(&email_id),
                        &view,
                        base_flags,
                        base_keywords.clone(),
                    );
                    (
                        p.flags.bits(),
                        p.keywords.to_storage(),
                        Some(base_flags.bits()),
                        Some(base_keywords.to_storage()),
                        p.hidden || base_hidden,
                    )
                }
                None => (
                    base_flags.bits(),
                    base_keywords.to_storage(),
                    None,
                    None,
                    base_hidden,
                ),
            };
            conn.execute(
                "UPDATE location SET flags = ?3, keywords = ?4, srv_flags = ?5, srv_keywords = ?6, \
                 hidden = ?7 WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox, uid, f, k, sf, sk, h],
            )?;
        }
        derive_labels(conn, account, message)?;
        log.push(NewChange {
            entity: Entity::Message,
            entity_id: message,
            op: ChangeOp::Upsert,
            payload: Some(json!({ "threadId": thread }).to_string()),
        });
        threads.extend(thread);
    }
    for thread in threads {
        if let Some(change) = refresh_thread(conn, account, overlay, &views, mode, thread)? {
            log.push(change);
        }
    }
    Ok(log)
}

/// `message_label` = the account's keyword labels over the union of the
/// message's keywords.
fn derive_labels(conn: &Connection, account: i64, message: i64) -> Result<()> {
    let mut keys = BTreeSet::new();
    {
        let mut stmt =
            conn.prepare_cached("SELECT keywords FROM location WHERE message_id = ?1")?;
        for kw in stmt.query_map([message], |r| r.get::<_, String>(0))? {
            for k in KeywordSet::from_storage(&kw?)?.iter() {
                if let KeywordClass::Label { key, .. } = classify_keyword(k) {
                    keys.insert(key.as_str().to_owned());
                }
            }
        }
    }
    conn.execute(
        "DELETE FROM message_label WHERE message_id = ?1 AND label_id IN \
         (SELECT id FROM label WHERE kind = 'keyword')",
        [message],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO message_label (message_id, label_id) \
         SELECT ?1, id FROM label WHERE account_id = ?2 AND label_key = ?3 AND kind = 'keyword'",
    )?;
    for key in keys {
        insert.execute(params![message, account, key])?;
    }
    Ok(())
}

/// Counts and `thread_mailbox` from the visible locations, plus the
/// mailboxes open moves put messages into; the change as the server logs it.
fn refresh_thread(
    conn: &Connection,
    account: i64,
    overlay: &Overlay,
    views: &Views,
    mode: ArchiveMode,
    thread: i64,
) -> Result<Option<NewChange>> {
    let merged: Option<Option<i64>> = conn
        .query_row(
            "SELECT merged_into FROM thread WHERE id = ?1",
            [thread],
            |r| r.get(0),
        )
        .optional()?;
    if !matches!(merged, Some(None)) {
        return Ok(None);
    }
    let before = thread_mailboxes(conn, thread)?;
    conn.execute(
        "UPDATE thread SET \
           msg_count = (SELECT count(*) FROM message m WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL), \
           unread_count = (SELECT count(*) FROM message m WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL \
             AND EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & ?2) = 0)), \
           starred = (SELECT count(*) > 0 FROM message m WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL \
             AND EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & ?3) != 0)) \
         WHERE id = ?1",
        params![thread, Flags::SEEN.bits(), Flags::FLAGGED.bits()],
    )?;
    crate::apply::derive_threads(conn, &BTreeSet::from([thread]))?;
    let members: Vec<(String, i64)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT email_id, date FROM message WHERE thread_id = ?1 AND tombstoned_at IS NULL",
        )?;
        stmt.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (email_id, date) in members {
        let email_id = EmailId::parse(&email_id)?;
        for op in overlay.ops_for(&email_id) {
            let Some(dest) = op.destination() else {
                continue;
            };
            for (id, name, role, is_archive) in views {
                let view = MailboxView {
                    name,
                    role: *role,
                    archive_mode: mode,
                    is_archive: *is_archive,
                };
                if dest.matches(&view) {
                    conn.execute(
                        "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) \
                         VALUES (?1, ?2, ?3) ON CONFLICT (thread_id, mailbox_id) \
                         DO UPDATE SET last_date = max(last_date, excluded.last_date)",
                        params![thread, id, date],
                    )?;
                }
            }
        }
    }
    let after = thread_mailboxes(conn, thread)?;
    let removed: Vec<i64> = before
        .keys()
        .filter(|m| !after.contains_key(m))
        .copied()
        .collect();
    let last_date: i64 = conn.query_row(
        "SELECT last_date FROM thread WHERE id = ?1",
        [thread],
        |r| r.get(0),
    )?;
    let labels: Vec<String> = {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT l.label_key FROM message m \
             JOIN message_label ml ON ml.message_id = m.id \
             JOIN label l ON l.id = ml.label_id \
             WHERE m.thread_id = ?1 AND m.tombstoned_at IS NULL ORDER BY l.label_key",
        )?;
        stmt.query_map([thread], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    Ok(Some(NewChange {
        entity: Entity::Thread,
        entity_id: thread,
        op: ChangeOp::Upsert,
        payload: Some(
            json!({
                "accountId": account,
                "lastDate": last_date,
                "mailboxes": after.keys().collect::<Vec<_>>(),
                "removedFrom": removed,
                "labels": labels,
            })
            .to_string(),
        ),
    }))
}

fn thread_mailboxes(conn: &Connection, thread: i64) -> Result<BTreeMap<i64, i64>> {
    let mut stmt = conn
        .prepare_cached("SELECT mailbox_id, last_date FROM thread_mailbox WHERE thread_id = ?1")?;
    let rows = stmt
        .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Messages of `account` among `email_ids` that the replica has.
pub fn messages_of(
    conn: &Connection,
    account: i64,
    email_ids: &BTreeSet<EmailId>,
) -> Result<BTreeSet<i64>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM message WHERE account_id = ?1 AND email_id = ?2")?;
    let mut out = BTreeSet::new();
    for e in email_ids {
        if let Some(id) = stmt
            .query_row(params![account, e.as_str()], |r| r.get(0))
            .optional()?
        {
            out.insert(id);
        }
    }
    Ok(out)
}
