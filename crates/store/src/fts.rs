//! Contentless FTS5 index `message_fts`, keyed by `message.id`. Contentless
//! means no `snippet()`/`highlight()`: those come from the text blob.

use rusqlite::{Connection, params};
use tagmail_core::MessageRowId;

use crate::error::Result;

#[derive(Debug, Clone, Copy, Default)]
pub struct FtsDocument<'a> {
    pub subject: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub body: &'a str,
}

/// Indexes a message, replacing its previous row.
pub fn index(conn: &Connection, message: MessageRowId, doc: &FtsDocument<'_>) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR REPLACE INTO message_fts (rowid, subject, from_txt, to_txt, body) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    stmt.execute(params![message.0, doc.subject, doc.from, doc.to, doc.body])?;
    Ok(())
}

pub fn delete(conn: &Connection, message: MessageRowId) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM message_fts WHERE rowid = ?1")?;
    stmt.execute([message.0])?;
    Ok(())
}

/// Messages matching an FTS5 query expression (phrases `"a b"`, `NEAR`,
/// column filters `subject: x`), highest id first. Ordering by date is the
/// query planner's job.
pub fn search(conn: &Connection, expr: &str, limit: usize) -> Result<Vec<MessageRowId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT rowid FROM message_fts WHERE message_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2",
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let ids = stmt
        .query_map(params![expr, limit], |r| r.get(0).map(MessageRowId))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}
