//! The replica routes over HTTP with real device tokens: snapshot
//! consistency and limits, row re-fetch, raw messages and the image route.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tagmail_api_core::replica_dto::{ReplicaRecord, SNAPSHOT_FORMAT};
use tagmail_api_server::{Config, Server};
use tagmail_core::{AccountId, EmailId};
use tagmail_store::{BlobKey, BlobStore, ChangeOp, DEFAULT_RAW_CAP, Entity, NewChange, changes};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";
const DAY: i64 = 86_400;
const RAW: &str = "From: ann@x.example\r\nSubject: Hi\r\n\r\nHello there\r\n";

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

    fn config(&self) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: self.data.path().to_owned(),
            master_key: self.keys.path().join("master.key"),
            init_key: true,
            insecure_plain_imap: false,
            max_sse_clients: 4,
            auth: Default::default(),
            web_dir: None,
            push: Default::default(),
            render_author_css: false,
        }
    }
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn client(headers: &[(&'static str, String)]) -> reqwest::Client {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(*name, HeaderValue::from_str(value).unwrap());
    }
    reqwest::Client::builder()
        .default_headers(map)
        .build()
        .unwrap()
}

/// The owner's browser session (cookie + CSRF header).
async fn owner(server: &Server) -> reqwest::Client {
    let token = server.auth().setup_token().unwrap();
    let resp = reqwest::Client::new()
        .post(url(server, "/api/auth/setup"))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let cookie = resp.headers()["set-cookie"].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    client(&[
        ("cookie", cookie),
        ("x-requested-with", "tagmail".to_owned()),
    ])
}

/// A device paired through the real code exchange.
async fn device(server: &Server, owner: &reqwest::Client, name: &str) -> reqwest::Client {
    let pairing: Value = owner
        .post(url(server, "/api/pairing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(url(server, "/api/pairing/exchange"))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "code": pairing["code"], "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let token: Value = resp.json().await.unwrap();
    client(&[(
        "authorization",
        format!("Bearer {}", token["token"].as_str().unwrap()),
    )])
}

fn key(email_id: &str) -> BlobKey {
    BlobKey::for_message(AccountId(1), &EmailId::parse(email_id).unwrap())
}

/// Two mailboxes; thread 1 merged into 2; message 10 recent, 11 old, 12
/// old but starred, all with text; 10 has its raw message cached.
async fn seed(server: &Server) {
    let (recent, old) = (now() - DAY, now() - 400 * DAY);
    server
        .store()
        .config()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) VALUES \
                 (1, 'Work', 'imap.x.example', 993, 'tls', 'smtp.x.example', 465, 'tls', 'me', \
                  x'00', x'00', 'me@x.example');
                 INSERT INTO identity (id, account_id, address, is_default) \
                 VALUES (3, 1, 'me@x.example', 1);
                 INSERT INTO label_registry (account_id, label_key, display_name) \
                 VALUES (1, 'work', 'Work');
                 INSERT INTO image_sender (address) VALUES ('news@x.example');",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    server
        .store()
        .cache()
        .write(move |tx| {
            tx.execute_batch(&format!(
                "INSERT INTO mailbox (id, account_id, name, special_use, uidvalidity, sync_state) VALUES
                     (1, 1, 'INBOX', NULL, 7, 'live'), (2, 1, 'Archive', '\\Archive', 8, 'live');
                 INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred) VALUES
                     (2, 1, 'hi', {recent}, 3, 1, 1);
                 INSERT INTO thread (id, account_id, subject_norm, merged_into) VALUES (1, 1, 'hi', 2);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, text_blob) VALUES
                     (10, 1, 'g:10', 2, {recent}, 'Hi', 'hello', '[{{\"email\":\"ann@x.example\"}}]', '{k10}'),
                     (11, 1, 'g:11', 2, {old}, 'Old', 'old', NULL, '{k11}'),
                     (12, 1, 'g:12', 2, {old}, 'Starred', 'star', NULL, '{k12}');
                 INSERT INTO location (mailbox_id, uid, message_id, flags, keywords) VALUES
                     (1, 1, 10, 0, '$label_work'), (2, 1, 11, 1, ''), (2, 2, 12, 3, '');
                 INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES (1, 1, '$label_work', 'work', 'keyword');
                 INSERT INTO message_label (message_id, label_id) VALUES (10, 1);",
                k10 = key("g:10").as_str(),
                k11 = key("g:11").as_str(),
                k12 = key("g:12").as_str(),
            ))?;
            Ok(())
        })
        .await
        .unwrap();
    let blobs = server.store().blobs().clone();
    tokio::task::spawn_blocking(move || {
        for id in ["g:10", "g:11", "g:12"] {
            blobs.put_text(&key(id), &format!("text of {id}")).unwrap();
        }
        let mut writer = blobs.raw_writer(key("g:10"), DEFAULT_RAW_CAP).unwrap();
        writer.write_chunk(RAW.as_bytes()).unwrap();
        writer.finish().unwrap();
    })
    .await
    .unwrap();
}

