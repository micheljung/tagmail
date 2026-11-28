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
            "alarm_ack",
            "alarm_sent",
            "cal_op",
            "caldav",
            "caldav_trust",
            "calendar_pref",
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
            "cal_alarm",
            "cal_event",
            "cal_inbox",
            "cal_instance",
            "cal_skip",
            "calendar",
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
async fn upgraded_cache_marks_old_messages_without_calendar() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.db");
    let conn = Connection::open(&path).unwrap();
    for sql in [
        include_str!("../migrations/cache/V1__init.sql"),
        include_str!("../migrations/cache/V2__pending_text_index.sql"),
        include_str!("../migrations/cache/V3__op_overlay.sql"),
        include_str!("../migrations/cache/V4__hide_has_cal.sql"),
    ] {
        conn.execute_batch(sql).unwrap();
    }
    conn.pragma_update(None, "user_version", 4).unwrap();
    conn.execute(
        "INSERT INTO message (account_id, email_id, date) VALUES (1, 'e1', 0)",
        [],
    )
    .unwrap();
    drop(conn);

    let db = CacheDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CACHE_SCHEMA);
    let has_calendar: i64 = db
        .read(|c| Ok(c.query_row("SELECT has_calendar FROM message", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(has_calendar, 0);
    let rejected = db
        .write(|tx| Ok(tx.execute("UPDATE message SET has_calendar = 2", [])?))
        .await;
    assert!(rejected.is_err(), "has_calendar is a boolean");
    integrity(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn upgraded_config_marks_seen_ops_as_mail() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.db");
    let conn = Connection::open(&path).unwrap();
    for sql in [
        include_str!("../migrations/config/V1__init.sql"),
        include_str!("../migrations/config/V2__image_sender.sql"),
        include_str!("../migrations/config/V3__compose.sql"),
        include_str!("../migrations/config/V4__outbox_file.sql"),
        include_str!("../migrations/config/V5__multi_account.sql"),
        include_str!("../migrations/config/V6__default_identity.sql"),
        include_str!("../migrations/config/V7__draft_client_id.sql"),
        include_str!("../migrations/config/V8__push_subscription.sql"),
    ] {
        conn.execute_batch(sql).unwrap();
    }
    conn.pragma_update(None, "user_version", 8).unwrap();
    conn.execute(
        "INSERT INTO op_seen (op_id, account_id) VALUES ('op-1', 1)",
        [],
    )
    .unwrap();
    drop(conn);

    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let kind: String = db
        .read(|c| Ok(c.query_row("SELECT kind FROM op_seen", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(kind, "mail");
    let rejected = db
        .write(|tx| Ok(tx.execute("UPDATE op_seen SET kind = 'other'", [])?))
        .await;
    assert!(rejected.is_err(), "kind is mail or cal");
    integrity(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn calendar_config_rows_go_with_their_account() {
    let dir = TempDir::new().unwrap();
    let db = ConfigDb::open(&dir.path().join("config.db")).unwrap();
    let left = db
        .write(|tx| {
            for id in [1, 2] {
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                     VALUES (?1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00')",
                    [id],
                )?;
            }
            tx.execute_batch(
                "INSERT INTO caldav (account_id) VALUES (1);
                 INSERT INTO caldav (account_id, fallback_account_id) VALUES (2, 1);
                 INSERT INTO calendar_pref (account_id, href) VALUES (1, '/c/');
                 INSERT INTO caldav_trust (account_id) VALUES (1);
                 INSERT INTO cal_op (op_id, account_id, target_account_id, uid, kind) \
                     VALUES ('a', 2, 1, 'u', 'apply');
                 INSERT INTO alarm_ack (alarm_key, account_id, fire_at) VALUES ('k', 1, 0);
                 INSERT INTO alarm_sent (alarm_key, target, account_id, fire_at) \
                     VALUES ('k', 'device:1', 1, 0);
                 DELETE FROM account WHERE id = 1;",
            )?;
            let mut counts = Vec::new();
            for table in [
                "caldav",
                "calendar_pref",
                "caldav_trust",
                "cal_op",
                "alarm_ack",
                "alarm_sent",
            ] {
                counts.push(
                    tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                );
            }
            let fallback: Option<i64> =
                tx.query_row("SELECT fallback_account_id FROM caldav", [], |r| r.get(0))?;
            Ok((counts, fallback))
        })
        .await
        .unwrap();
    assert_eq!(left, (vec![1, 0, 0, 0, 0, 0], None));
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
        "fallback_account_id",
        "target_account_id",
        "recurrence_id",
        "reply_op_id",
        "authserv_id",
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
            if !["account", "identity", "draft", "session"].contains(&target.as_str()) {
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
    assert_eq!((CONFIG_SCHEMA, CACHE_SCHEMA), (11, 8));
}

#[tokio::test]
async fn cache_v4_drops_has_cal_labels() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.db");
    {
        let conn = open_raw(&path);
        for sql in [
            include_str!("../migrations/cache/V1__init.sql"),
            include_str!("../migrations/cache/V2__pending_text_index.sql"),
            include_str!("../migrations/cache/V3__op_overlay.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO message (id, account_id, email_id, date) VALUES (1, 1, 'e1', 0); \
             INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES \
             (1, 1, '$has_cal', '$has_cal', 'keyword'), \
             (2, 2, '$HAS_CAL', '$has_cal', 'keyword'), \
             (3, 1, 'Work', 'work', 'keyword'), \
             (4, 1, '$label_=24has_cal', '$has_cal_label', 'keyword'); \
             INSERT INTO message_label (message_id, label_id) VALUES (1, 1), (1, 3), (1, 4);",
        )
        .unwrap();
    }
    let db = CacheDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CACHE_SCHEMA);
    let (labels, message_labels) = db
        .read(|c| {
            let ids = |sql: &str| -> rusqlite::Result<Vec<i64>> {
                c.prepare(sql)?.query_map([], |r| r.get(0))?.collect()
            };
            Ok((
                ids("SELECT id FROM label ORDER BY id")?,
                ids("SELECT label_id FROM message_label ORDER BY label_id")?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(labels, [3, 4]);
    assert_eq!(message_labels, [3, 4]);
    integrity(&db).await;
    db.close().await.unwrap();
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
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
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

#[tokio::test]
async fn config_v8_ties_push_subscriptions_to_their_session() {
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
            include_str!("../migrations/config/V7__draft_client_id.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 7).unwrap();
        conn.execute_batch(
            "INSERT INTO push_subscription (endpoint, p256dh, auth) \
             VALUES ('https://push.example/legacy', x'01', x'02');",
        )
        .unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let p256dh = vec![4u8; 65];
    let auth = vec![7u8; 16];
    let (inserted, preview, short_key) = db
        .write(move |c| {
            c.execute_batch(
                "INSERT INTO session (id_hash, expires_at) VALUES (x'aa', 0), (x'bb', 0);",
            )?;
            let mut insert = c.prepare(
                "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut inserted = 0;
            for (session, endpoint) in [
                (vec![0xaau8], "https://push.example/a1"),
                (vec![0xaau8], "https://push.example/a2"),
                (vec![0xbbu8], "https://push.example/b1"),
            ] {
                inserted += insert.execute(rusqlite::params![session, endpoint, p256dh, auth])?;
            }
            let short_key = insert
                .execute(rusqlite::params![
                    vec![0xbbu8],
                    "https://push.example/short",
                    &p256dh[..64],
                    auth
                ])
                .is_err();
            let preview: i64 = c.query_row(
                "SELECT preview FROM push_subscription WHERE endpoint = 'https://push.example/a1'",
                [],
                |r| r.get(0),
            )?;
            Ok((inserted, preview, short_key))
        })
        .await
        .unwrap();
    assert_eq!((inserted, preview, short_key), (3, 1, true));
    let orphan = db
        .write(|c| {
            Ok(c.execute(
                "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth) \
                 VALUES (x'cc', 'https://push.example/c', zeroblob(65), zeroblob(16))",
                [],
            )?)
        })
        .await;
    assert!(orphan.is_err(), "subscription without a session accepted");

    db.write(|c| Ok(c.execute("DELETE FROM session WHERE id_hash = x'aa'", [])?))
        .await
        .unwrap();
    let left: Vec<String> = db
        .read(|c| {
            let mut stmt = c.prepare("SELECT endpoint FROM push_subscription ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(left, ["https://push.example/b1"]);
    integrity(&db).await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn config_v10_keeps_existing_cal_ops() {
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
            include_str!("../migrations/config/V7__draft_client_id.sql"),
            include_str!("../migrations/config/V8__push_subscription.sql"),
            include_str!("../migrations/config/V9__caldav.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 9).unwrap();
        conn.execute_batch(
            "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
             smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
             VALUES (1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00'); \
             INSERT INTO caldav_trust (account_id, authserv_id) VALUES (1, 'mx.example.com'); \
             INSERT INTO cal_op (op_id, account_id, target_account_id, uid, kind, partstat, \
             reply_op_id, state) VALUES ('o1', 1, 1, 'u1', 'rsvp', 'ACCEPTED', 'o1#reply', 'failed');",
        )
        .unwrap();
    }
    let db = ConfigDb::open(&path).unwrap();
    assert_eq!(user_version(&db).await, CONFIG_SCHEMA);
    let row = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT c.state, c.reply_op_id, c.op_seq, c.reply_cancelled, c.attendee, \
                 t.authserv_id, t.suggestion FROM cal_op c, caldav_trust t WHERE c.op_id = 'o1'",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        row,
        (
            "failed".to_owned(),
            "o1#reply".to_owned(),
            0,
            false,
            None,
            "mx.example.com".to_owned(),
            None
        )
    );
    integrity(&db).await;
    db.close().await.unwrap();
}
