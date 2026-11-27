//! Web Push end to end: subscription routes, the dispatcher and a mock push
//! service on loopback that decrypts what it receives.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::SecretKey;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::Generate;
use serde_json::{Value, json};
use tagmail_api_core::Arrival;
use tagmail_api_core::dto::AddressDto;
use tagmail_api_server::auth::DEFAULT_SESSION_IDLE;
use tagmail_api_server::outbound::{EndpointPolicy, Resolver};
use tagmail_api_server::push::dispatch::BATCH_DELAY;
use tagmail_api_server::push::ece;
use tagmail_api_server::push::store::{MAX_FAILURES, MAX_SUBSCRIPTIONS};
use tagmail_api_server::push::{PUSH_WRITES, PushConfig};
use tagmail_api_server::{Config, Server};
use tagmail_store::rusqlite::params;
use tempfile::TempDir;
use tokio::sync::mpsc;

const PASSWORD: &str = "correct horse battery";
/// Batching delay plus slack.
const DELIVERY: Duration = Duration::from_secs(6);
/// After a delivery, how long nothing else may arrive.
const QUIET: Duration = Duration::from_secs(3);

struct Dirs {
    data: TempDir,
    keys: TempDir,
}

impl Dirs {
    fn new() -> Dirs {
        Dirs {
            data: TempDir::new().unwrap(),
            keys: TempDir::new().unwrap(),
        }
    }

    fn config(&self, push: PushConfig) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: self.data.path().to_owned(),
            master_key: self.keys.path().join("master.key"),
            init_key: true,
            insecure_plain_imap: false,
            max_sse_clients: 2,
            auth: Default::default(),
            web_dir: None,
            push,
            render_author_css: false,
        }
    }
}

