//! `change_log`: the ordered feed behind `/changes`, SSE and the replica
//! (plan §3.6). Cursors are `<cache_epoch>:<seq>`; an epoch mismatch or a
//! cursor below the trim floor `meta.min_seq` is [`Changes::Gone`] (HTTP 410).

use std::fmt;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, params};
use tagmail_core::{CacheEpoch, ChangeCursor};

use crate::db::parse_meta;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    Account,
    Mailbox,
    Thread,
    Message,
    Label,
}

impl Entity {
    const ALL: [Entity; 5] = [
        Entity::Account,
        Entity::Mailbox,
        Entity::Thread,
        Entity::Message,
        Entity::Label,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Entity::Account => "account",
            Entity::Mailbox => "mailbox",
            Entity::Thread => "thread",
            Entity::Message => "message",
            Entity::Label => "label",
        }
    }
}

impl FromStr for Entity {
    type Err = Error;

    fn from_str(s: &str) -> Result<Entity> {
        Entity::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| Error::Corrupt(format!("change_log entity {s:?}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    Upsert,
    Delete,
    /// The entity was merged into the thread with this id.
    Merged {
        target: i64,
    },
    /// Many entities of the mailbox with this id changed (initial sync
    /// window); clients refetch the affected views.
    Bulk {
        mailbox: i64,
    },
}

impl fmt::Display for ChangeOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangeOp::Upsert => f.write_str("upsert"),
            ChangeOp::Delete => f.write_str("delete"),
            ChangeOp::Merged { target } => write!(f, "merged:{target}"),
            ChangeOp::Bulk { mailbox } => write!(f, "bulk:{mailbox}"),
        }
    }
}

impl FromStr for ChangeOp {
    type Err = Error;

    fn from_str(s: &str) -> Result<ChangeOp> {
        let corrupt = || Error::Corrupt(format!("change_log op {s:?}"));
        match s.split_once(':') {
            None if s == "upsert" => Ok(ChangeOp::Upsert),
            None if s == "delete" => Ok(ChangeOp::Delete),
            Some(("merged", id)) => Ok(ChangeOp::Merged {
                target: id.parse().map_err(|_| corrupt())?,
            }),
            Some(("bulk", id)) => Ok(ChangeOp::Bulk {
                mailbox: id.parse().map_err(|_| corrupt())?,
            }),
            _ => Err(corrupt()),
        }
    }
}

/// An entry to append. `payload` is caller-defined JSON, e.g. a thread's
/// `last_date` and affected view ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChange {
    pub entity: Entity,
    pub entity_id: i64,
    pub op: ChangeOp,
    pub payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub seq: i64,
    pub entity: Entity,
    pub entity_id: i64,
    pub op: ChangeOp,
    pub payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changes {
    /// `next` is the cursor to resume from; `more` says whether entries
    /// beyond `limit` remain.
    Page {
        changes: Vec<Change>,
        next: ChangeCursor,
        more: bool,
    },
    /// The cursor is from another cache epoch or older than the trim floor:
    /// the client must refetch (web) or re-bootstrap (replica).
    Gone,
}

pub(crate) fn read_epoch(conn: &Connection) -> Result<CacheEpoch> {
    let value: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'cache_epoch'",
        [],
        |r| r.get(0),
    )?;
    parse_meta("cache_epoch", &value)
}

fn min_seq(conn: &Connection) -> Result<i64> {
    let value: String =
        conn.query_row("SELECT value FROM meta WHERE key = 'min_seq'", [], |r| {
            r.get(0)
        })?;
    parse_meta("min_seq", &value)
}

fn head_seq(conn: &Connection) -> Result<i64> {
    let seq = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'change_log'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(seq.unwrap_or(0))
}

/// Appends one entry inside the caller's write transaction, so it commits
/// atomically with the data it describes. Returns its seq.
pub fn append(conn: &Connection, change: &NewChange) -> Result<i64> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO change_log (entity, entity_id, op, payload) VALUES (?1, ?2, ?3, ?4)",
    )?;
    stmt.execute(params![
        change.entity.as_str(),
        change.entity_id,
        change.op.to_string(),
        change.payload
    ])?;
    Ok(conn.last_insert_rowid())
}

/// Appends entries in order; returns the last seq, if any.
pub fn append_all<'a>(
    conn: &Connection,
    changes: impl IntoIterator<Item = &'a NewChange>,
) -> Result<Option<i64>> {
    changes
        .into_iter()
        .try_fold(None, |_, change| append(conn, change).map(Some))
}

pub fn head(conn: &Connection, epoch: CacheEpoch) -> Result<ChangeCursor> {
    Ok(ChangeCursor {
        epoch,
        seq: head_seq(conn)?,
    })
}

pub fn changes_since(
    conn: &Connection,
    epoch: CacheEpoch,
    since: ChangeCursor,
    limit: usize,
) -> Result<Changes> {
    let _snapshot = if conn.is_autocommit() {
        Some(conn.unchecked_transaction()?)
    } else {
        None
    };
    if since.epoch != epoch || since.seq < min_seq(conn)? || since.seq > head_seq(conn)? {
        return Ok(Changes::Gone);
    }
    let fetch = i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1);
    let mut stmt = conn.prepare_cached(
        "SELECT seq, entity, entity_id, op, payload FROM change_log \
         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let mut changes = stmt
        .query_map(params![since.seq, fetch], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .map(|row| {
            let (seq, entity, entity_id, op, payload) = row?;
            Ok(Change {
                seq,
                entity: entity.parse()?,
                entity_id,
                op: op.parse()?,
                payload,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let more = changes.len() > limit;
    changes.truncate(limit);
    let next = ChangeCursor {
        epoch,
        seq: changes.last().map_or(since.seq, |c| c.seq),
    };
    Ok(Changes::Page {
        changes,
        next,
        more,
    })
}

/// Deletes entries created before `older_than` (Unix seconds) and all but
/// the newest `max_rows`, and raises the `min_seq` floor to the last deleted
/// seq. Returns the new floor.
pub fn trim(conn: &Connection, older_than: i64, max_rows: i64) -> Result<i64> {
    let by_age: Option<i64> = conn.query_row(
        "SELECT max(seq) FROM change_log WHERE created_at < ?1",
        [older_than],
        |r| r.get(0),
    )?;
    let by_count = head_seq(conn)? - max_rows.max(0);
    let floor = min_seq(conn)?.max(by_age.unwrap_or(0)).max(by_count);
    conn.execute("DELETE FROM change_log WHERE seq <= ?1", [floor])?;
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'min_seq'",
        [floor.to_string()],
    )?;
    Ok(floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_encoding_round_trips() {
        for op in [
            ChangeOp::Upsert,
            ChangeOp::Delete,
            ChangeOp::Merged { target: 7 },
            ChangeOp::Bulk { mailbox: 3 },
        ] {
            assert_eq!(op.to_string().parse::<ChangeOp>().unwrap(), op);
        }
        assert_eq!(ChangeOp::Merged { target: 7 }.to_string(), "merged:7");
        assert_eq!(ChangeOp::Bulk { mailbox: 3 }.to_string(), "bulk:3");
        for bad in ["", "upsert:1", "merged", "merged:x", "bulk:", "other:1"] {
            assert!(bad.parse::<ChangeOp>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn entity_encoding_round_trips() {
        for e in Entity::ALL {
            assert_eq!(e.as_str().parse::<Entity>().unwrap(), e);
        }
        assert!("Thread".parse::<Entity>().is_err());
    }
}
