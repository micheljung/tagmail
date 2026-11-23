//! The disposable replica: `<data>/replica/{cache,config}.db` with the
//! server's own schemas, so the desktop reads through the same store code.
//! A replica written by another snapshot format or schema, or for another
//! server epoch, is discarded and bootstrapped again, never migrated.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use tagmail_api_core::replica_dto::SNAPSHOT_FORMAT;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_core::{CacheEpoch, ChangeCursor};
use tagmail_store::credentials::KEY_LEN;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

const REPLICA_DIR: &str = "replica";
const TMP_DIR: &str = "replica.tmp";
const OLD_DIR: &str = "replica.old";
const DURABLE_FILE: &str = "replica-durable.db";
const CACHE_FILE: &str = "cache.db";
const CONFIG_FILE: &str = "config.db";
const BLOBS_DIR: &str = "blobs";

const KEY_FORMAT: &str = "replica_snapshot_format";
const KEY_SERVER_EPOCH: &str = "replica_server_epoch";
const KEY_CURSOR: &str = "replica_cursor";

/// Paths below the desktop data directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout { root: root.into() }
    }

    pub fn replica(&self) -> PathBuf {
        self.root.join(REPLICA_DIR)
    }

    /// Where a bootstrap imports before it replaces [`Layout::replica`].
    pub fn replica_tmp(&self) -> PathBuf {
        self.root.join(TMP_DIR)
    }

    pub fn durable(&self) -> PathBuf {
        self.root.join(DURABLE_FILE)
    }

    /// Replaces the replica with a completed import in `replica.tmp`. The
    /// old replica stays readable up to the rename.
    pub fn install_tmp(&self) -> Result<()> {
        let old = self.root.join(OLD_DIR);
        remove_dir(&old)?;
        let current = self.replica();
        if current.exists() {
            std::fs::rename(&current, &old)?;
        }
        std::fs::rename(self.replica_tmp(), &current)?;
        remove_dir(&old)
    }

    pub fn discard(&self) -> Result<()> {
        remove_dir(&self.replica())?;
        self.discard_tmp()
    }

    pub fn discard_tmp(&self) -> Result<()> {
        remove_dir(&self.replica_tmp())
    }
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// What the running binary can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    pub snapshot_format: u32,
    pub cache_schema: i64,
    pub config_schema: i64,
}

impl Expected {
    pub fn current() -> Expected {
        Expected {
            snapshot_format: SNAPSHOT_FORMAT,
            cache_schema: tagmail_store::CACHE_SCHEMA,
            config_schema: tagmail_store::CONFIG_SCHEMA,
        }
    }
}

/// Server position a replica was synced to, kept in the replica's
/// `cache.db` so a page and its cursor commit together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaMeta {
    pub snapshot_format: u32,
    pub server_epoch: CacheEpoch,
    pub cursor: ChangeCursor,
}

impl ReplicaMeta {
    pub fn write(&self, tx: &Transaction<'_>) -> tagmail_store::Result<()> {
        set(tx, KEY_FORMAT, &self.snapshot_format.to_string())?;
        set(tx, KEY_SERVER_EPOCH, &self.server_epoch.to_string())?;
        set_cursor(tx, self.cursor)
    }
}

pub fn set_cursor(tx: &Transaction<'_>, cursor: ChangeCursor) -> tagmail_store::Result<()> {
    set(tx, KEY_CURSOR, &cursor.to_string())
}

fn set(conn: &Connection, key: &str, value: &str) -> tagmail_store::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Why a replica cannot be used and must be bootstrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    Missing,
    /// A bootstrap never finished writing its meta.
    Incomplete,
    SnapshotFormat,
    CacheSchema,
    ConfigSchema,
    ServerEpoch,
}

pub struct LocalDbs {
    pub cache: CacheDb,
    pub config: ConfigDb,
    pub blobs: BlobStore,
    pub meta: ReplicaMeta,
}