fn loopback() -> PushConfig {
    PushConfig {
        policy: EndpointPolicy::LoopbackForTests,
        ..PushConfig::default()
    }
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

fn client_with(cookie: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("x-requested-with", "tagmail".parse().unwrap());
    headers.insert("user-agent", "TestBrowser/1".parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// A logged-in browser: setup on first start, login after.
async fn owner(server: &Server) -> reqwest::Client {
    let (path, body) = match server.auth().setup_token() {
        Some(token) => (
            "/api/auth/setup",
            json!({ "token": token, "password": PASSWORD }),
        ),
        None => ("/api/auth/login", json!({ "password": PASSWORD })),
    };
    let resp = reqwest::Client::new()
        .post(url(server, path))
        .header("x-requested-with", "tagmail")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let cookie = resp.headers()["set-cookie"].to_str().unwrap();
    client_with(cookie.split(';').next().unwrap())
}

async fn add_accounts(server: &Server, ids: &[i64]) {
    let ids = ids.to_vec();
    server
        .store()
        .config()
        .write(move |tx| {
            for id in ids {
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) \
                     VALUES (?1, 'a', 'i', 993, 'tls', 's', 465, 'tls', 'u', x'00', x'00', ?2)",
                    params![id, format!("me{id}@example.com")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[derive(Debug)]
struct Received {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct MockState {
    tx: mpsc::UnboundedSender<Received>,
    status: Arc<AtomicU16>,
    stall: Arc<AtomicBool>,
}

/// A push service on loopback that records every request.
struct Mock {
    addr: SocketAddr,
    rx: mpsc::UnboundedReceiver<Received>,
    status: Arc<AtomicU16>,
    stall: Arc<AtomicBool>,
}

async fn receive(
    State(state): State<MockState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let _ = state.tx.send(Received {
        path: uri.path().to_owned(),
        headers,
        body,
    });
    if state.stall.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    StatusCode::from_u16(state.status.load(Ordering::SeqCst)).unwrap()
}

impl Mock {
    async fn start() -> Mock {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = MockState {
            tx,
            status: Arc::new(AtomicU16::new(201)),
            stall: Arc::new(AtomicBool::new(false)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Mock {
            addr,
            rx,
            status: state.status.clone(),
            stall: state.stall.clone(),
        };
        let app = Router::new().fallback(receive).with_state(state);
        tokio::spawn(async move { axum::serve(listener, app).await });
        mock
    }

    fn endpoint(&self, name: &str) -> String {
        format!("http://{}/push/{name}", self.addr)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn next(&mut self) -> Received {
        tokio::time::timeout(DELIVERY, self.rx.recv())
            .await
            .expect("no push within the delivery window")
            .unwrap()
    }

    async fn quiet(&mut self) {
        if let Ok(Some(r)) = tokio::time::timeout(QUIET, self.rx.recv()).await {
            panic!("unexpected push to {}", r.path);
        }
    }
}

/// A browser's subscription keys.
struct Browser {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Browser {
    fn new() -> Browser {
        let mut auth = [0u8; 16];
        getrandom::fill(&mut auth).unwrap();
        Browser {
            secret: SecretKey::generate(),
            auth,
        }
    }

    fn subscription(&self, endpoint: &str) -> Value {
        json!({
            "endpoint": endpoint,
            "expirationTime": null,
            "keys": {
                "p256dh": URL_SAFE_NO_PAD.encode(self.secret.public_key().to_sec1_bytes()),
                "auth": URL_SAFE_NO_PAD.encode(self.auth),
            }
        })
    }

    fn decrypt(&self, body: &[u8]) -> Value {
        let plain = ece::decrypt(body, &self.secret, &self.auth).unwrap();
        serde_json::from_slice(&plain).unwrap()
    }
}

async fn subscribe(c: &reqwest::Client, server: &Server, body: &Value) -> reqwest::Response {
    c.post(url(server, "/api/push/subscriptions"))
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn subscribed(c: &reqwest::Client, server: &Server, body: &Value) -> i64 {
    let resp = subscribe(c, server, body).await;
    assert_eq!(resp.status(), 201);
    let dto: Value = resp.json().await.unwrap();
    dto["id"].as_i64().unwrap()
}

async fn subscriptions(c: &reqwest::Client, server: &Server) -> Vec<Value> {
    c.get(url(server, "/api/push/subscriptions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn arrival(account_id: i64, message_id: i64, from: &str, subject: &str) -> Arrival {
    Arrival {
        account_id,
        message_id,
        thread_id: Some(message_id + 1000),
        from: Some(AddressDto {
            name: Some(from.to_owned()),
            email: format!("{}@example.org", from.to_lowercase().replace(' ', ".")),
        }),
        subject: Some(subject.to_owned()),
        date: message_id,
    }
}

async fn column(server: &Server, sql: &'static str) -> Vec<i64> {
    server
        .store()
        .config()
        .read(move |c| {
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<tagmail_store::rusqlite::Result<_>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn new_mail_is_pushed_encrypted_and_signed() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    let resp = subscribe(&c, &server, &browser.subscription(&mock.endpoint("a"))).await;
    assert_eq!(resp.status(), 201);
    let dto: Value = resp.json().await.unwrap();
    assert_eq!(dto["endpointHost"], "127.0.0.1");
    assert_eq!(dto["preview"], true);
    assert_eq!(dto["current"], true);
    assert_eq!(dto["userAgent"], "TestBrowser/1");
    assert!(dto.get("endpoint").is_none(), "endpoint URL leaked");

    let key: Value = c
        .get(url(&server, "/api/push/key"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let public = URL_SAFE_NO_PAD
        .decode(key["publicKey"].as_str().unwrap())
        .unwrap();
    assert_eq!(public.len(), 65);

    server
        .feed()
        .publish_arrivals(vec![arrival(1, 7, "Alice Example", "Lunch?")]);
    let push = mock.next().await;
    assert_eq!(push.path, "/push/a");
    let header = |name: &str| push.headers[name].to_str().unwrap().to_owned();
    assert_eq!(header("ttl"), "43200");
    assert_eq!(header("urgency"), "normal");
    assert_eq!(header("topic"), "inbox-1");
    assert_eq!(header("content-encoding"), "aes128gcm");
    assert_eq!(header("content-type"), "application/octet-stream");

    let authorization = header("authorization");
    let (jwt, k) = authorization
        .strip_prefix("vapid t=")
        .unwrap()
        .split_once(", k=")
        .unwrap();
    assert_eq!(URL_SAFE_NO_PAD.decode(k).unwrap(), public);
    let (signed, signature) = jwt.rsplit_once('.').unwrap();
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
    VerifyingKey::from_sec1_bytes(&public)
        .unwrap()
        .verify(signed.as_bytes(), &signature)
        .unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(signed.split_once('.').unwrap().1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["aud"], mock.origin());
    assert_eq!(claims["sub"], "https://github.com/micheljung/tagmail");

    assert_eq!(
        browser.decrypt(&push.body),
        json!({"v": 1, "kind": "newMail", "accountId": 1, "count": 1, "threadId": 1007,
               "from": "Alice Example", "subject": "Lunch?", "url": "/thread/1007"})
    );
    assert!(push.body.len() <= 1365, "{}", push.body.len());
    assert_eq!(
        (push.body.len() - ece::HEADER_LEN - 16) % ece::PAD_BUCKET,
        0
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    let list = subscriptions(&c, &server).await;
    assert!(list[0]["lastSuccessAt"].is_i64(), "{list:?}");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_flood_of_arrivals_becomes_one_push_per_cooldown() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    subscribed(&c, &server, &browser.subscription(&mock.endpoint("a"))).await;

    for i in 0..5 {
        server
            .feed()
            .publish_arrivals(vec![arrival(1, i, &format!("Sender {i}"), "Hi")]);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let push = browser.decrypt(&mock.next().await.body);
    assert_eq!(push["count"], 5);
    assert_eq!(push["from"], "Sender 4", "newest describes the batch");
    assert_eq!(push["url"], "/inbox");

    server
        .feed()
        .publish_arrivals(vec![arrival(1, 9, "Late", "Hi")]);
    mock.quiet().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn preview_off_and_account_filters_shape_who_gets_what() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1, 2]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    let mut quiet = browser.subscription(&mock.endpoint("private"));
    quiet["preview"] = json!(false);
    quiet["accountIds"] = json!([1]);
    subscribed(&c, &server, &quiet).await;
    let mut other = browser.subscription(&mock.endpoint("other"));
    other["accountIds"] = json!([2]);
    subscribed(&c, &server, &other).await;

    server.feed().publish_arrivals(vec![
        arrival(1, 1, "Alice", "Secret"),
        arrival(2, 2, "Me", "Own mail"),
    ]);
    server.feed().publish_arrivals(vec![Arrival {
        from: Some(AddressDto {
            name: None,
            email: "ME2@example.com".into(),
        }),
        ..arrival(2, 3, "x", "Sent by me")
    }]);
    let mut pushes = [mock.next().await, mock.next().await];
    pushes.sort_by(|a, b| a.path.cmp(&b.path));
    let [other, private] = pushes;
    assert_eq!(private.path, "/push/private");
    assert_eq!(
        browser.decrypt(&private.body),
        json!({"v": 1, "kind": "newMail", "accountId": 1, "count": 1, "url": "/inbox"})
    );
    assert_eq!(other.path, "/push/other");
    assert_eq!(browser.decrypt(&other.body)["count"], 1, "own mail dropped");
    mock.quiet().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn gone_subscriptions_are_deleted() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    for status in [410, 404] {
        let browser = Browser::new();
        let endpoint = mock.endpoint(&format!("gone-{status}"));
        subscribed(&c, &server, &browser.subscription(&endpoint)).await;
        mock.status.store(status, Ordering::SeqCst);
        server
            .feed()
            .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
        mock.next().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            subscriptions(&c, &server).await,
            Vec::<Value>::new(),
            "{status}"
        );
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn logout_deletes_the_subscription_and_stops_pushes() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    subscribed(
        &c,
        &server,
        &Browser::new().subscription(&mock.endpoint("a")),
    )
    .await;
    let resp = c
        .post(url(&server, "/api/auth/logout"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        column(&server, "SELECT count(*) FROM push_subscription").await,
        [0]
    );
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    mock.quiet().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_sessions_get_no_pushes_and_hold_no_slots() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let idle = owner(&server).await;
    let live = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    subscribed(
        &idle,
        &server,
        &browser.subscription(&mock.endpoint("idle")),
    )
    .await;
    let past_idle = DEFAULT_SESSION_IDLE.as_secs() as i64 + 60;
    server
        .store()
        .config()
        .write(move |tx| {
            let session: Vec<u8> =
                tx.query_row("SELECT session_hash FROM push_subscription", [], |r| {
                    r.get(0)
                })?;
            tx.execute(
                "UPDATE session SET last_seen_at = last_seen_at - ?2 WHERE id_hash = ?1",
                params![session, past_idle],
            )?;
            for i in 1..MAX_SUBSCRIPTIONS {
                tx.execute(
                    "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth) \
                     VALUES (?1, ?2, zeroblob(65), zeroblob(16))",
                    params![session, format!("https://push.example/{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    mock.quiet().await;

    subscribed(
        &live,
        &server,
        &browser.subscription(&mock.endpoint("live")),
    )
    .await;
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 2, "Alice", "Hi")]);
    assert_eq!(mock.next().await.path, "/push/live");
    mock.quiet().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_capped_renewal_keeps_the_old_row() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let idle = owner(&server).await;
    let live = owner(&server).await;
    let mock = Mock::start().await;
    let browser = Browser::new();
    subscribed(&idle, &server, &browser.subscription(&mock.endpoint("old"))).await;
    let past_idle = DEFAULT_SESSION_IDLE.as_secs() as i64 + 60;
    server
        .store()
        .config()
        .write(move |tx| {
            let idle: Vec<u8> =
                tx.query_row("SELECT session_hash FROM push_subscription", [], |r| {
                    r.get(0)
                })?;
            tx.execute(
                "UPDATE session SET last_seen_at = last_seen_at - ?2 WHERE id_hash = ?1",
                params![idle, past_idle],
            )?;
            let live: Vec<u8> = tx.query_row(
                "SELECT id_hash FROM session WHERE id_hash != ?1",
                [&idle],
                |r| r.get(0),
            )?;
            for i in 0..MAX_SUBSCRIPTIONS {
                tx.execute(
                    "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth) \
                     VALUES (?1, ?2, zeroblob(65), zeroblob(16))",
                    params![live, format!("https://push.example/{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let renew = |old: String| {
        let mut body = browser.subscription(&mock.endpoint("new"));
        json_merge(&mut body, json!({"oldEndpoint": old}));
        body
    };

    let capped = subscribe(&live, &server, &renew(mock.endpoint("old"))).await;
    assert_eq!(capped.status(), 409);
    let old = column(
        &server,
        "SELECT count(*) FROM push_subscription WHERE endpoint LIKE '%/old'",
    )
    .await;
    assert_eq!(old, [1], "the refused renewal keeps the old row");

    let renewed = subscribe(&live, &server, &renew("https://push.example/0".into())).await;
    assert_eq!(renewed.status(), 201, "renewing a live row frees its slot");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_faults_do_not_count_against_a_subscription() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    subscribed(
        &c,
        &server,
        &Browser::new().subscription(&mock.endpoint("a")),
    )
    .await;
    server
        .store()
        .config()
        .write(|tx| {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('vapid_key', 'not hex')",
                [],
            )?;
            tx.execute(
                "UPDATE push_subscription SET failures = ?1",
                [MAX_FAILURES - 1],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    tokio::time::sleep(BATCH_DELAY + Duration::from_secs(1)).await;
    mock.quiet().await;
    assert_eq!(
        column(&server, "SELECT failures FROM push_subscription").await,
        [MAX_FAILURES - 1],
        "an unreadable VAPID key is not the subscription's failure"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn devices_cannot_use_push_routes() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    let pairing: Value = c
        .post(url(&server, "/api/pairing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paired: Value = reqwest::Client::new()
        .post(url(&server, "/api/pairing/exchange"))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "code": pairing["code"], "name": "Laptop" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = paired["token"].as_str().unwrap();
    let desktop = reqwest::Client::new();
    for (method, path) in [
        (reqwest::Method::GET, "/api/push/key"),
        (reqwest::Method::GET, "/api/push/subscriptions"),
        (reqwest::Method::POST, "/api/push/subscriptions"),
        (reqwest::Method::DELETE, "/api/push/subscriptions/1"),
    ] {
        let resp = desktop
            .request(method.clone(), url(&server, path))
            .bearer_auth(token)
            .json(&Browser::new().subscription("https://push.example/x"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{method} {path}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "session_only");
    }
    server.shutdown().await.unwrap();
}

/// Answers every lookup with fixed addresses.
struct FixedDns(Vec<IpAddr>);

impl Resolver for FixedDns {
    fn resolve(&self, _host: String) -> tagmail_api_core::BoxFuture<io::Result<Vec<IpAddr>>> {
        let ips = self.0.clone();
        Box::pin(async move { Ok(ips) })
    }
}

#[tokio::test]
async fn strict_policy_never_sends_to_private_addresses() {
    let dirs = Dirs::new();
    let strict = PushConfig {
        policy: EndpointPolicy::Strict,
        resolver: Arc::new(FixedDns(vec!["10.0.0.1".parse().unwrap()])),
    };
    let server = Server::start(&dirs.config(strict), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    let resp = subscribe(&c, &server, &browser.subscription(&mock.endpoint("a"))).await;
    assert_eq!(resp.status(), 422, "http loopback refused under Strict");
    subscribed(
        &c,
        &server,
        &browser.subscription("https://push.example/send/abc"),
    )
    .await;
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    let mut failures = Vec::new();
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        failures = column(&server, "SELECT failures FROM push_subscription").await;
        if failures == [1] {
            break;
        }
    }
    assert_eq!(failures, [1]);
    mock.quiet().await;
    server.shutdown().await.unwrap();
}

/// Answers `gone.example` with NXDOMAIN, `empty.example` with no
/// addresses and every other name with a resolver timeout.
struct BrokenDns;

impl Resolver for BrokenDns {
    fn resolve(&self, host: String) -> tagmail_api_core::BoxFuture<io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            match host.as_str() {
                "gone.example" => Err(io::ErrorKind::NotFound.into()),
                "empty.example" => Ok(Vec::new()),
                _ => Err(io::ErrorKind::TimedOut.into()),
            }
        })
    }
}

#[tokio::test]
async fn a_host_that_no_longer_exists_counts_as_a_failure() {
    let dirs = Dirs::new();
    let strict = PushConfig {
        policy: EndpointPolicy::Strict,
        resolver: Arc::new(BrokenDns),
    };
    let server = Server::start(&dirs.config(strict), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    for host in ["gone.example", "empty.example", "flaky.example"] {
        subscribed(
            &c,
            &server,
            &Browser::new().subscription(&format!("https://{host}/send")),
        )
        .await;
    }
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    tokio::time::sleep(BATCH_DELAY + Duration::from_secs(1)).await;
    assert_eq!(
        column(
            &server,
            "SELECT failures FROM push_subscription ORDER BY id"
        )
        .await,
        [1, 1, 0],
        "a missing host counts, a resolver timeout does not"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_vapid_key_survives_a_restart_sealed() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    let key = |c: reqwest::Client, server_url: String| async move {
        let body: Value = c
            .get(server_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["publicKey"].as_str().unwrap().to_owned()
    };
    let first = key(c.clone(), url(&server, "/api/push/key")).await;
    assert_eq!(first, key(c, url(&server, "/api/push/key")).await);
    server.shutdown().await.unwrap();

    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    assert_eq!(key(c, url(&server, "/api/push/key")).await, first);
    let stored: String = server
        .store()
        .config()
        .read(|c| {
            Ok(
                c.query_row("SELECT value FROM meta WHERE key = 'vapid_key'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    const SCALAR_HEX: usize = 64;
    assert!(stored.len() > SCALAR_HEX, "raw scalar stored");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn subscriptions_are_validated_capped_and_rate_limited() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1, 2]).await;
    let mock = Mock::start().await;
    let browser = Browser::new();
    let good = browser.subscription(&mock.endpoint("a"));
    let field = |patch: Value| {
        let mut body = good.clone();
        json_merge(&mut body, patch);
        body
    };
    for (body, expected) in [
        (field(json!({"endpoint": "ftp://127.0.0.1/x"})), "endpoint"),
        (field(json!({"keys": {"p256dh": "AAAA"}})), "keys.p256dh"),
        (field(json!({"keys": {"auth": "AAAA"}})), "keys.auth"),
        (field(json!({"accountIds": []})), "accountIds"),
    ] {
        let resp = subscribe(&c, &server, &body).await;
        assert_eq!(resp.status(), 422, "{body}");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(err["field"], expected, "{body}");
    }
    let csrf = reqwest::Client::new()
        .post(url(&server, "/api/push/subscriptions"))
        .json(&good)
        .send()
        .await
        .unwrap();
    assert_eq!(csrf.status(), 401, "no cookie, no CSRF header");

    let id = subscribed(&c, &server, &good).await;
    server
        .store()
        .config()
        .write(|tx| {
            let session: Vec<u8> =
                tx.query_row("SELECT id_hash FROM session LIMIT 1", [], |r| r.get(0))?;
            for i in 1..20 {
                tx.execute(
                    "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth) \
                     VALUES (?1, ?2, zeroblob(65), zeroblob(16))",
                    params![session, format!("https://push.example/{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let full = subscribe(&c, &server, &browser.subscription(&mock.endpoint("21st"))).await;
    assert_eq!(full.status(), 409);
    let err: Value = full.json().await.unwrap();
    assert_eq!(err["error"], "too_many_subscriptions");

    let delete = url(&server, &format!("/api/push/subscriptions/{id}"));
    assert_eq!(c.delete(&delete).send().await.unwrap().status(), 204);
    assert_eq!(c.delete(&delete).send().await.unwrap().status(), 404);
    let mut limited = None;
    for _ in 0..PUSH_WRITES.burst {
        let resp = c.delete(&delete).send().await.unwrap();
        if resp.status() == 429 {
            limited = Some(resp);
            break;
        }
    }
    let limited = limited.expect("writes share one rate limit");
    assert!(limited.headers().contains_key("retry-after"));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_re_post_drops_removed_accounts_and_a_patch_rejects_them() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1, 2]).await;
    let mock = Mock::start().await;
    let browser = Browser::new();
    let mut body = browser.subscription(&mock.endpoint("a"));
    json_merge(&mut body, json!({"preview": false, "accountIds": [1, 999]}));
    let created = subscribe(&c, &server, &body).await;
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    assert_eq!(created["accountIds"], json!([1]));

    json_merge(&mut body, json!({"preview": null, "accountIds": [999]}));
    let reposted = subscribe(&c, &server, &body).await;
    assert_eq!(reposted.status(), 200);
    let reposted: Value = reposted.json().await.unwrap();
    assert_eq!(
        (&reposted["preview"], &reposted["accountIds"]),
        (&json!(false), &Value::Null),
        "no known account left means all accounts"
    );

    let patched = c
        .patch(url(
            &server,
            &format!("/api/push/subscriptions/{}", created["id"]),
        ))
        .json(&json!({"accountIds": [2, 999]}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 422);
    let err: Value = patched.json().await.unwrap();
    assert_eq!(err["field"], "accountIds");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn subscriptions_keep_settings_and_send_tests() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1, 2]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    let good = browser.subscription(&mock.endpoint("a"));
    let id = subscribed(&c, &server, &good).await;
    let again = subscribe(&c, &server, &good).await;
    assert_eq!(again.status(), 200, "same endpoint updates");

    let patched: Value = c
        .patch(url(&server, &format!("/api/push/subscriptions/{id}")))
        .json(&json!({"preview": false, "accountIds": [2]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (&patched["preview"], &patched["accountIds"]),
        (&json!(false), &json!([2]))
    );
    let reposted: Value = subscribe(&c, &server, &good).await.json().await.unwrap();
    assert_eq!(
        (&reposted["preview"], &reposted["accountIds"]),
        (&json!(false), &json!([2])),
        "a bare re-post keeps the settings"
    );
    let all: Value = c
        .patch(url(&server, &format!("/api/push/subscriptions/{id}")))
        .json(&json!({"accountIds": null}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (&all["preview"], &all["accountIds"]),
        (&json!(false), &Value::Null)
    );
    let missing = c
        .patch(url(&server, "/api/push/subscriptions/999"))
        .json(&json!({"preview": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let test_url = url(&server, &format!("/api/push/subscriptions/{id}/test"));
    assert_eq!(c.post(&test_url).send().await.unwrap().status(), 202);
    assert_eq!(c.post(&test_url).send().await.unwrap().status(), 429);
    assert_eq!(
        browser.decrypt(&mock.next().await.body),
        json!({"v": 1, "kind": "test", "url": "/settings/notifications"})
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_renewed_endpoint_replaces_its_old_row_and_keeps_its_settings() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1, 2]).await;
    let mock = Mock::start().await;
    let browser = Browser::new();
    let renew = |endpoint: &str, old: &str, patch: Value| {
        let mut body = browser.subscription(&mock.endpoint(endpoint));
        json_merge(&mut body, json!({"oldEndpoint": mock.endpoint(old)}));
        json_merge(&mut body, patch);
        body
    };
    let mut first = browser.subscription(&mock.endpoint("a"));
    json_merge(&mut first, json!({"preview": false, "accountIds": [2]}));
    subscribed(&c, &server, &first).await;

    let resp = subscribe(&c, &server, &renew("b", "a", json!({}))).await;
    assert_eq!(resp.status(), 201);
    let renewed: Value = resp.json().await.unwrap();
    assert_eq!(
        (&renewed["preview"], &renewed["accountIds"]),
        (&json!(false), &json!([2])),
        "the renewal inherits the settings"
    );
    let ids: Vec<Value> = subscriptions(&c, &server)
        .await
        .into_iter()
        .map(|s| s["id"].clone())
        .collect();
    assert_eq!(ids, vec![renewed["id"].clone()], "the old row is gone");

    let given: Value = subscribe(&c, &server, &renew("c", "b", json!({"preview": true})))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (&given["preview"], &given["accountIds"]),
        (&json!(true), &json!([2])),
        "explicit fields win, the rest is inherited"
    );
    let unknown: Value = subscribe(&c, &server, &renew("d", "nowhere", json!({})))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (&unknown["preview"], &unknown["accountIds"]),
        (&json!(true), &Value::Null),
        "an unknown old endpoint gives the defaults"
    );
    assert_eq!(subscriptions(&c, &server).await.len(), 2);
    server.shutdown().await.unwrap();
}

fn json_merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, v) in p {
                json_merge(t.entry(k).or_insert(Value::Null), v);
            }
        }
        (t, p) => *t = p,
    }
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_stalled_push_service() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    mock.stall.store(true, Ordering::SeqCst);
    subscribed(
        &c,
        &server,
        &Browser::new().subscription(&mock.endpoint("a")),
    )
    .await;
    server
        .feed()
        .publish_arrivals(vec![arrival(1, 1, "Alice", "Hi")]);
    mock.next().await;
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("shutdown hung on a push in flight")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_lagging_dispatcher_sends_generic_new_mail() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(loopback()), None).await.unwrap();
    let c = owner(&server).await;
    add_accounts(&server, &[1]).await;
    let mut mock = Mock::start().await;
    let browser = Browser::new();
    subscribed(&c, &server, &browser.subscription(&mock.endpoint("a"))).await;
    const BATCHES: i64 = 100;
    for i in 0..BATCHES {
        server
            .feed()
            .publish_arrivals(vec![arrival(1, i, "Bulk", "Hi")]);
    }
    assert_eq!(
        browser.decrypt(&mock.next().await.body),
        json!({"v": 1, "kind": "newMail", "url": "/inbox"}),
        "missed arrivals leave count, account and sender unknown"
    );
    mock.quiet().await;
    server.shutdown().await.unwrap();
}
