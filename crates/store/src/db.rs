use std::ops::Deref;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, Transaction};
use tagmail_core::{CacheEpoch, ChangeCursor, ChangeFilter};

use crate::changes::{self, Changes};
use crate::error::{Error, Result};
use crate::migrations;
use crate::readers::Readers;
use crate::writer::Writer;

const BUSY_TIMEOUT_MS: i64 = 5000;

/// Per-connection SQLite page cache in KiB (negative `cache_size`) and pool
/// size, plan §3.13.
struct Sizing {
    name: &'static str,
    writer_cache_kib: i64,
    reader_cache_kib: i64,
    readers: usize,
    migrations: &'static [&'static str],
}

const CONFIG: Sizing = Sizing {
    name: "config",
    writer_cache_kib: 512,
    reader_cache_kib: 512,
    readers: 1,
    migrations: migrations::CONFIG,
};

const CACHE: Sizing = Sizing {
    name: "cache",
    writer_cache_kib: 4096,
    reader_cache_kib: 2048,
    readers: 2,
    migrations: migrations::CACHE,
};

fn configure(conn: &Connection, cache_kib: i64) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    conn.pragma_update(None, "cache_size", -cache_kib)?;
    conn.pragma_update(None, "mmap_size", 0)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// One SQLite file: a single writer and a small reader pool, both in WAL mode.
pub struct Database {
    writer: Writer,
    readers: Readers,
}

impl Database {
    /// Opens the writer, migrates, runs `init` on the writer connection and
    /// opens the readers.
    fn open<T>(
        path: &Path,
        sizing: &Sizing,
        init: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<(Database, T)> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        configure(&conn, sizing.writer_cache_kib)?;
        migrations::apply(&mut conn, sizing.migrations)?;
        let initialized = init(&conn)?;

        let readers = (0..sizing.readers)
            .map(|_| {
                let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                let reader = Connection::open_with_flags(path, flags)?;
                configure(&reader, sizing.reader_cache_kib)?;
                reader.pragma_update(None, "query_only", true)?;
                Ok(reader)
            })
            .collect::<Result<Vec<_>>>()?;

        let db = Database {
            writer: Writer::spawn(sizing.name, conn)?,
            readers: Readers::new(readers),
        };
        Ok((db, initialized))
    }

    /// Runs `f` in one immediate transaction on the writer. Keep it to one
    /// sync window (≤ 50 ms); slower transactions are logged.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        self.writer.write(f).await
    }

    /// Runs `f` on a pooled read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.readers.read(f).await
    }

    /// Returns cached pages to the allocator; call after sync bursts.
    pub async fn shrink_memory(&self) -> Result<()> {
        self.writer
            .call(|conn| Ok(conn.execute_batch("PRAGMA shrink_memory")?))
            .await?;
        self.readers
            .for_each_idle(|conn| Ok(conn.execute_batch("PRAGMA shrink_memory")?))
            .await
    }

    /// Finishes queued writes and closes all connections.
    pub async fn close(self) -> Result<()> {
        self.readers.close();
        self.writer.close().await
    }
}

/// Durable `config.db`.
pub struct ConfigDb(Database);

impl ConfigDb {
    /// Opens or creates the file and applies pending migrations. Blocking.
    pub fn open(path: &Path) -> Result<ConfigDb> {
        Database::open(path, &CONFIG, |_| Ok(())).map(|(db, ())| ConfigDb(db))
    }

    pub async fn close(self) -> Result<()> {
        self.0.close().await
    }
}

impl Deref for ConfigDb {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.0
    }
}

/// Disposable `cache.db`.
pub struct CacheDb {
    db: Database,
    epoch: CacheEpoch,
}

impl CacheDb {
    /// Opens or creates the file and applies pending migrations. A new file
    /// gets a new random epoch. Blocking.
    pub fn open(path: &Path) -> Result<CacheDb> {
        let (db, epoch) = Database::open(path, &CACHE, changes::read_epoch)?;
        Ok(CacheDb { db, epoch })
    }

    /// Deletes a closed cache database and its WAL files. The next `open`
    /// starts a new epoch.
    pub fn wipe(path: &Path) -> Result<()> {
        let mut names = vec![path.as_os_str().to_owned()];
        for suffix in ["-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            names.push(name);
        }
        for name in names {
            match std::fs::remove_file(&name) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn epoch(&self) -> CacheEpoch {
        self.epoch
    }

    /// Cursor of the newest change-log entry.
    pub async fn head(&self) -> Result<ChangeCursor> {
        let epoch = self.epoch;
        self.db.read(move |conn| changes::head(conn, epoch)).await
    }

    /// Up to `limit` changes after `since`, or [`Changes::Gone`].
    pub async fn changes_since(
        &self,
        since: ChangeCursor,
        limit: usize,
        filter: ChangeFilter,
    ) -> Result<Changes> {
        let epoch = self.epoch;
        self.db
            .read(move |conn| changes::changes_since(conn, epoch, since, limit, filter))
            .await
    }

    /// See [`changes::trim`].
    pub async fn trim_changes(&self, older_than: i64, max_rows: i64) -> Result<i64> {
        self.db
            .write(move |tx| changes::trim(tx, older_than, max_rows))
            .await
    }

    pub async fn close(self) -> Result<()> {
        self.db.close().await
    }
}

impl Deref for CacheDb {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.db
    }
}

/// Fails with `Error::Corrupt` unless `value` parses.
pub(crate) fn parse_meta<T: std::str::FromStr>(key: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::Corrupt(format!("meta {key} = {value:?}")))
}
