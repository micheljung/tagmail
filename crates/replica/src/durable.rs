//! `replica-durable.db`: queued intents, drafts and the outbox. Unlike the
//! replica it migrates forward and is never discarded.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use url::Url;

use crate::error::{Error, Result};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/durable/V1__init.sql"),
    include_str!("../migrations/durable/V2__server_binding.sql"),
    include_str!("../migrations/durable/V3__intent_sent_at.sql"),
];

pub const BOUND_SERVER_KEY: &str = "bound_server";
pub const FROM_OTHER_SERVER: &str = "From another server";

pub const DURABLE_SCHEMA: i64 = MIGRATIONS.len() as i64;

const BUSY_TIMEOUT_MS: i64 = 5000;

pub struct Durable {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// First pairing, or the same server again: everything is kept.
    Bound,
    /// Another server: open intents and unsent mail failed, drafts kept
    /// locally only.
    Changed,
}

/// The server's identity: scheme, host, port and path, without a trailing
/// slash, query or fragment.
pub fn server_identity(server: &Url) -> String {
    let path = server.path().trim_end_matches('/');
    format!("{}{path}", server.origin().ascii_serialization())
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

    /// The identity of the server the rows are bound to.
    pub fn bound_server(&self) -> Result<Option<String>> {
        self.meta(BOUND_SERVER_KEY)
    }

    /// Binds the durable rows to `server`. On a change of server, every
    /// open intent and unsent outbox item fails with [`FROM_OTHER_SERVER`]
    /// and the drafts lose their server ids and stay local.
    pub fn bind_server(&self, server: &Url) -> Result<Binding> {
        let identity = server_identity(server);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let bound: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [BOUND_SERVER_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let binding = match bound {
            Some(b) if b != identity => {
                tx.execute(
                    "UPDATE intent SET other_server = 1, state = 'failed', error = ?1, next_at = NULL
                     WHERE state IN ('pending', 'sent')",
                    [FROM_OTHER_SERVER],
                )?;
                tx.execute("UPDATE intent SET other_server = 1", [])?;
                tx.execute(
                    "UPDATE outbox SET state = 'failed', error = ?1, next_at = NULL
                     WHERE state != 'sent'",
                    [FROM_OTHER_SERVER],
                )?;
                tx.execute(
                    "UPDATE draft SET other_server = 1, server_draft_id = NULL, synced_version = NULL",
                    [],
                )?;
                tx.execute("UPDATE draft_attachment SET server_upload_id = NULL", [])?;
                Binding::Changed
            }
            _ => Binding::Bound,
        };
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![BOUND_SERVER_KEY, identity],
        )?;
        tx.commit()?;
        Ok(binding)
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
    fn a_server_is_known_by_origin_and_path() {
        let id = |s: &str| server_identity(&Url::parse(s).unwrap());
        assert_eq!(id("https://Mail.Example:443/"), "https://mail.example");
        assert_eq!(
            id("https://mail.example/tm/"),
            id("https://mail.example/tm")
        );
        assert_ne!(id("https://mail.example"), id("https://mail.example:8443"));
        assert_ne!(id("https://mail.example"), id("https://other.example"));
    }

    #[test]
    fn binding_to_another_server_fails_open_work_once() {
        let dir = tempfile::tempdir().unwrap();
        let durable = Durable::open(&dir.path().join("d.db")).unwrap();
        let a = Url::parse("https://a.example/").unwrap();
        let b = Url::parse("https://b.example/").unwrap();
        assert_eq!(durable.bind_server(&a).unwrap(), Binding::Bound);
        assert_eq!(
            durable.bound_server().unwrap().as_deref(),
            Some("https://a.example")
        );
        durable
            .conn()
            .execute(
                "INSERT INTO intent (op_id, seq, kind, account_id, email_ids, created_at, state)
                 VALUES ('op-1', 1, 'archive', 7, '[]', 0, 'sent')",
                [],
            )
            .unwrap();
        assert_eq!(durable.bind_server(&a).unwrap(), Binding::Bound);
        assert_eq!(durable.bind_server(&b).unwrap(), Binding::Changed);
        let (state, error, other): (String, String, bool) = durable
            .conn()
            .query_row("SELECT state, error, other_server FROM intent", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(
            (state.as_str(), error.as_str(), other),
            ("failed", FROM_OTHER_SERVER, true)
        );
        assert_eq!(durable.bind_server(&b).unwrap(), Binding::Bound);
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
