//! The replica against an in-process server: bootstrap parity, SSE-driven
//! pulls, re-bootstrap on an epoch change, the window sweep and new-mail
//! detection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_core::{EventSource, Store, View};
use tagmail_api_server::{CACHE_DB, Config, Server};
use tagmail_core::{AccountId, EmailId, LabelKey, MailboxRowId, ThreadId};
use tagmail_replica::sync::ReplicaEvent;
use tagmail_replica::{Durable, Layout, Replica, ServerClient, Settings};
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobKey, CacheDb, ChangeOp, DEFAULT_RAW_CAP, Entity, NewChange, changes};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, watch};
use url::Url;

const PASSWORD: &str = "correct horse battery";
const DAY: i64 = 86_400;
const RAW: &str = "From: ann@x.example\r\nSubject: Hi\r\n\r\nHello there\r\n";
const WAIT: Duration = Duration::from_secs(2);

struct Env {
    data: TempDir,
    keys: TempDir,
    desktop: TempDir,
    listen: std::net::SocketAddr,
}

impl Env {
    fn new() -> Env {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        Env {
            data: tempfile::tempdir().unwrap(),
            keys: tempfile::tempdir().unwrap(),
            desktop: tempfile::tempdir().unwrap(),
            listen: port,
        }
    }

    fn config(&self) -> Config {
        Config {
            listen: self.listen,
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

    fn layout(&self) -> Layout {
        Layout::new(self.desktop.path())
    }

    async fn start(&self) -> Server {
        Server::start(&self.config(), None).await.unwrap()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

/// A device token from the real pairing exchange.
async fn pair(server: &Server) -> String {
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
    let pairing: Value = reqwest::Client::new()
        .post(url(server, "/api/pairing"))
        .header("cookie", cookie)
        .header("x-requested-with", "tagmail")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let device: Value = reqwest::Client::new()
        .post(url(server, "/api/pairing/exchange"))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "code": pairing["code"], "name": "Laptop" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    device["token"].as_str().unwrap().to_owned()
}

fn key(email_id: &str) -> BlobKey {
    BlobKey::for_message(AccountId(1), &EmailId::parse(email_id).unwrap())
}

/// INBOX with threads 2 and 3, Archive with an old message; thread 1 is
/// merged into 2. Message 10 is recent (raw cached), 11 old, 12 old and
/// starred; all have text.
async fn seed(server: &Server) {
    seed_config(server).await;
    seed_cache(server).await;
}

async fn seed_config(server: &Server) {
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
                 VALUES (1, 'work', 'Work');",
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// The server's FTS rows, as the engine writes them.
const FTS: [(i64, &str, &str, &str); 4] = [
    (10, "Hi", "ann@x.example", "quick fox"),
    (11, "Old", "", "old news"),
    (12, "Starred", "", "starred gem"),
    (13, "Other", "", ""),
];

async fn seed_cache(server: &Server) {
    let (recent, old) = (now() - DAY, now() - 400 * DAY);
    server
        .store()
        .cache()
        .write(move |tx| {
            for (id, subject, from, body) in FTS {
                tagmail_store::fts::index(
                    tx,
                    tagmail_core::MessageRowId(id),
                    &tagmail_store::FtsDocument {
                        subject,
                        from,
                        to: "",
                        body,
                    },
                )?;
            }
            tx.execute_batch(&format!(
                "INSERT INTO mailbox (id, account_id, name, special_use, uidvalidity, sync_state) VALUES
                     (1, 1, 'INBOX', NULL, 7, 'live'), (2, 1, 'Archive', '\\Archive', 8, 'live');
                 INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred) VALUES
                     (2, 1, 'hi', {recent}, 3, 1, 1), (3, 1, 'other', {old}, 1, 0, 0);
                 INSERT INTO thread (id, account_id, subject_norm, merged_into) VALUES (1, 1, 'hi', 2);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, text_blob) VALUES
                     (10, 1, 'g:10', 2, {recent}, 'Hi', 'hello', '[{{\"email\":\"ann@x.example\"}}]', '{k10}'),
                     (11, 1, 'g:11', 2, {old}, 'Old', 'old', NULL, '{k11}'),
                     (12, 1, 'g:12', 2, {old}, 'Starred', 'star', NULL, '{k12}'),
                     (13, 1, 'g:13', 3, {old}, 'Other', 'other', NULL, NULL);
                 INSERT INTO location (mailbox_id, uid, message_id, flags, keywords) VALUES
                     (1, 1, 10, 0, '$label_work'), (2, 1, 11, 1, ''), (2, 2, 12, 3, ''), (1, 2, 13, 1, '');
                 INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES (1, 1, '$label_work', 'work', 'keyword');
                 INSERT INTO message_label (message_id, label_id) VALUES (10, 1);
                 INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES
                     (2, 1, {recent}), (2, 2, {old}), (3, 1, {old});",
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
        for (id, text) in [
            ("g:10", "quick fox"),
            ("g:11", "old news"),
            ("g:12", "starred gem"),
        ] {
            blobs.put_text(&key(id), text).unwrap();
        }
        let mut writer = blobs.raw_writer(key("g:10"), DEFAULT_RAW_CAP).unwrap();
        writer.write_chunk(RAW.as_bytes()).unwrap();
        writer.finish().unwrap();
    })
    .await
    .unwrap();
}

/// Adds message `id` in a new thread `id` to `mailbox` like the engine
/// would, logging `changes`, and wakes the server's event streams.
async fn arrive(
    server: &Server,
    id: i64,
    mailbox: i64,
    flags: i64,
    from: &str,
    log: Vec<NewChange>,
) {
    let date = now();
    let from = format!(r#"[{{"email":"{from}"}}]"#);
    server
        .store()
        .cache()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count) \
                 VALUES (?1, 1, 'new', ?2, 1, 1)",
                params![id, date],
            )?;
            tx.execute(
                "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, from_json) \
                 VALUES (?1, 1, ?2, ?1, ?3, 'New mail', ?4)",
                params![id, format!("g:{id}"), date, from],
            )?;
            tx.execute(
                "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (?1, ?2, ?2, ?3)",
                params![mailbox, id, flags],
            )?;
            tx.execute(
                "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, ?2, ?3)",
                params![id, mailbox, date],
            )?;
            changes::append_all(tx, &log)?;
            Ok(())
        })
        .await
        .unwrap();
    server
        .feed()
        .publish(server.store().cache().head().await.unwrap());
}

fn upserts(id: i64) -> Vec<NewChange> {
    [Entity::Thread, Entity::Message]
        .into_iter()
        .map(|entity| NewChange {
            entity,
            entity_id: id,
            op: ChangeOp::Upsert,
            payload: None,
        })
        .collect()
}

async fn replica(env: &Env, server: &Server, token: &str) -> Arc<Replica> {
    let client = ServerClient::new(
        &Url::parse(&format!("http://{}/", server.local_addr())).unwrap(),
        Some(token.to_owned()),
    )
    .unwrap();
    Replica::open(env.layout(), client, Settings::default())
        .await
        .unwrap()
}

async fn same_views(server: &Server, replica: &Replica) {
    let local = replica.store().unwrap();
    let views = [
        View::Inbox,
        View::Mailbox(MailboxRowId(2)),
        View::Label(LabelKey::from_stored("work".into())),
        View::Account(AccountId(1), Box::new(View::Inbox)),
    ];
    for view in views {
        let theirs = server.store().view_index(view.clone()).await.unwrap().ids;
        let ours = local.view_index(view.clone()).await.unwrap().ids;
        assert_eq!(ours, theirs, "{view:?}");
        let theirs = server.store().threads(theirs).await.unwrap().rows;
        let ours = local.threads(ours).await.unwrap().rows;
        assert_eq!(ours, theirs, "{view:?}");
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + WAIT;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn new_mail(events: &mut broadcast::Receiver<ReplicaEvent>) -> Vec<i64> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ReplicaEvent::NewMail(arrivals) = event {
            out.extend(arrivals.iter().map(|a| a.message_id));
        }
    }
    out
}

async fn local_value<T: tagmail_store::rusqlite::types::FromSql + Send + 'static>(
    replica: &Replica,
    sql: &'static str,
) -> T {
    replica
        .store()
        .unwrap()
        .cache()
        .read(move |conn| Ok(conn.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn bootstrap_equals_the_server() {
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let replica = replica(&env, &server, &token).await;

    replica.rebootstrap().await.unwrap();

    same_views(&server, &replica).await;
    assert_eq!(
        replica.server_cursor().unwrap(),
        server.store().cache().head().await.unwrap()
    );
    let texts: i64 = local_value(
        &replica,
        "SELECT count(*) FROM message WHERE text_blob IS NOT NULL",
    )
    .await;
    assert_eq!(texts, 2, "in-window 10 and starred 12");
    let (store, local) = (server.store(), replica.store().unwrap());
    for q in ["fox", "gem", "from:ann", "other"] {
        let query = || View::Search(tagmail_core::query::parse(q).unwrap());
        let theirs = store.view_index(query()).await.unwrap().ids;
        let ours = local.view_index(query()).await.unwrap().ids;
        assert_eq!(ours, theirs, "{q}");
    }
    let news = || View::Search(tagmail_core::query::parse("news").unwrap());
    assert_eq!(
        local.view_index(news()).await.unwrap().ids,
        Vec::<ThreadId>::new(),
        "out-of-window text is not on the replica"
    );
    replica.sync_once().await.unwrap();
    eventually("raw prefetch of message 10", || async {
        local_value::<Option<String>>(&replica, "SELECT raw_blob FROM message WHERE id = 10")
            .await
            .is_some()
    })
    .await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sse_pulls_new_mail_within_two_seconds() {
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let replica = replica(&env, &server, &token).await;
    let mut events = replica.subscribe();
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(Arc::clone(&replica).run(stopped));
    eventually("live", || async {
        *replica.phase().borrow() == tagmail_replica::Phase::Live
    })
    .await;
    assert!(
        new_mail(&mut events).is_empty(),
        "bootstrap is not new mail"
    );

    arrive(&server, 50, 1, 0, "bob@x.example", upserts(50)).await;
    eventually("thread 50 in the replica inbox", || async {
        let store = replica.store().unwrap();
        store
            .view_index(View::Inbox)
            .await
            .unwrap()
            .ids
            .contains(&ThreadId(50))
    })
    .await;
    let deadline = Instant::now() + WAIT;
    let mut arrived = Vec::new();
    while arrived.is_empty() && Instant::now() < deadline {
        arrived = new_mail(&mut events);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(arrived, vec![50]);

    arrive(&server, 51, 1, 0, "me@x.example", upserts(51)).await;
    arrive(&server, 52, 1, 1, "bob@x.example", upserts(52)).await;
    arrive(&server, 53, 2, 0, "bob@x.example", upserts(53)).await;
    let bulk = vec![NewChange {
        entity: Entity::Mailbox,
        entity_id: 1,
        op: ChangeOp::Bulk { mailbox: 1 },
        payload: None,
    }];
    arrive(&server, 54, 1, 0, "bob@x.example", bulk).await;
    eventually("thread 54 via the bulk refresh", || async {
        let store = replica.store().unwrap();
        store
            .view_index(View::Inbox)
            .await
            .unwrap()
            .ids
            .contains(&ThreadId(54))
    })
    .await;
    assert_eq!(
        new_mail(&mut events),
        Vec::<i64>::new(),
        "own, seen, non-INBOX and bulk mail are not new"
    );
    same_views(&server, &replica).await;

    stop.send(true).unwrap();
    task.await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_epoch_change_bootstraps_again_and_keeps_durable_intents() {
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let durable = Durable::open(&env.layout().durable()).unwrap();
    durable
        .conn()
        .execute(
            "INSERT INTO intent (op_id, seq, kind, account_id, email_ids, created_at, state) \
             VALUES ('op-1', 1, 'archive', 1, '[\"g:999\"]', 0, 'pending')",
            [],
        )
        .unwrap();
    let replica = replica(&env, &server, &token).await;
    replica.rebootstrap().await.unwrap();
    let before = replica.server_cursor().unwrap();
    let local_before = replica.feed().subscribe().borrow().epoch;

    server.shutdown().await.unwrap();
    CacheDb::wipe(&env.data.path().join(CACHE_DB)).unwrap();
    let server = env.start().await;
    seed_cache(&server).await;

    let err = replica.sync_once().await.unwrap_err();
    assert!(
        matches!(
            err,
            tagmail_replica::Error::Client(tagmail_replica::ClientError::Gone)
        ),
        "{err:?}"
    );
    replica.rebootstrap().await.unwrap();

    let after = replica.server_cursor().unwrap();
    assert_ne!(after.epoch, before.epoch);
    assert_ne!(replica.feed().subscribe().borrow().epoch, local_before);
    same_views(&server, &replica).await;
    let intents: i64 = durable
        .conn()
        .query_row("SELECT count(*) FROM intent", [], |r| r.get(0))
        .unwrap();
    assert_eq!(intents, 1);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_window_sweep_drops_text_but_keeps_rows() {
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let replica = replica(&env, &server, &token).await;
    replica.rebootstrap().await.unwrap();
    let search = |q: &'static str| {
        let store = replica.store().unwrap();
        async move {
            store
                .view_index(View::Search(tagmail_core::query::parse(q).unwrap()))
                .await
                .unwrap()
                .ids
        }
    };
    assert_eq!(search("fox").await, vec![ThreadId(2)]);

    replica.set_settings(Settings {
        window_days: 0,
        ..Settings::default()
    });
    replica.sync_once().await.unwrap();

    let texts: Vec<i64> = replica
        .store()
        .unwrap()
        .cache()
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM message WHERE text_blob IS NOT NULL")?;
            Ok(stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(texts, vec![12], "the starred message keeps its text");
    let rows: i64 = local_value(&replica, "SELECT count(*) FROM message").await;
    assert_eq!(rows, 4);
    assert_eq!(search("fox").await, Vec::<ThreadId>::new());
    assert_eq!(search("gem").await, vec![ThreadId(2)]);
    assert_eq!(
        search("Hi").await,
        vec![ThreadId(2)],
        "headers stay searchable"
    );
    server.shutdown().await.unwrap();
}

/// A TCP proxy to `server` counting the bytes it sends back.
async fn counting_proxy(server: &Server) -> (std::net::SocketAddr, Arc<AtomicU64>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = server.local_addr();
    let received = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&received);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
                });
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = server_read.read(&mut buf).await {
                    if n == 0 || client_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    counter.fetch_add(n as u64, Ordering::Relaxed);
                }
            });
        }
    });
    (addr, received)
}