/// Enough messages that a snapshot outlasts the socket buffers.
async fn fill(server: &Server, n: i64) {
    let snippet = "x".repeat(1000);
    server
        .store()
        .cache()
        .write(move |tx| {
            let mut stmt = tx.prepare(
                "INSERT INTO message (id, account_id, email_id, thread_id, date, snippet) \
                 VALUES (?1, 1, ?2, 2, 0, ?3)",
            )?;
            for id in 1000..1000 + n {
                stmt.execute(tagmail_store::rusqlite::params![
                    id,
                    format!("g:{id}"),
                    snippet
                ])?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

fn parse(body: &[u8]) -> Vec<ReplicaRecord> {
    body.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

/// Checks the `end` line against the records before it.
fn assert_complete(records: &[ReplicaRecord]) {
    let Some(ReplicaRecord::End(end)) = records.last() else {
        panic!("no end record");
    };
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for r in records {
        if let Some(kind) = r.kind() {
            *counts.entry(kind.to_owned()).or_default() += 1;
        }
    }
    assert_eq!(end.counts, counts);
}

fn texts(records: &[ReplicaRecord]) -> BTreeMap<i64, String> {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    records
        .iter()
        .filter_map(|r| match r {
            ReplicaRecord::Text(t) => Some(t),
            _ => None,
        })
        .map(|t| {
            let zstd = base64::engine::general_purpose::STANDARD
                .decode(&t.zstd)
                .unwrap();
            let k = key(&format!("g:{}", t.message_id));
            store.put_text_zstd(&k, &zstd).unwrap();
            (t.message_id, store.read_text(&k).unwrap().unwrap())
        })
        .collect()
}

fn ids(records: &[ReplicaRecord], kind: &str) -> Vec<i64> {
    records
        .iter()
        .filter_map(|r| match (r, kind) {
            (ReplicaRecord::Thread(t), "thread") => Some(t.id),
            (ReplicaRecord::Message(m), "message") => Some(m.id),
            (ReplicaRecord::Mailbox(m), "mailbox") => Some(m.id),
            (ReplicaRecord::Label(l), "label") => Some(l.id),
            (ReplicaRecord::Account(a), "account") => Some(a.id),
            (ReplicaRecord::Identity(i), "identity") => Some(i.id),
            _ => None,
        })
        .collect()
}

async fn start() -> (Dirs, Server, reqwest::Client) {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    let owner = owner(&server).await;
    seed(&server).await;
    (dirs, server, owner)
}

#[tokio::test]
async fn snapshot_is_the_cache_at_its_cursor() {
    let (_dirs, server, owner) = start().await;
    let desktop = device(&server, &owner, "Laptop").await;
    let resp = desktop
        .get(url(&server, "/api/replica/snapshot"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/x-ndjson");
    let records = parse(&resp.bytes().await.unwrap());
    assert_complete(&records);

    let ReplicaRecord::Header(header) = &records[0] else {
        panic!("no header first: {:?}", records[0]);
    };
    assert_eq!(header.format, SNAPSHOT_FORMAT);
    assert_eq!(header.window_days, 90);
    assert_eq!((header.api.major, header.api.minor), (1, 3));
    assert!(header.cursor.starts_with(&format!("{}:", header.epoch)));
    let changes: Value = desktop
        .get(url(
            &server,
            &format!("/api/changes?since={}", header.cursor),
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changes["next"], header.cursor.as_str());
    assert_eq!(changes["changes"], json!([]));

    let kinds: Vec<&str> = records.iter().filter_map(ReplicaRecord::kind).collect();
    let mut order = kinds.clone();
    order.dedup();
    assert_eq!(
        order,
        [
            "account",
            "identity",
            "labelRegistry",
            "imageSender",
            "mailbox",
            "label",
            "thread",
            "message",
            "text"
        ]
    );
    assert_eq!(ids(&records, "thread"), [1, 2]);
    assert_eq!(ids(&records, "message"), [10, 11, 12]);
    let ten = records
        .iter()
        .find_map(|r| match r {
            ReplicaRecord::Message(m) if m.id == 10 => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(ten.labels, [1]);
    assert_eq!(ten.locations[0].keywords, "$label_work");
    assert_eq!(
        texts(&records),
        BTreeMap::from([
            (10, "text of g:10".to_owned()),
            (12, "text of g:12".to_owned())
        ]),
        "in-window and starred only"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn writes_during_a_snapshot_come_through_changes() {
    let (_dirs, server, owner) = start().await;
    fill(&server, 20_000).await;
    let desktop = device(&server, &owner, "Laptop").await;
    let mut resp = desktop
        .get(url(&server, "/api/replica/snapshot"))
        .send()
        .await
        .unwrap();
    let mut body = Vec::new();
    while !body.contains(&b'\n') {
        body.extend_from_slice(&resp.chunk().await.unwrap().unwrap());
    }
    let ReplicaRecord::Header(header) =
        &parse(&body[..=body.iter().position(|b| *b == b'\n').unwrap()])[0]
    else {
        panic!("no header first");
    };
    let cursor = header.cursor.clone();

    let late = 99_999_999;
    let late_key = key("g:99999999");
    let blobs = server.store().blobs().clone();
    let text_key = late_key.clone();
    tokio::task::spawn_blocking(move || blobs.put_text(&text_key, "late").unwrap())
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&body).contains(r#""t":"end""#),
        "the snapshot is still streaming"
    );
    let date = now();
    server
        .store()
        .cache()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO message (id, account_id, email_id, thread_id, date, text_blob) \
                 VALUES (?1, 1, 'g:99999999', 2, ?2, ?3)",
                tagmail_store::rusqlite::params![late, date, late_key.as_str()],
            )?;
            changes::append(
                tx,
                &NewChange {
                    entity: Entity::Message,
                    entity_id: late,
                    op: ChangeOp::Upsert,
                    payload: None,
                },
            )
        })
        .await
        .unwrap();
    server
        .feed()
        .publish(server.store().cache().head().await.unwrap());

    while let Some(chunk) = resp.chunk().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    let records = parse(&body);
    assert_complete(&records);
    assert_eq!(ids(&records, "message").len(), 3 + 20_000);
    assert!(!ids(&records, "message").contains(&late));
    assert!(!texts(&records).contains_key(&late));

    let changes: Value = desktop
        .get(url(&server, &format!("/api/changes?since={cursor}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changes["changes"][0]["entity"], "message");
    assert_eq!(changes["changes"][0]["id"], late);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_config_is_at_least_as_new_as_its_cursor() {
    let (_dirs, server, owner) = start().await;
    let desktop = device(&server, &owner, "Laptop").await;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let store = server.store().clone();
    let writer = tokio::spawn(async move {
        let mut n = 0;
        while !*stopped.borrow() {
            n += 1;
            store
                .config()
                .write(move |tx| {
                    tx.execute(
                        "UPDATE account SET name = ?1 WHERE id = 1",
                        [format!("v{n}")],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            store
                .cache()
                .write(move |tx| {
                    changes::append(
                        tx,
                        &NewChange {
                            entity: Entity::Account,
                            entity_id: 1,
                            op: ChangeOp::Upsert,
                            payload: Some(json!({ "n": n }).to_string()),
                        },
                    )
                })
                .await
                .unwrap();
        }
    });

    let mut snapshots = Vec::new();
    for _ in 0..100 {
        let body = desktop
            .get(url(&server, "/api/replica/snapshot"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let records = parse(&body);
        let ReplicaRecord::Header(header) = &records[0] else {
            panic!("no header first");
        };
        let name = records
            .iter()
            .find_map(|r| match r {
                ReplicaRecord::Account(a) => Some(a.name.clone()),
                _ => None,
            })
            .unwrap();
        let version: i64 = name.trim_start_matches('v').parse().unwrap_or(0);
        snapshots.push((header.cursor.clone(), version));
    }
    stop.send(true).unwrap();
    writer.await.unwrap();

    for (cursor, version) in snapshots {
        let changes: Value = desktop
            .get(url(
                &server,
                &format!("/api/changes?since={cursor}&limit=1"),
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(next) = changes["changes"][0]["payload"]["n"].as_i64() {
            assert!(
                version >= next - 1,
                "config v{version} misses change {} at or before {cursor}",
                next - 1
            );
        }
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshots_are_limited_per_device_and_overall() {
    let (_dirs, server, owner) = start().await;
    fill(&server, 20_000).await;
    let (a, b, c) = (
        device(&server, &owner, "A").await,
        device(&server, &owner, "B").await,
        device(&server, &owner, "C").await,
    );
    let snapshot =
        |client: &reqwest::Client| client.get(url(&server, "/api/replica/snapshot")).send();
    let held_a = snapshot(&a).await.unwrap();
    assert_eq!(held_a.status(), 200);
    let again = snapshot(&a).await.unwrap();
    assert_eq!(
        again.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "one per device"
    );
    let held_b = snapshot(&b).await.unwrap();
    assert_eq!(held_b.status(), 200);
    let third = snapshot(&c).await.unwrap();
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(third.headers()["retry-after"], "30");
    let body: Value = third.json().await.unwrap();
    assert_eq!(body["error"], "busy");

    let records = parse(&held_a.bytes().await.unwrap());
    assert_complete(&records);
    drop(held_b);
    let mut freed = None;
    for _ in 0..50 {
        let resp = snapshot(&c).await.unwrap();
        if resp.status() == 200 {
            freed = Some(resp);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_complete(&parse(&freed.expect("slot freed").bytes().await.unwrap()));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn replica_routes_refuse_browser_sessions() {
    let (_dirs, server, owner) = start().await;
    for resp in [
        owner
            .get(url(&server, "/api/replica/snapshot"))
            .send()
            .await,
        owner
            .post(url(&server, "/api/replica/rows"))
            .json(&json!({ "threads": [2] }))
            .send()
            .await,
        owner
            .get(url(&server, "/api/replica/messages/10/raw"))
            .send()
            .await,
        owner
            .post(url(&server, "/api/replica/image"))
            .json(&json!({ "url": "https://x.example/a.png" }))
            .send()
            .await,
    ] {
        let resp = resp.unwrap();
        assert_eq!(resp.status(), 403);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "device_only");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn rows_resend_changed_entities() {
    let (_dirs, server, owner) = start().await;
    let desktop = device(&server, &owner, "Laptop").await;
    let rows = |body: Value| {
        desktop
            .post(url(&server, "/api/replica/rows"))
            .json(&body)
            .send()
    };

    let resp = rows(json!({ "threads": [1], "messages": [10, 11], "labels": [1] }))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let records = parse(&resp.bytes().await.unwrap());
    assert_complete(&records);
    assert_eq!(
        ids(&records, "thread"),
        [1, 2],
        "the merge target comes along"
    );
    assert_eq!(ids(&records, "message"), [10, 11]);
    assert_eq!(ids(&records, "label"), [1]);
    assert_eq!(texts(&records).into_keys().collect::<Vec<_>>(), [10]);

    let records = parse(
        &rows(json!({ "mailbox": 2 }))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    );
    assert_complete(&records);
    assert_eq!(ids(&records, "thread"), [2]);
    assert_eq!(ids(&records, "message"), [11, 12]);
    assert_eq!(texts(&records).into_keys().collect::<Vec<_>>(), [12]);

    let records = parse(
        &rows(json!({ "mailbox": 2, "uids": [2, 2] }))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    );
    assert_complete(&records);
    assert_eq!(ids(&records, "thread"), [2]);
    assert_eq!(ids(&records, "message"), [12], "only the bulk's UID range");
    assert_eq!(texts(&records).into_keys().collect::<Vec<_>>(), [12]);
    let records = parse(
        &rows(json!({ "mailbox": 2, "uids": [3, 9] }))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    );
    assert!(ids(&records, "thread").is_empty());
    assert!(ids(&records, "message").is_empty());

    let records = parse(
        &rows(json!({ "accounts": [1], "identities": [3], "mailboxes": [2, 404] }))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    );
    assert_complete(&records);
    assert_eq!(ids(&records, "account"), [1]);
    assert_eq!(ids(&records, "identity"), [3]);
    assert_eq!(ids(&records, "mailbox"), [2], "missing ids are left out");

    let too_many = rows(json!({ "messages": (0..1001).collect::<Vec<i64>>() }))
        .await
        .unwrap();
    assert_eq!(too_many.status(), 422);
    let body: Value = too_many.json().await.unwrap();
    assert_eq!(body["field"], "messages");
    assert_eq!(
        rows(json!({ "thread": [1] })).await.unwrap().status(),
        400,
        "unknown fields are refused"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_messages_are_served_whole() {
    let (_dirs, server, owner) = start().await;
    let desktop = device(&server, &owner, "Laptop").await;
    let resp = desktop
        .get(url(&server, "/api/replica/messages/10/raw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["content-length"], RAW.len().to_string());
    assert_eq!(resp.bytes().await.unwrap(), RAW.as_bytes());

    let missing = desktop
        .get(url(&server, "/api/replica/messages/404/raw"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let uncached = desktop
        .get(url(&server, "/api/replica/messages/11/raw"))
        .send()
        .await
        .unwrap();
    assert_eq!(uncached.status(), 503, "no engine to fetch it from");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_image_route_keeps_the_proxy_ssrf_checks() {
    let (_dirs, server, owner) = start().await;
    let desktop = device(&server, &owner, "Laptop").await;
    for target in [
        "http://127.0.0.1/a.png",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/a.png",
        "http://10.0.0.1:80/a.png",
        "file:///etc/passwd",
    ] {
        let resp = desktop
            .post(url(&server, "/api/replica/image"))
            .json(&json!({ "url": target }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{target}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "blocked", "{target}");
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_images_are_rate_limited_per_device() {
    let (_dirs, server, owner) = start().await;
    let (a, b) = (
        device(&server, &owner, "A").await,
        device(&server, &owner, "B").await,
    );
    let fetch = |client: &reqwest::Client| {
        client
            .post(url(&server, "/api/replica/image"))
            .json(&json!({ "url": "http://127.0.0.1/a.png" }))
            .send()
    };
    let burst = tagmail_api_server::ratelimit::REPLICA_IMAGES.burst;
    for _ in 0..burst {
        assert_eq!(fetch(&a).await.unwrap().status(), 403);
    }
    let limited = fetch(&a).await.unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()["retry-after"], "1");
    let body: Value = limited.json().await.unwrap();
    assert_eq!(body["error"], "rate_limited");
    assert_eq!(
        fetch(&b).await.unwrap().status(),
        403,
        "other devices go on"
    );
    server.shutdown().await.unwrap();
}
