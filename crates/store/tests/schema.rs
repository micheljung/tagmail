use std::path::Path;

use tagmail_store::rusqlite::{self, Connection};
use tagmail_store::{CACHE_SCHEMA, CONFIG_SCHEMA, CacheDb, ConfigDb, Database, Error};
use tempfile::TempDir;

#[test]
fn bundled_sqlite_supports_contentless_delete() {
    assert!(
        rusqlite::version_number() >= 3_043_000,
        "SQLite {} < 3.43.0",
        rusqlite::version()
    );
}

fn table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'message_fts_%' ORDER BY name",
    )?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

async fn tables(db: &Database) -> Vec<String> {
    db.read(|c| Ok(table_names(c)?)).await.unwrap()
}

async fn user_version(db: &Database) -> i64 {
    db.read(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap()
}

async fn integrity(db: &Database) {
    let (ok, fk_violations) = db
        .read(|c| {
            let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            let fk: i64 =
                c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            Ok((ok, fk))
        })
        .await
        .unwrap();
    assert_eq!(ok, "ok");
    assert_eq!(fk_violations, 0);
}

#[tokio::test]
async fn config_migrations_apply_to_fresh_db_and_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    assert_eq!(
        tables(&db).await,
        [
            "account",
            "device",
            "draft",
            "draft_purge",
            "identity",
            "image_sender",
            "label_job",
            "label_registry",
            "meta",
            "op_seen",
            "outbox",
            "owner",
            "pairing_code",
            "passkey",
            "pending_op",
            "push_subscription",
            "session",
            "upload",
        ]
    );
    integrity(&db).await;
    db.close().await.unwrap();

    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    db.close().await.unwrap();
}

#[tokio::test]
async fn cache_migrations_apply_to_fresh_db_and_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.db");
    let db = CacheDb::open(&path).unwrap();
    let epoch = db.epoch();
    assert_eq!(user_version(&db).await, CACHE_SCHEMA);
    assert_eq!(
        tables(&db).await,
        [
            "change_log",
            "label",
            "location",
            "mailbox",
            "message",
            "message_fts",
            "message_label",
            "meta",
            "thread",
            "thread_mailbox",
            "thread_ref",
        ]
    );
    integrity(&db).await;
    db.close().await.unwrap();

    let db = CacheDb::open(&path).unwrap();
    assert_eq!(db.epoch(), epoch, "reopening keeps the epoch");
    db.close().await.unwrap();
}

#[tokio::test]
async fn newer_schema_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    assert!(matches!(
        ConfigDb::open(&path),
        Err(Error::SchemaTooNew {
            found: 99,
            supported: CONFIG_SCHEMA
        })
    ));
}

#[derive(Debug, PartialEq)]
struct Pragmas {
    cache_size: i64,
    mmap_size: i64,
    journal_mode: String,
    foreign_keys: i64,
    busy_timeout: i64,
    query_only: i64,
}

fn pragmas(c: &Connection) -> rusqlite::Result<Pragmas> {
    let int = |name: &str| c.pragma_query_value(None, name, |r| r.get::<_, i64>(0));
    Ok(Pragmas {
        cache_size: int("cache_size")?,
        mmap_size: int("mmap_size")?,
        journal_mode: c.pragma_query_value(None, "journal_mode", |r| r.get(0))?,
        foreign_keys: int("foreign_keys")?,
        busy_timeout: int("busy_timeout")?,
        query_only: int("query_only")?,
    })
}

fn expected(cache_size: i64, query_only: i64) -> Pragmas {
    Pragmas {
        cache_size,
        mmap_size: 0,
        journal_mode: "wal".into(),
        foreign_keys: 1,
        busy_timeout: 5000,
        query_only,
    }
}

async fn assert_pragmas(db: &Database, writer_cache: i64, reader_cache: i64) {
    let w = db.write(|tx| Ok(pragmas(tx)?)).await.unwrap();
    assert_eq!(w, expected(writer_cache, 0));
    let r = db.read(|c| Ok(pragmas(c)?)).await.unwrap();
    assert_eq!(r, expected(reader_cache, 1));
    db.shrink_memory().await.unwrap();
}

#[tokio::test]
async fn connections_follow_the_memory_budget() {
    let dir = TempDir::new().unwrap();
    let cache = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    assert_pragmas(&cache, -4096, -2048).await;
    let config = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    assert_pragmas(&config, -512, -512).await;
}