impl LocalDbs {
    /// The same store the server reads through, over the replica.
    pub async fn into_store(self) -> Result<(SqliteStore, ReplicaMeta)> {
        // The replica's config holds no account secrets, so its vault key
        // protects nothing and need not be kept anywhere.
        let master = MasterKey::from_bytes(Zeroizing::new([0; KEY_LEN]));
        let store = SqliteStore::open(self.config, self.cache, self.blobs, &master).await?;
        Ok((store, self.meta))
    }

    pub async fn close(self) -> Result<()> {
        self.cache.close().await?;
        self.config.close().await?;
        Ok(())
    }
}

pub enum Opened {
    Ready(LocalDbs),
    /// The replica directory was removed; the durable store is untouched.
    NeedsBootstrap(Discard),
}

/// Opens the replica if it matches `expected` and, when known, the
/// server's current epoch. Otherwise removes it. Blocking.
pub fn open(
    layout: &Layout,
    expected: Expected,
    server_epoch: Option<CacheEpoch>,
) -> Result<Opened> {
    let dir = layout.replica();
    let verdict = inspect(&dir, expected, server_epoch)?;
    let meta = match verdict {
        Ok(meta) => meta,
        Err(discard) => {
            layout.discard()?;
            return Ok(Opened::NeedsBootstrap(discard));
        }
    };
    Ok(Opened::Ready(LocalDbs {
        cache: CacheDb::open(&dir.join(CACHE_FILE))?,
        config: ConfigDb::open(&dir.join(CONFIG_FILE))?,
        blobs: BlobStore::open(&dir.join(BLOBS_DIR))?,
        meta,
    }))
}

/// Opens fresh databases in `dir` for an import.
pub fn create(dir: &Path) -> Result<(CacheDb, ConfigDb, BlobStore)> {
    remove_dir(dir)?;
    std::fs::create_dir_all(dir)?;
    Ok((
        CacheDb::open(&dir.join(CACHE_FILE))?,
        ConfigDb::open(&dir.join(CONFIG_FILE))?,
        BlobStore::open(&dir.join(BLOBS_DIR))?,
    ))
}

/// Reads versions without opening through the store, which would migrate.
fn inspect(
    dir: &Path,
    expected: Expected,
    server_epoch: Option<CacheEpoch>,
) -> Result<Result<ReplicaMeta, Discard>> {
    let cache = dir.join(CACHE_FILE);
    let config = dir.join(CONFIG_FILE);
    if !cache.exists() || !config.exists() {
        return Ok(Err(Discard::Missing));
    }
    if user_version(&config)? != expected.config_schema {
        return Ok(Err(Discard::ConfigSchema));
    }
    let conn = read_only(&cache)?;
    if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?
        != expected.cache_schema
    {
        return Ok(Err(Discard::CacheSchema));
    }
    let (Some(format), Some(epoch), Some(cursor)) = (
        get(&conn, KEY_FORMAT)?,
        get(&conn, KEY_SERVER_EPOCH)?,
        get(&conn, KEY_CURSOR)?,
    ) else {
        return Ok(Err(Discard::Incomplete));
    };
    let meta = ReplicaMeta {
        snapshot_format: parse(KEY_FORMAT, &format)?,
        server_epoch: parse(KEY_SERVER_EPOCH, &epoch)?,
        cursor: parse(KEY_CURSOR, &cursor)?,
    };
    if meta.snapshot_format != expected.snapshot_format {
        return Ok(Err(Discard::SnapshotFormat));
    }
    if server_epoch.is_some_and(|e| e != meta.server_epoch)
        || meta.cursor.epoch != meta.server_epoch
    {
        return Ok(Err(Discard::ServerEpoch));
    }
    Ok(Ok(meta))
}

