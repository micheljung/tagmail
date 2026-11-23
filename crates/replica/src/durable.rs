//! `replica-durable.db`: queued intents, drafts and the outbox. Unlike the
//! replica it migrates forward and is never discarded.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

const MIGRATIONS: &[&str] = &[include_str!("../migrations/durable/V1__init.sql")];

pub const DURABLE_SCHEMA: i64 = MIGRATIONS.len() as i64;

const BUSY_TIMEOUT_MS: i64 = 5000;

pub struct Durable {
    conn: Mutex<Connection>,
}

impl Durable {
    /// Opens or creates the file and applies pending migrations. Blocking.
    pub fn open(path: &Path) -> Result<Durable> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut conn)?;
        Ok(Durable {
            conn: Mutex::new(conn),
        })
    }

    /// The connection; short synchronous statements only.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn schema(&self) -> Result<i64> {
        Ok(self
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > DURABLE_SCHEMA {
        return Err(Error::DurableTooNew {
            found: current,
            supported: DURABLE_SCHEMA,
        });
    }
    for (version, sql) in (1..).zip(MIGRATIONS).skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_from_empty_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica-durable.db");
        let durable = Durable::open(&path).unwrap();
        assert_eq!(durable.schema().unwrap(), DURABLE_SCHEMA);
        durable
            .set_meta("server_url", "https://mail.example")
            .unwrap();
        durable
            .conn()
            .execute(
                "INSERT INTO intent (op_id, seq, kind, account_id, email_ids, created_at, state)
                 VALUES ('op-1', 1, 'archive', 7, '[\"E1\"]', 0, 'pending')",
                [],
            )
            .unwrap();
        drop(durable);

        let durable = Durable::open(&path).unwrap();
        assert_eq!(
            durable.meta("server_url").unwrap().as_deref(),
            Some("https://mail.example")
        );
        let state: String = durable
            .conn()
            .query_row("SELECT state FROM intent WHERE op_id = 'op-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "pending");
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replica-durable.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", DURABLE_SCHEMA + 1)
            .unwrap();
        assert!(matches!(
            Durable::open(&path),
            Err(Error::DurableTooNew { .. })
        ));
    }

    #[test]
    fn outbox_needs_its_draft_and_attachments_follow_it() {
        let dir = tempfile::tempdir().unwrap();
        let durable = Durable::open(&dir.path().join("d.db")).unwrap();
        let conn = durable.conn();
        let orphan = conn.execute(
            "INSERT INTO outbox (op_id, client_id, created_at, state)
             VALUES ('s-1', 'missing', 0, 'queued')",
            [],
        );
        assert!(orphan.is_err());
        conn.execute(
            "INSERT INTO draft (client_id, account_id, content, updated_at)
             VALUES ('c-1', 1, '{}', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO draft_attachment (client_id, name, content_type, size, path)
             VALUES ('c-1', 'a.txt', 'text/plain', 1, '/tmp/a')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM draft WHERE client_id = 'c-1'", [])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM draft_attachment", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }
}