/// One initial-sync window as the engine writes it: `count` new messages
/// in INBOX below `top`, one thread each, and a ranged bulk entry. They are
/// older than the window, so no raw prefetch adds to the traffic.
async fn bulk_window(server: &Server, top: i64, count: i64) {
    let date = now() - 400 * DAY;
    let lo = top - count + 1;
    server
        .store()
        .cache()
        .write(move |tx| {
            for id in lo..=top {
                tx.execute(
                    "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count) \
                     VALUES (?1, 1, 'window', ?2, 1, 0)",
                    params![id, date],
                )?;
                tx.execute(
                    "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet) \
                     VALUES (?1, 1, ?2, ?1, ?3, 'Window', ?4)",
                    params![id, format!("g:{id}"), date, "s".repeat(500)],
                )?;
                tx.execute(
                    "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (1, ?1, ?1, 1)",
                    [id],
                )?;
                tx.execute(
                    "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, 1, ?2)",
                    params![id, date],
                )?;
            }
            changes::append_all(
                tx,
                &[NewChange {
                    entity: Entity::Mailbox,
                    entity_id: 1,
                    op: ChangeOp::Bulk { mailbox: 1 },
                    payload: Some(json!({ "uids": [lo, top] }).to_string()),
                }],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    server
        .feed()
        .publish(server.store().cache().head().await.unwrap());
}

#[tokio::test]
async fn bulk_windows_transfer_only_what_they_added() {
    const WINDOWS: i64 = 8;
    const SIZE: i64 = 50;
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let (proxy, received) = counting_proxy(&server).await;
    let client = ServerClient::new(
        &Url::parse(&format!("http://{proxy}/")).unwrap(),
        Some(token),
    )
    .unwrap();
    let replica = Replica::open(env.layout(), client, Settings::default())
        .await
        .unwrap();
    replica.rebootstrap().await.unwrap();

    let mut per_window = Vec::new();
    for w in 0..WINDOWS {
        bulk_window(&server, 10_000 - w * SIZE, SIZE).await;
        received.store(0, Ordering::Relaxed);
        replica.sync_once().await.unwrap();
        per_window.push(received.load(Ordering::Relaxed));
    }
    assert!(
        per_window[WINDOWS as usize - 1] < 2 * per_window[0],
        "bytes per window grow with the mailbox: {per_window:?}"
    );
    same_views(&server, &replica).await;
    server.shutdown().await.unwrap();
}

/// A proxy to `upstream` that never forwards a snapshot request once
/// `hold` is set, and reports that one arrived.
async fn snapshot_trap(
    upstream: std::net::SocketAddr,
) -> (std::net::SocketAddr, Arc<AtomicBool>, Arc<AtomicBool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hold = Arc::new(AtomicBool::new(false));
    let trapped = Arc::new(AtomicBool::new(false));
    let (h, t) = (Arc::clone(&hold), Arc::clone(&trapped));
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let (hold, trapped) = (Arc::clone(&h), Arc::clone(&t));
            tokio::spawn(async move {
                let Ok(server) = tokio::net::TcpStream::connect(upstream).await else {
                    return;
                };
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                });
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = client_read.read(&mut buf).await {
                    let snapshot = buf[..n]
                        .windows(b"/api/replica/snapshot".len())
                        .any(|w| w == b"/api/replica/snapshot");
                    if snapshot && hold.load(Ordering::Acquire) {
                        trapped.store(true, Ordering::Release);
                        std::future::pending::<()>().await;
                    }
                    if n == 0 || server_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (addr, hold, trapped)
}

#[tokio::test]
async fn stopping_does_not_wait_for_a_rebootstrap() {
    let env = Env::new();
    let server = env.start().await;
    seed(&server).await;
    let token = pair(&server).await;
    let (proxy, hold, trapped) = snapshot_trap(env.listen).await;
    let client = ServerClient::new(
        &Url::parse(&format!("http://{proxy}/")).unwrap(),
        Some(token),
    )
    .unwrap();
    let replica = Replica::open(env.layout(), client, Settings::default())
        .await
        .unwrap();
    replica.rebootstrap().await.unwrap();

    server.shutdown().await.unwrap();
    CacheDb::wipe(&env.data.path().join(CACHE_DB)).unwrap();
    let server = env.start().await;
    seed_cache(&server).await;
    hold.store(true, Ordering::Release);

    let (stop, stopped) = watch::channel(false);
    let run = tokio::spawn(Arc::clone(&replica).run(stopped));
    eventually("the re-bootstrap starts", || {
        let trapped = Arc::clone(&trapped);
        async move { trapped.load(Ordering::Acquire) }
    })
    .await;
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("run stops promptly")
        .unwrap();
    server.shutdown().await.unwrap();
}