fn read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn user_version(path: &Path) -> Result<i64> {
    Ok(read_only(path)?.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn parse<T: std::str::FromStr>(key: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::Corrupt(format!("meta {key} = {value:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::Durable;

    const EPOCH: CacheEpoch = CacheEpoch(0xabc);

    fn meta() -> ReplicaMeta {
        ReplicaMeta {
            snapshot_format: SNAPSHOT_FORMAT,
            server_epoch: EPOCH,
            cursor: ChangeCursor {
                epoch: EPOCH,
                seq: 42,
            },
        }
    }

    async fn bootstrapped(layout: &Layout, meta: Option<ReplicaMeta>) {
        let (cache, config, _) = create(&layout.replica()).unwrap();
        if let Some(meta) = meta {
            cache.write(move |tx| meta.write(tx)).await.unwrap();
        }
        cache.close().await.unwrap();
        config.close().await.unwrap();
    }

    fn queued_intent(durable: &Durable) {
        durable
            .conn()
            .execute(
                "INSERT INTO intent (op_id, seq, kind, account_id, email_ids, created_at, state)
                 VALUES ('op-1', 1, 'archive', 1, '[]', 0, 'pending')",
                [],
            )
            .unwrap();
    }

    fn intents(durable: &Durable) -> i64 {
        durable
            .conn()
            .query_row("SELECT count(*) FROM intent", [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn matching_replica_opens_with_its_meta() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        bootstrapped(&layout, Some(meta())).await;

        match open(&layout, Expected::current(), Some(EPOCH)).unwrap() {
            Opened::Ready(dbs) => {
                assert_eq!(dbs.meta, meta());
                dbs.close().await.unwrap();
            }
            Opened::NeedsBootstrap(why) => panic!("discarded: {why:?}"),
        }
    }

    #[tokio::test]
    async fn every_mismatch_discards_the_replica_but_not_the_durable_store() {
        let current = Expected::current();
        let cases = [
            (None, current, Some(EPOCH), Discard::Incomplete),
            (
                Some(meta()),
                Expected {
                    snapshot_format: SNAPSHOT_FORMAT + 1,
                    ..current
                },
                None,
                Discard::SnapshotFormat,
            ),
            (
                Some(meta()),
                Expected {
                    cache_schema: current.cache_schema + 1,
                    ..current
                },
                None,
                Discard::CacheSchema,
            ),
            (
                Some(meta()),
                Expected {
                    config_schema: current.config_schema + 1,
                    ..current
                },
                None,
                Discard::ConfigSchema,
            ),
            (
                Some(meta()),
                current,
                Some(CacheEpoch(0xdef)),
                Discard::ServerEpoch,
            ),
        ];
        for (meta, expected, epoch, why) in cases {
            let dir = tempfile::tempdir().unwrap();
            let layout = Layout::new(dir.path());
            let durable = Durable::open(&layout.durable()).unwrap();
            queued_intent(&durable);
            bootstrapped(&layout, meta).await;

            match open(&layout, expected, epoch).unwrap() {
                Opened::NeedsBootstrap(got) => assert_eq!(got, why),
                Opened::Ready(_) => panic!("{why:?} kept the replica"),
            }
            assert!(!layout.replica().exists(), "{why:?}");
            assert_eq!(intents(&durable), 1, "{why:?}");
        }
    }

    #[tokio::test]
    async fn missing_replica_needs_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        assert!(matches!(
            open(&layout, Expected::current(), None).unwrap(),
            Opened::NeedsBootstrap(Discard::Missing)
        ));
    }

    #[tokio::test]
    async fn install_replaces_the_replica_with_the_import() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        bootstrapped(&layout, Some(meta())).await;
        let newer = ReplicaMeta {
            cursor: ChangeCursor {
                epoch: EPOCH,
                seq: 99,
            },
            ..meta()
        };
        let (cache, config, _) = create(&layout.replica_tmp()).unwrap();
        cache.write(move |tx| newer.write(tx)).await.unwrap();
        cache.close().await.unwrap();
        config.close().await.unwrap();

        layout.install_tmp().unwrap();

        assert!(!layout.replica_tmp().exists());
        match open(&layout, Expected::current(), Some(EPOCH)).unwrap() {
            Opened::Ready(dbs) => {
                assert_eq!(dbs.meta.cursor.seq, 99);
                dbs.close().await.unwrap();
            }
            Opened::NeedsBootstrap(why) => panic!("discarded: {why:?}"),
        }
    }
}