#[tokio::test]
async fn readers_cannot_write() {
    let dir = TempDir::new().unwrap();
    let db = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    let err = db
        .read(|c| Ok(c.execute("INSERT INTO meta VALUES ('k', 'v')", [])?))
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn fts_uses_full_detail() {
    let dir = TempDir::new().unwrap();
    let db = CacheDb::open(&dir.path().join("cache.db")).unwrap();
    let sql: String = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'message_fts'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    let compact: String = sql.split_whitespace().collect();
    assert!(compact.contains("detail=full"), "{sql}");
    assert!(compact.contains("contentless_delete=1"), "{sql}");
    assert!(compact.contains("content=''"), "{sql}");
}

/// Columns that would hold a cache.db surrogate id or a change-log seq.
fn cache_references(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    const DURABLE_ID_COLUMNS: &[&str] = &[
        "id",
        "account_id",
        "identity_id",
        "op_id",
        "cred_id",
        "reply_to_email_id",
        "draft_id",
        "client_id",
    ];
    let mut found = Vec::new();
    for table in table_names(conn)? {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
        let columns: Vec<String> = stmt
            .query_map([&table], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for column in columns {
            let id_like = column == "seq" || column.ends_with("_id") || column == "id";
            if id_like && !DURABLE_ID_COLUMNS.contains(&column.as_str()) {
                found.push(format!("{table}.{column}"));
            }
        }
        let mut fks = conn.prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1)")?;
        let targets: Vec<String> = fks
            .query_map([&table], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for target in targets {
            if !["account", "identity", "draft"].contains(&target.as_str()) {
                found.push(format!("{table} -> {target}"));
            }
        }
    }
    Ok(found)
}

fn open_raw(path: &Path) -> Connection {
    Connection::open(path).unwrap()
}

#[tokio::test]
async fn durable_tables_hold_no_cache_ids() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.db");
    let cache = dir.path().join("cache.db");
    ConfigDb::open(&config).unwrap().close().await.unwrap();
    CacheDb::open(&cache).unwrap().close().await.unwrap();

    assert_eq!(
        cache_references(&open_raw(&config)).unwrap(),
        Vec::<String>::new()
    );

    let in_cache = cache_references(&open_raw(&cache)).unwrap();
    for expected in [
        "location.mailbox_id",
        "message.thread_id",
        "change_log.entity_id",
    ] {
        assert!(
            in_cache.iter().any(|c| c == expected),
            "{expected} not detected"
        );
    }
}

#[tokio::test]
async fn pending_op_mailbox_reference_is_complete_or_absent() {
    let dir = TempDir::new().unwrap();
    let db = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    let result = db
        .write(|tx| {
            tx.execute(
                "INSERT INTO account (name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                 VALUES ('a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00')",
                [],
            )?;
            tx.execute(
                "INSERT INTO pending_op (op_id, account_id, kind, mailbox_name, \
                 mailbox_special_use, mailbox_uidvalidity) \
                 VALUES ('op1', 1, 'move', 'Archive', '\\Archive', 7)",
                [],
            )?;
            Ok(tx.execute(
                "INSERT INTO pending_op (op_id, account_id, kind, mailbox_name) \
                 VALUES ('op2', 1, 'move', 'Archive')",
                [],
            )?)
        })
        .await;
    assert!(result.is_err(), "mailbox name without uidvalidity accepted");
    let count: i64 = db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM pending_op", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0, "failed write transaction rolled back");
}

#[tokio::test]
async fn config_v3_upgrades_to_outbox_files() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    {
        let conn = open_raw(&path);
        for sql in [
            include_str!("../migrations/config/V1__init.sql"),
            include_str!("../migrations/config/V2__image_sender.sql"),
            include_str!("../migrations/config/V3__compose.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
             VALUES (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00'); \
             INSERT INTO outbox (op_id, account_id, mime_blob, msgid_hdr, sender, \
             recipients, subject) VALUES ('op', 1, x'0102', 'm@x', 'u@x', '[]', 'Hi');",
        )
        .unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let row = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT op_id, state, mime_file, mime_size, flag_msgid, flag_kind FROM outbox",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        row,
        ("op".to_owned(), "queued".to_owned(), None, 0, None, None)
    );
    let blob_gone = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM pragma_table_info('outbox') WHERE name = 'mime_blob'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(blob_gone, 0);
    integrity(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn config_v4_upgrades_with_duplicate_identities() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    {
        let conn = open_raw(&path);
        for sql in [
            include_str!("../migrations/config/V1__init.sql"),
            include_str!("../migrations/config/V2__image_sender.sql"),
            include_str!("../migrations/config/V3__compose.sql"),
            include_str!("../migrations/config/V4__outbox_file.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce) VALUES \
             (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'Me@Example.com', x'00', x'00'), \
             (2, 'b', 'i', 993, 'tls', 's', 465, 'tls', 'bare', x'00', x'00'); \
             INSERT INTO identity (id, account_id, address, is_default) VALUES \
             (1, 1, 'me@example.com', 0), (2, 1, 'ME@example.com', 1), \
             (3, 1, 'alias@example.com', 0), (4, 2, 'bare@i', 1), (5, 2, 'bare@i', 0);",
        )
        .unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let (identities, emails) = db
        .read(|c| {
            let mut stmt =
                c.prepare("SELECT id, account_id, is_default FROM identity ORDER BY id")?;
            let identities = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()?;
            let mut stmt = c.prepare("SELECT email FROM account ORDER BY id")?;
            let emails = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<Option<String>>>>()?;
            Ok((identities, emails))
        })
        .await
        .unwrap();
    assert_eq!(identities, [(1, 1, 1), (3, 1, 0), (4, 2, 1)]);
    assert_eq!(emails, [Some("me@example.com".to_owned()), None]);
    let duplicate = db
        .write(|c| {
            Ok(c.execute(
                "INSERT INTO identity (account_id, address) VALUES (1, 'Alias@Example.com')",
                [],
            )?)
        })
        .await;
    assert!(duplicate.is_err(), "duplicate address accepted");
    integrity(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn config_v5_gives_identityless_accounts_a_default() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    {
        let mut conn = open_raw(&path);
        for sql in [
            include_str!("../migrations/config/V1__init.sql"),
            include_str!("../migrations/config/V2__image_sender.sql"),
            include_str!("../migrations/config/V3__compose.sql"),
            include_str!("../migrations/config/V4__outbox_file.sql"),
            include_str!("../migrations/config/V5__multi_account.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 5).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) VALUES \
             (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00', 'a@example.com'), \
             (2, 'b', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00', 'b@example.com'), \
             (3, 'c', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00', NULL); \
             INSERT INTO identity (account_id, address, is_default) \
             VALUES (2, 'alias@example.com', 1);",
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let identities = db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT account_id, address, is_default FROM identity ORDER BY account_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<(i64, String, i64)>>>()?)
        })
        .await
        .unwrap();
    assert_eq!(
        identities,
        [
            (1, "a@example.com".to_owned(), 1),
            (2, "alias@example.com".to_owned(), 1),
        ]
    );
    integrity(&db).await;
    db.close().await.unwrap();
}

#[test]
fn schema_constants_count_the_migrations() {
    assert_eq!((CONFIG_SCHEMA, CACHE_SCHEMA), (7, 3));
}

#[tokio::test]
async fn config_v7_makes_draft_client_ids_unique() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    {
        let conn = open_raw(&path);
        for sql in [
            include_str!("../migrations/config/V1__init.sql"),
            include_str!("../migrations/config/V2__image_sender.sql"),
            include_str!("../migrations/config/V3__compose.sql"),
            include_str!("../migrations/config/V4__outbox_file.sql"),
            include_str!("../migrations/config/V5__multi_account.sql"),
            include_str!("../migrations/config/V6__default_identity.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce) VALUES \
             (1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00'); \
             INSERT INTO draft (id, account_id, msgid_hdr, content) VALUES (1, 1, 'm1', '{}');",
        )
        .unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, 7);
    let insert = |id: i64, client_id: Option<&'static str>| {
        db.write(move |c| {
            Ok(c.execute(
                "INSERT INTO draft (id, account_id, msgid_hdr, content, client_id) \
                 VALUES (?1, 1, 'm', '{}', ?2)",
                rusqlite::params![id, client_id],
            )?)
        })
    };
    insert(2, None).await.unwrap();
    insert(3, Some("c-1")).await.unwrap();
    assert!(insert(4, Some("c-1")).await.is_err(), "duplicate client id");
    let legacy: Option<String> = db
        .read(|c| Ok(c.query_row("SELECT client_id FROM draft WHERE id = 1", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(legacy, None);
    integrity(&db).await;
    db.close().await.unwrap();
}
