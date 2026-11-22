use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tagmail_api_core::{AccountRegistry, RuntimeStatus, id_index};
use tagmail_api_server::{CACHE_DB, CONFIG_DB, Config, Server};
use tagmail_core::{AccountId, ChangeCursor};
use tagmail_store::{CacheDb, ChangeOp, Entity, NewChange, changes};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Dirs {
    data: TempDir,
    keys: TempDir,
}

impl Dirs {
    fn new() -> Dirs {
        Dirs {
            data: tempfile::tempdir().unwrap(),
            keys: tempfile::tempdir().unwrap(),
        }
    }

    fn config(&self, key: &Path) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: self.data.path().to_owned(),
            master_key: key.to_owned(),
            init_key: true,
            insecure_no_auth: false,
            insecure_plain_imap: false,
            max_sse_clients: 2,
        }
    }

    fn key(&self) -> PathBuf {
        self.keys.path().join("master.key")
    }
}

#[derive(Default)]
struct Registry {
    added: Mutex<Vec<AccountId>>,
}

impl AccountRegistry for Registry {
    fn account_added(&self, id: AccountId) {
        self.added.lock().unwrap().push(id);
    }

    fn runtime_status(&self, _: AccountId) -> Option<RuntimeStatus> {
        None
    }
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

async fn append(server: &Server, thread: i64) -> ChangeCursor {
    let cache = server.store().cache();
    cache
        .write(move |tx| {
            changes::append(
                tx,
                &NewChange {
                    entity: Entity::Thread,
                    entity_id: thread,
                    op: ChangeOp::Upsert,
                    payload: Some(format!(r#"{{"lastDate":{thread}}}"#)),
                },
            )
        })
        .await
        .unwrap();
    let head = cache.head().await.unwrap();
    server.feed().publish(head);
    head
}

/// Reads SSE frames until one with an `event:` line arrives.
struct SseReader {
    resp: reqwest::Response,
    buf: String,
}

#[derive(Debug, PartialEq)]
struct Frame {
    event: String,
    id: Option<String>,
    data: Value,
}

impl SseReader {
    async fn next(&mut self) -> Frame {
        loop {
            while let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut frame = Frame {
                    event: String::new(),
                    id: None,
                    data: Value::Null,
                };
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        frame.event = v.to_owned();
                    } else if let Some(v) = line.strip_prefix("id: ") {
                        frame.id = Some(v.to_owned());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        frame.data = serde_json::from_str(v).unwrap();
                    }
                }
                if !frame.event.is_empty() {
                    return frame;
                }
            }
            let chunk = tokio::time::timeout(TIMEOUT, self.resp.chunk())
                .await
                .expect("timed out waiting for an event")
                .unwrap()
                .expect("stream ended");
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}

async fn open_events(server: &Server, last_event_id: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(url(server, "/api/events"));
    if let Some(id) = last_event_id {
        req = req.header("last-event-id", id);
    }
    req.send().await.unwrap()
}

async fn sse(server: &Server, last_event_id: Option<&str>) -> SseReader {
    let resp = open_events(server, last_event_id).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    SseReader {
        resp,
        buf: String::new(),
    }
}

fn seqs(frame: &Frame) -> Vec<i64> {
    frame.data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["seq"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn sse_resumes_from_last_event_id_and_410s_after_cache_wipe() {
    let dirs = Dirs::new();
    let config = dirs.config(&dirs.key());
    let server = Server::start(&config, None).await.unwrap();
    let epoch = server.store().cache().epoch();

    let mut events = sse(&server, None).await;
    let first = append(&server, 1).await;
    let frame = events.next().await;
    assert_eq!(frame.event, "changes");
    assert_eq!(frame.id.as_deref(), Some(format!("{epoch}:1").as_str()));
    assert_eq!(frame.id.as_deref(), Some(first.to_string().as_str()));
    assert_eq!(seqs(&frame), [1]);
    assert_eq!(
        frame.data["changes"][0]["payload"],
        json!({ "lastDate": 1 })
    );
    drop(events);

    append(&server, 2).await;
    append(&server, 3).await;
    let mut events = sse(&server, Some(&first.to_string())).await;
    let frame = events.next().await;
    assert_eq!(seqs(&frame), [2, 3], "missed changes replayed on resume");
    assert_eq!(frame.id.as_deref(), Some(format!("{epoch}:3").as_str()));

    append(&server, 4).await;
    assert_eq!(seqs(&events.next().await), [4]);
    drop(events);

    let changes: Value = reqwest::get(url(&server, &format!("/api/changes?since={first}")))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changes["next"], format!("{epoch}:4"));

    server.shutdown().await.unwrap();
    CacheDb::wipe(&dirs.data.path().join(CACHE_DB)).unwrap();
    let server = Server::start(&config, None).await.unwrap();
    assert_ne!(server.store().cache().epoch(), epoch);

    let resp = open_events(&server, Some(&first.to_string())).await;
    assert_eq!(resp.status(), 410);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "gone");
    let resp = reqwest::get(url(&server, &format!("/api/changes?since={first}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sse_sends_reset_on_epoch_change_and_caps_clients() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(&dirs.key()), None)
        .await
        .unwrap();

    let mut a = sse(&server, None).await;
    let _b = sse(&server, None).await;
    let over = open_events(&server, None).await;
    assert_eq!(over.status(), 503);

    let head = server.store().cache().head().await.unwrap();
    server.feed().publish(ChangeCursor {
        epoch: tagmail_core::CacheEpoch(head.epoch.0.wrapping_add(1)),
        seq: 0,
    });
    let frame = a.next().await;
    assert_eq!(frame.event, "reset");
    assert_eq!(frame.id, None);
    assert_eq!(frame.data["error"], "gone");
    let end = tokio::time::timeout(TIMEOUT, a.resp.chunk()).await.unwrap();
    assert!(matches!(end, Ok(None)), "stream closes after reset");
    drop(a);

    // The closed stream's slot is free again.
    let resp = tokio::time::timeout(TIMEOUT, async {
        loop {
            let resp = open_events(&server, None).await;
            if resp.status() != 503 {
                return resp;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp);
    drop(_b);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reads_the_inbox_index_and_rows_from_cache_db() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(&dirs.key()), None)
        .await
        .unwrap();
    server
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO mailbox (id, account_id, name, uidvalidity, sync_state) VALUES
                     (1, 1, 'INBOX', 1, 'live'), (2, 1, 'Archive', 1, 'initial');
                 INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred) VALUES
                     (10, 1, 'hello', 100, 2, 1, 1), (11, 1, 'later', 300, 1, 0, 0),
                     (12, 1, 'same', 100, 1, 0, 0), (9007199254740991, 1, 'big', 50, 1, 0, 0);
                 INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES
                     (10, 1, 100), (11, 1, 300), (12, 1, 100), (9007199254740991, 1, 50), (10, 2, 100);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, has_attachment) VALUES
                     (1, 1, 'a', 10, 90, 'Hello', 'first', '[{\"name\":\"Ann\",\"email\":\"ann@x\"}]', 0),
                     (2, 1, 'b', 10, 100, 'Re: Hello', 'second', '[{\"email\":\"bob@x\"},{\"email\":\"ANN@x\"}]', 1);
                 INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES (1, 1, '$label_work', 'work', 'keyword');
                 INSERT INTO message_label (message_id, label_id) VALUES (2, 1);",
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let resp = reqwest::get(url(&server, "/api/views/inbox/index"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cursor = resp.headers()["x-tagmail-cursor"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(cursor, format!("{}:0", server.store().cache().epoch()));
    let ids = id_index::decode(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(ids, [11, 10, 12, 9_007_199_254_740_991]);

    let resp = reqwest::get(url(&server, "/api/views/mailbox%3A2/index"))
        .await
        .unwrap();
    assert_eq!(
        id_index::decode(&resp.bytes().await.unwrap()).unwrap(),
        [10]
    );

    let body: Value = reqwest::get(url(&server, "/api/threads?ids=10,404,11"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let threads = body["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    let t = &threads[0];
    assert_eq!(t["id"], 10);
    assert_eq!(t["subject"], "Re: Hello");
    assert_eq!(t["snippet"], "second");
    assert_eq!(t["messageCount"], 2);
    assert_eq!(t["unreadCount"], 1);
    assert_eq!(t["starred"], true);
    assert_eq!(t["hasAttachment"], true);
    assert_eq!(
        t["senders"],
        json!([{ "name": "Ann", "email": "ann@x" }, { "name": null, "email": "bob@x" }])
    );
    assert_eq!(t["labels"], json!(["work"]));
    assert_eq!(threads[1]["subject"], "later");

    let status: Value = reqwest::get(url(&server, "/api/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["accounts"], json!([]));
    let version: Value = reqwest::get(url(&server, "/api/version"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(version["api"]["major"], 1);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn search_label_views_mailboxes_and_labels() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(&dirs.key()), None)
        .await
        .unwrap();
    server
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO mailbox (id, account_id, name, delimiter, special_use, uidvalidity) VALUES
                     (1, 1, 'INBOX', '/', NULL, 1), (2, 1, 'Trash', '/', '\\Trash', 1),
                     (3, 1, 'Entw&APw-rfe', '/', '\\Drafts', 1);
                 INSERT INTO thread (id, account_id, last_date) VALUES (10, 1, 100), (11, 1, 300), (12, 1, 200);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject) VALUES
                     (1, 1, 'a', 10, 1767225600, 'Invoice 2026/01'), (2, 1, 'b', 11, 1767225600, 'Invoice'),
                     (3, 1, 'c', 12, 1700000000, 'Invoice');
                 INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (1, 1, 1, 0), (2, 1, 2, 0), (1, 2, 3, 1);
                 INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES (1, 1, 'Work', 'work', 'keyword');
                 INSERT INTO message_label (message_id, label_id) VALUES (1, 1), (2, 1), (3, 1);",
            )?;
            for (id, subject) in [(1, "Invoice 2026/01"), (2, "Invoice"), (3, "Invoice")] {
                tagmail_store::fts::index(
                    tx,
                    tagmail_core::MessageRowId(id),
                    &tagmail_store::FtsDocument {
                        subject,
                        ..Default::default()
                    },
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    server
        .store()
        .config()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, smtp_port,
                     smtp_tls, username, secret_ct, secret_nonce) VALUES
                     (1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00');
                 INSERT INTO label_registry (account_id, label_key, display_name, color) VALUES
                     (1, 'travel', 'Travel', '#0a0');",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let index = |view: &'static str| {
        let u = url(&server, &format!("/api/views/{view}/index"));
        async move {
            let resp = reqwest::get(u).await.unwrap();
            let status = resp.status().as_u16();
            let body = resp.bytes().await.unwrap();
            (status, id_index::decode(&body).unwrap_or_default())
        }
    };
    assert_eq!(index("search%3Ainvoice").await, (200, vec![12, 10]));
    assert_eq!(
        index("search%3Ainvoice%20in%3Aanywhere").await,
        (200, vec![11, 12, 10])
    );
    assert_eq!(
        index("search%3Ainvoice%20after%3A2026%2F01%2F01").await,
        (200, vec![10])
    );
    assert_eq!(index("search%3A%222026%2F01%22").await, (200, vec![10]));
    assert_eq!(index("search%3Ais%3Aread").await, (200, vec![12]));
    assert_eq!(index("label%3AWORK").await, (200, vec![12, 10]));
    assert_eq!(index("search%3A%28").await.0, 400);

    let mailboxes: Value = reqwest::get(url(&server, "/api/mailboxes"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mailboxes[2]["name"], "Entwürfe");
    assert_eq!(mailboxes[2]["specialUse"], "drafts");
    assert_eq!(mailboxes[0]["specialUse"], Value::Null);
    let labels: Value = reqwest::get(url(&server, "/api/labels"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        labels,
        json!([
            { "key": "travel", "name": "Travel", "color": "#0a0" },
            { "key": "work", "name": "Work", "color": null },
        ])
    );
    server.shutdown().await.unwrap();
}

fn new_account() -> Value {
    json!({
        "name": "Work",
        "imapHost": "imap.example.com",
        "imapPort": 993,
        "imapTls": "tls",
        "smtpHost": "smtp.example.com",
        "smtpPort": 465,
        "smtpTls": "tls",
        "username": "me@example.com",
        "password": "s3cret-imap-password",
    })
}

#[tokio::test]
async fn accounts_store_encrypted_credentials() {
    let dirs = Dirs::new();
    let config = dirs.config(&dirs.key());
    let registry = Arc::new(Registry::default());
    let server = Server::start(&config, Some(registry.clone() as Arc<dyn AccountRegistry>))
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(url(&server, "/api/accounts"))
        .json(&new_account())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 1);
    assert_eq!(*registry.added.lock().unwrap(), [AccountId(1)]);

    let status: Value = client
        .get(url(&server, "/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["accounts"][0]["phase"], "pending");
    assert_eq!(status["accounts"][0]["name"], "Work");
    assert_eq!(status["accounts"][0]["textPending"], 0);

    server
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO message (account_id, email_id, date, text_blob, tombstoned_at) VALUES
                     (1, 'a', 1, NULL, NULL), (1, 'b', 2, NULL, NULL),
                     (1, 'c', 3, 'text/c', NULL), (1, 'd', 4, NULL, 5), (2, 'e', 5, NULL, NULL);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let status: Value = client
        .get(url(&server, "/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["accounts"][0]["textPending"], 2);

    let secret = server.store().account_secret(AccountId(1)).await.unwrap();
    assert_eq!(secret.expose(), "s3cret-imap-password");
    server.shutdown().await.unwrap();

    let config_db = std::fs::read(dirs.data.path().join(CONFIG_DB)).unwrap();
    let wal = std::fs::read(dirs.data.path().join(format!("{CONFIG_DB}-wal"))).unwrap_or_default();
    for bytes in [&config_db, &wal] {
        assert!(
            !bytes.windows(b"s3cret".len()).any(|w| w == b"s3cret"),
            "plaintext password on disk"
        );
    }

    // Restart with the same key: the secret still decrypts.
    let server = Server::start(&config, None).await.unwrap();
    let secret = server.store().account_secret(AccountId(1)).await.unwrap();
    assert_eq!(secret.expose(), "s3cret-imap-password");
    let resp = client
        .post(url(&server, "/api/accounts"))
        .json(&new_account())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "no engine, no new accounts");
    server.shutdown().await.unwrap();

    // Another key must not open this config.db.
    let other = dirs.keys.path().join("other.key");
    let err = Server::start(&dirs.config(&other), None)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("does not match"), "{err}");
}

#[tokio::test]
async fn refuses_to_start_without_a_key_or_on_a_public_address() {
    let dirs = Dirs::new();
    let mut config = dirs.config(&dirs.key());
    config.init_key = false;
    let err = Server::start(&config, None).await.err().unwrap();
    assert!(err.to_string().contains("--init-key"), "{err}");
    assert!(!dirs.key().exists());

    let mut config = dirs.config(&dirs.key());
    config.listen = "0.0.0.0:0".parse().unwrap();
    let err = Server::start(&config, None).await.err().unwrap();
    assert!(err.to_string().contains("--insecure-no-auth"), "{err}");
}

#[tokio::test]
async fn engine_syncs_added_accounts_and_reports_errors() {
    let dirs = Dirs::new();
    let config = dirs.config(&dirs.key());
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let server = Server::start_with_engine(&config, tagmail_engine::EngineConfig::default())
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let mut account = new_account();
    account["imapHost"] = json!("127.0.0.1");
    account["imapPort"] = json!(port);
    let resp = client
        .post(url(&server, "/api/accounts"))
        .json(&account)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let status: Value = client
            .get(url(&server, "/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["accounts"][0]["lastError"].is_string() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no sync error reported: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.shutdown().await.unwrap();
    assert!(dirs.data.path().join("blobs").is_dir());

    let restarted = Server::start_with_engine(&config, tagmail_engine::EngineConfig::default())
        .await
        .unwrap();
    restarted.shutdown().await.unwrap();
}
