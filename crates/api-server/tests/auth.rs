//! Owner authentication end to end over HTTP: route protection, setup,
//! login, sessions, rate limits, CSRF, device pairing, passkeys (with a
//! software authenticator) and the SPA.

use std::path::Path;
use std::time::Duration;

use passkey::authenticator::{Authenticator, UiHint, UserCheck, UserValidationMethod};
use passkey::client::{Client as PasskeyClient, DefaultClientData};
use passkey::crypto::rust_crypto::RustCryptoBackend;
use passkey::types::ctap2::{Aaguid, Ctap2Error};
use passkey::types::webauthn::{CredentialCreationOptions, CredentialRequestOptions};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tagmail_api_server::auth::{AuthConfig, parse_origin};
use tagmail_api_server::{Config, Server, auth, http, ratelimit};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

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
            max_sse_clients: 2,
            auth: AuthConfig::default(),
            web_dir: None,
            push: Default::default(),
        }
    }
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

fn origin(server: &Server) -> String {
    format!("http://localhost:{}", server.local_addr().port())
}

/// A client like the SPA: no cookie yet, sends the CSRF header.
fn browser() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert("x-requested-with", HeaderValue::from_static("tagmail"));
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn with_cookie(cookie: &str) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert("x-requested-with", HeaderValue::from_static("tagmail"));
    headers.insert("cookie", cookie.parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// `name=value` of the response's session cookie.
fn session_cookie(resp: &reqwest::Response) -> String {
    let set = resp.headers()["set-cookie"].to_str().unwrap();
    set.split(';').next().unwrap().to_owned()
}

async fn setup(server: &Server) -> reqwest::Client {
    let token = server.auth().setup_token().expect("no owner yet");
    let resp = browser()
        .post(url(server, "/api/auth/setup"))
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    with_cookie(&session_cookie(&resp))
}

async fn login(server: &Server) -> (reqwest::Client, String) {
    let resp = browser()
        .post(url(server, "/api/auth/login"))
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cookie = session_cookie(&resp);
    (with_cookie(&cookie), cookie)
}

async fn status(client: &reqwest::Client, server: &Server, path: &str) -> StatusCode {
    client.get(url(server, path)).send().await.unwrap().status()
}

async fn error_code(resp: reqwest::Response) -> String {
    let body: Value = resp.json().await.unwrap();
    body["error"].as_str().unwrap_or_default().to_owned()
}

/// Requests that must be denied without credentials besides the route
/// table: wrong methods, unknown paths and encoded spellings.
const EXTRA_PROTECTED: &[(&str, &str)] = &[
    ("GET", "/api/auth/login"),
    ("GET", "/api/auth/setup"),
    ("GET", "/api/pairing/exchange"),
    ("PUT", "/api/version"),
    ("DELETE", "/api/status"),
    ("PUT", "/api/threads/1"),
    ("PATCH", "/api/labels/1"),
    ("DELETE", "/api/messages/1"),
    ("GET", "/api/no-such-route"),
    ("GET", "/api"),
    ("GET", "/api/"),
    ("GET", "/%61pi/status"),
    ("GET", "/api/%73tatus"),
];

/// Every protected request: each route from the server's route table with
/// its `{…}` segments filled in, plus [`EXTRA_PROTECTED`].
fn protected() -> Vec<(Method, String)> {
    let filled = http::api_routes()
        .into_iter()
        .filter(|(method, path)| !auth::is_public(method, path))
        .map(|(method, path)| {
            let path = path
                .split('/')
                .map(|s| if s.starts_with('{') { "1" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            (method, path)
        });
    let extra = EXTRA_PROTECTED
        .iter()
        .map(|(m, p)| (Method::from_bytes(m.as_bytes()).unwrap(), (*p).to_owned()));
    filled.chain(extra).collect()
}

#[test]
fn the_route_table_covers_every_api_area() {
    let routes: Vec<String> = http::api_routes()
        .into_iter()
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
    for expected in [
        "GET /api/events",
        "GET /api/status",
        "GET /api/views/{view}/index",
        "GET /api/threads",
        "GET /api/threads/{id}",
        "GET /api/messages/{id}",
        "GET /api/messages/{id}/render",
        "GET /api/parts/{token}",
        "GET /api/proxy/img/{token}",
        "GET /api/image-senders",
        "POST /api/image-senders",
        "GET /api/mailboxes",
        "GET /api/labels",
        "POST /api/accounts",
        "GET /api/sessions",
        "DELETE /api/devices/{id}",
        "POST /api/pairing",
        "GET /api/replica/snapshot",
        "POST /api/replica/rows",
        "GET /api/replica/messages/{id}/raw",
        "POST /api/replica/image",
        "GET /api/push/key",
        "GET /api/push/subscriptions",
        "POST /api/push/subscriptions",
        "PATCH /api/push/subscriptions/{id}",
        "DELETE /api/push/subscriptions/{id}",
        "POST /api/push/subscriptions/{id}/test",
    ] {
        assert!(
            routes.iter().any(|r| r == expected),
            "{expected} not in {routes:?}"
        );
    }
}

async fn assert_all_protected(server: &Server, expected: &str) {
    let client = reqwest::Client::new();
    for (method, path) in protected() {
        let resp = client
            .request(method.clone(), url(server, &path))
            .header("x-requested-with", "tagmail")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{method} {path}");
        assert_eq!(error_code(resp).await, expected, "{method} {path}");
    }
}

#[tokio::test]
async fn every_protected_route_answers_401_without_credentials() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    assert_all_protected(&server, "setup_required").await;
    let owner = setup(&server).await;
    assert_all_protected(&server, "unauthorized").await;

    let anon = reqwest::Client::new();
    assert_eq!(status(&anon, &server, "/healthz").await, 200);
    assert_eq!(status(&anon, &server, "/api/version").await, 200);
    for (method, path) in protected().into_iter().filter(|(m, _)| *m == Method::GET) {
        let resp = owner.get(url(&server, &path)).send().await.unwrap();
        assert_ne!(resp.status(), 401, "{method} {path} with a session");
    }
    let bad = reqwest::Client::new()
        .get(url(&server, "/api/status"))
        .bearer_auth("not-a-device-token")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    let garbage = reqwest::Client::new()
        .get(url(&server, "/api/status"))
        .header("cookie", "tagmail_session=garbage")
        .send()
        .await
        .unwrap();
    assert_eq!(garbage.status(), 401);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn setup_needs_the_token_and_happens_once() {
    let dirs = Dirs::new();
    let config = dirs.config();
    let server = Server::start(&config, None).await.unwrap();
    let token = server.auth().setup_token().unwrap();
    let setup_with = |token: String, password: &'static str| {
        browser()
            .post(url(&server, "/api/auth/setup"))
            .json(&json!({ "token": token, "password": password }))
            .send()
    };

    let resp = setup_with("wrong".into(), PASSWORD).await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(error_code(resp).await, "invalid_setup_token");
    let resp = setup_with(token.clone(), "short").await.unwrap();
    assert_eq!(resp.status(), 422);
    let resp = browser()
        .post(url(&server, "/api/auth/login"))
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(resp).await, "setup_required");

    let resp = setup_with(token.clone(), PASSWORD).await.unwrap();
    assert_eq!(resp.status(), 201);
    let set_cookie = resp.headers()["set-cookie"].to_str().unwrap().to_owned();
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.contains(attr), "{set_cookie}");
    }
    assert!(
        !set_cookie.contains("Secure"),
        "loopback http: {set_cookie}"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["via"], "session");
    assert_eq!(body["session"]["current"], true);
    assert!(server.auth().setup_token().is_none());

    let resp = setup_with(token, "another password").await.unwrap();
    assert_eq!(resp.status(), 409);
    server.shutdown().await.unwrap();

    let server = Server::start(&config, None).await.unwrap();
    assert!(server.auth().setup_token().is_none(), "owner persisted");
    login(&server).await;
    let hash: String = server
        .store()
        .config()
        .read(|conn| Ok(conn.query_row("SELECT password_hash FROM owner", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(
        hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{hash}"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn login_logout_expiry_and_revocation() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    setup(&server).await;

    let resp = browser()
        .post(url(&server, "/api/auth/login"))
        .json(&json!({ "password": "wrong password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(error_code(resp).await, "invalid_credentials");

    let (a, _) = login(&server).await;
    let (b, _) = login(&server).await;
    assert_eq!(status(&a, &server, "/api/status").await, 200);
    let me: Value = a
        .get(url(&server, "/api/auth/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let a_id = me["session"]["id"].as_str().unwrap().to_owned();

    let list: Value = b
        .get(url(&server, "/api/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3, "setup + two logins");
    assert_eq!(sessions.iter().filter(|s| s["current"] == true).count(), 1);
    assert!(sessions.iter().any(|s| s["id"] == a_id.as_str()));

    let resp = b
        .delete(url(&server, &format!("/api/sessions/{a_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(status(&a, &server, "/api/status").await, 401, "revoked");
    let resp = b
        .delete(url(&server, &format!("/api/sessions/{a_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = b
        .post(url(&server, "/api/auth/logout"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert!(
        resp.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(status(&b, &server, "/api/status").await, 401, "logged out");

    let db = server.store().config();
    let (idle, _) = login(&server).await;
    db.write(|tx| {
        tx.execute(
            "UPDATE session SET last_seen_at = last_seen_at - 31 * 86400",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(status(&idle, &server, "/api/status").await, 401, "idle");

    let (old, _) = login(&server).await;
    db.write(|tx| {
        tx.execute("UPDATE session SET expires_at = unixepoch() - 1", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(status(&old, &server, "/api/status").await, 401, "absolute");

    let (fresh, _) = login(&server).await;
    db.write(|tx| {
        tx.execute("UPDATE session SET last_seen_at = last_seen_at - 3600", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(status(&fresh, &server, "/api/status").await, 200);
    let touched: i64 = db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT unixepoch() - max(last_seen_at) FROM session",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(touched < 60, "use refreshes the idle timer");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn login_rate_limit_uses_forwarded_for_only_from_trusted_proxies() {
    let dirs = Dirs::new();
    let mut config = dirs.config();
    config.auth.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()];
    let server = Server::start(&config, None).await.unwrap();
    setup(&server).await;
    let attempt = |client_ip: &'static str, password: &'static str| {
        browser()
            .post(url(&server, "/api/auth/login"))
            .header("x-forwarded-for", format!("{client_ip}, 127.0.0.1"))
            .json(&json!({ "password": password }))
            .send()
    };
    for _ in 0..ratelimit::PER_IP.burst {
        assert_eq!(
            attempt("198.51.100.7", "wrong one").await.unwrap().status(),
            401
        );
    }
    let limited = attempt("198.51.100.7", PASSWORD).await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    assert_eq!(
        attempt("203.0.113.9", PASSWORD).await.unwrap().status(),
        200
    );
    server.shutdown().await.unwrap();

    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    setup(&server).await;
    for n in 1..ratelimit::PER_IP.burst {
        let ip = format!("198.51.100.{n}");
        let resp = browser()
            .post(url(&server, "/api/auth/login"))
            .header("x-forwarded-for", ip)
            .json(&json!({ "password": "wrong one" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
    let resp = browser()
        .post(url(&server, "/api/auth/login"))
        .header("x-forwarded-for", "203.0.113.9")
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        429,
        "untrusted peer: X-Forwarded-For ignored"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutating_requests_need_the_csrf_header_and_same_origin() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    let owner = setup(&server).await;
    let cookie = |client: reqwest::RequestBuilder| client.json(&json!({}));

    let plain = reqwest::Client::new();
    let (_, session) = login(&server).await;
    let resp = cookie(
        plain
            .post(url(&server, "/api/accounts"))
            .header("cookie", &session),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 403, "no X-Requested-With");
    assert_eq!(error_code(resp).await, "csrf");
    let resp = cookie(
        owner
            .post(url(&server, "/api/accounts"))
            .header("origin", "https://evil.example"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 403, "foreign Origin");
    let resp = cookie(
        owner
            .post(url(&server, "/api/accounts"))
            .header("sec-fetch-site", "cross-site"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 403, "cross-site fetch");
    let resp = cookie(
        owner
            .post(url(&server, "/api/accounts"))
            .header("origin", origin(&server))
            .header("sec-fetch-site", "same-origin"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 503, "passes CSRF, reaches the handler");

    let resp = plain
        .post(url(&server, "/api/auth/login"))
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "login CSRF");
    assert_eq!(
        status(&plain, &server, "/api/version").await,
        200,
        "GET needs no header"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn https_origin_sets_secure_cookies_and_checks_that_origin() {
    let dirs = Dirs::new();
    let mut config = dirs.config();
    config.auth.public_origin = Some(parse_origin("https://mail.example.com").unwrap());
    let server = Server::start(&config, None).await.unwrap();
    let token = server.auth().setup_token().unwrap();
    let resp = browser()
        .post(url(&server, "/api/auth/setup"))
        .header("origin", "https://mail.example.com")
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert!(
        resp.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
    let resp = browser()
        .post(url(&server, "/api/auth/login"))
        .header("origin", origin(&server))
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "localhost is not the public origin");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn desktop_pairing_exchanges_a_single_use_code_for_a_revocable_token() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    let owner = setup(&server).await;
    let desktop = reqwest::Client::new();
    let exchange = |code: String| {
        desktop
            .post(url(&server, "/api/pairing/exchange"))
            .header("x-requested-with", "tagmail")
            .json(&json!({ "code": code, "name": "Laptop" }))
            .send()
    };

    let pairing: Value = owner
        .post(url(&server, "/api/pairing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = pairing["code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 8);
    assert_eq!(
        pairing["url"],
        format!(
            "tagmail://pair?server={}&code={code}",
            origin(&server).replace(':', "%3A").replace('/', "%2F")
        )
    );

    let resp = owner
        .post(url(&server, "/api/pairing/exchange"))
        .header("origin", origin(&server))
        .json(&json!({ "code": code, "name": "Browser" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "browsers never get a device token");

    let lower = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    let resp = exchange(lower).await.unwrap();
    assert_eq!(resp.status(), 201);
    let paired: Value = resp.json().await.unwrap();
    let token = paired["token"].as_str().unwrap().to_owned();
    let device_id = paired["deviceId"].as_i64().unwrap();
    let resp = exchange(code).await.unwrap();
    assert_eq!(resp.status(), 401, "single use");
    assert_eq!(error_code(resp).await, "invalid_pairing_code");

    let bearer = |path: &str| desktop.get(url(&server, path)).bearer_auth(&token);
    assert_eq!(bearer("/api/status").send().await.unwrap().status(), 200);
    let me: Value = bearer("/api/auth/session")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        me,
        json!({ "via": "device", "session": null, "deviceId": device_id })
    );
    let resp = bearer("/api/status")
        .header("origin", origin(&server))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "bearer from a browser context");
    let resp = desktop
        .post(url(&server, "/api/accounts"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "bearer requests skip the CSRF header");
    let resp = desktop
        .post(url(&server, "/api/pairing"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "devices cannot pair devices");

    let me: Value = owner
        .get(url(&server, "/api/auth/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner_session = me["session"]["id"].as_str().unwrap().to_owned();
    let management = [
        (Method::GET, "/api/sessions".to_owned()),
        (Method::DELETE, format!("/api/sessions/{owner_session}")),
        (Method::DELETE, format!("/api/devices/{device_id}")),
        (Method::POST, "/api/auth/passkey/register/start".to_owned()),
        (Method::POST, "/api/auth/passkey/register/finish".to_owned()),
        (Method::DELETE, "/api/passkeys/1".to_owned()),
    ];
    for (method, path) in management {
        let resp = desktop
            .request(method.clone(), url(&server, &path))
            .bearer_auth(&token)
            .json(&json!({ "name": "x", "ceremonyId": "x", "credential": {} }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "device token: {method} {path}");
        assert_eq!(error_code(resp).await, "session_required");
    }
    assert_eq!(
        status(&owner, &server, "/api/status").await,
        200,
        "owner session kept"
    );
    for path in ["/api/devices", "/api/passkeys"] {
        assert_eq!(bearer(path).send().await.unwrap().status(), 200, "{path}");
    }

    let expired: Value = owner
        .post(url(&server, "/api/pairing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    server
        .store()
        .config()
        .write(|tx| {
            tx.execute("UPDATE pairing_code SET expires_at = unixepoch() - 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let resp = exchange(expired["code"].as_str().unwrap().to_owned())
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "expired");

    let devices: Value = owner
        .get(url(&server, "/api/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices["devices"][0]["name"], "Laptop");
    assert!(devices["devices"][0]["lastSeenAt"].is_i64());
    let resp = owner
        .delete(url(&server, &format!("/api/devices/{device_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(
        bearer("/api/status").send().await.unwrap().status(),
        401,
        "revoked"
    );
    let devices: Value = owner
        .get(url(&server, "/api/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices["devices"], json!([]));

    let pairing: Value = owner
        .post(url(&server, "/api/pairing"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = exchange(pairing["code"].as_str().unwrap().to_owned())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second = second["token"].as_str().unwrap().to_owned();
    let resp = desktop
        .post(url(&server, "/api/auth/logout"))
        .bearer_auth(&second)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204, "a device may revoke itself");
    let resp = desktop
        .get(url(&server, "/api/status"))
        .bearer_auth(&second)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let stored: Vec<u8> = server
        .store()
        .config()
        .read(|conn| Ok(conn.query_row("SELECT token_hash FROM device", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token.as_bytes());
    server.shutdown().await.unwrap();
}

/// passkey-rs serializes binary fields as byte arrays; browsers'
/// `PublicKeyCredential.toJSON()` sends base64url strings.
fn to_webauthn_json(value: impl serde::Serialize) -> Value {
    use base64::Engine;
    fn convert(value: Value) -> Value {
        match value {
            Value::Array(items)
                if !items.is_empty()
                    && items.iter().all(|v| v.as_u64().is_some_and(|b| b <= 255)) =>
            {
                let bytes: Vec<u8> = items.iter().map(|v| v.as_u64().unwrap() as u8).collect();
                Value::String(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
            }
            Value::Array(items) => Value::Array(items.into_iter().map(convert).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, convert(v))).collect())
            }
            other => other,
        }
    }
    convert(serde_json::to_value(value).unwrap())
}

struct AlwaysVerified;

#[async_trait::async_trait]
impl UserValidationMethod for AlwaysVerified {
    type PasskeyItem = passkey::types::Passkey;

    async fn check_user<'a>(
        &self,
        _hint: UiHint<'a, Self::PasskeyItem>,
        presence: bool,
        verification: bool,
    ) -> Result<UserCheck, Ctap2Error> {
        Ok(UserCheck {
            presence,
            verification,
        })
    }

    fn is_verification_enabled(&self) -> Option<bool> {
        Some(true)
    }

    fn is_presence_enabled(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn passkey_registration_and_login_with_a_software_authenticator() {
    let dirs = Dirs::new();
    let server = Server::start(&dirs.config(), None).await.unwrap();
    let owner = setup(&server).await;
    let store: Option<passkey::types::Passkey> = None;
    let authenticator = Authenticator::new(
        Aaguid::new_empty(),
        store,
        AlwaysVerified,
        RustCryptoBackend,
    );
    let mut client = PasskeyClient::new(authenticator).allows_insecure_localhost(true);
    let web_origin = url::Url::parse(&origin(&server)).unwrap();

    let resp = browser()
        .post(url(&server, "/api/auth/passkey/login/start"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "no passkey yet");

    let start: Value = owner
        .post(url(&server, "/api/auth/passkey/register/start"))
        .json(&json!({ "name": "Test key" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let options: CredentialCreationOptions =
        serde_json::from_value(start["options"].clone()).unwrap();
    let created = client
        .register(&web_origin, options, DefaultClientData)
        .await
        .unwrap();
    let created = to_webauthn_json(created);
    let resp = owner
        .post(url(&server, "/api/auth/passkey/register/finish"))
        .json(&json!({ "ceremonyId": start["ceremonyId"], "credential": created }))
        .send()
        .await
        .unwrap();
    let status_code = resp.status();
    let passkey: Value = resp.json().await.unwrap();
    assert_eq!(status_code, 201, "{passkey}");
    assert_eq!(passkey["name"], "Test key");
    let resp = owner
        .post(url(&server, "/api/auth/passkey/register/finish"))
        .json(&json!({ "ceremonyId": start["ceremonyId"], "credential": created }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "ceremonies finish once");

    let login_once = async |client: &mut PasskeyClient<_, _, _, _, _>| {
        let start: Value = browser()
            .post(url(&server, "/api/auth/passkey/login/start"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let options: CredentialRequestOptions =
            serde_json::from_value(start["options"].clone()).unwrap();
        let assertion = client
            .authenticate(&web_origin, options, DefaultClientData)
            .await
            .unwrap();
        let assertion = to_webauthn_json(assertion);
        browser()
            .post(url(&server, "/api/auth/passkey/login/finish"))
            .json(&json!({ "ceremonyId": start["ceremonyId"], "credential": assertion }))
            .send()
            .await
            .unwrap()
    };
    let resp = login_once(&mut client).await;
    assert_eq!(resp.status(), 200);
    let passkey_session = with_cookie(&session_cookie(&resp));
    assert_eq!(status(&passkey_session, &server, "/api/status").await, 200);
    assert_eq!(
        login_once(&mut client).await.status(),
        200,
        "counter advances"
    );

    let list: Value = owner
        .get(url(&server, "/api/passkeys"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["passkeys"].as_array().unwrap().len(), 1);
    let id = list["passkeys"][0]["id"].as_i64().unwrap();
    let resp = owner
        .delete(url(&server, &format!("/api/passkeys/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = browser()
        .post(url(&server, "/api/auth/passkey/login/start"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "removed");
    server.shutdown().await.unwrap();
}

fn write_web(dir: &Path) {
    std::fs::create_dir_all(dir.join("_app/immutable")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><title>Tagmail</title>",
    )
    .unwrap();
    std::fs::write(dir.join("_app/immutable/app.js"), "export {}").unwrap();
    std::fs::write(dir.join("_app/boot.js"), "import('./immutable/app.js')").unwrap();
}

#[tokio::test]
async fn serves_the_spa_publicly_without_shadowing_the_api() {
    let dirs = Dirs::new();
    let web = tempfile::tempdir().unwrap();
    write_web(web.path());
    let mut config = dirs.config();
    config.web_dir = Some(web.path().to_owned());
    let server = Server::start(&config, None).await.unwrap();
    let anon = reqwest::Client::new();

    for path in [
        "/",
        "/thread/42",
        "/label/Work%20Stuff",
        "/settings/devices",
    ] {
        let resp = anon.get(url(&server, path)).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let headers = resp.headers().clone();
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(
            csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"),
            "{csp}"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            resp.text()
                .await
                .unwrap()
                .contains("<title>Tagmail</title>")
        );
    }
    let resp = anon
        .get(url(&server, "/_app/immutable/app.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert!(
        resp.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert_eq!(status(&anon, &server, "/_app/boot.js").await, 200);
    assert_eq!(status(&anon, &server, "/_app/missing.js").await, 404);
    assert_eq!(status(&anon, &server, "/../master.key").await, 404);
    assert_eq!(status(&anon, &server, "/%2e%2e/master.key").await, 404);
    let head = anon.head(url(&server, "/")).send().await.unwrap();
    assert_eq!(head.status(), 200);

    assert_eq!(status(&anon, &server, "/api/status").await, 401);
    assert_eq!(status(&anon, &server, "/api/nope").await, 401);
    assert_eq!(status(&anon, &server, "/healthz").await, 200);
    let owner = setup(&server).await;
    let resp = owner.get(url(&server, "/api/nope")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.headers()["content-type"], "application/json");
    server.shutdown().await.unwrap();
}

/// Reads the stream until it ends; panics if it is still open after 1 s.
async fn assert_stream_ends(mut resp: reqwest::Response, what: &str) {
    let ended = tokio::time::timeout(Duration::from_secs(1), async {
        while resp.chunk().await.unwrap().is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "stream still open after {what}");
}

async fn assert_stream_open(resp: &mut reqwest::Response) {
    let read = tokio::time::timeout(Duration::from_millis(500), resp.chunk()).await;
    assert!(read.is_err(), "stream ended early: {read:?}");
}

#[tokio::test]
async fn event_streams_end_when_credentials_do() {
    let dirs = Dirs::new();
    let mut config = dirs.config();
    config.max_sse_clients = 8;
    config.auth.stream_recheck = Duration::from_millis(200);
    let server = Server::start(&config, None).await.unwrap();
    let owner = setup(&server).await;
    let events = |client: &reqwest::Client| client.get(url(&server, "/api/events")).send();

    let (revoked, _) = login(&server).await;
    let me: Value = revoked
        .get(url(&server, "/api/auth/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut stream = events(&revoked).await.unwrap();
    assert_eq!(stream.status(), 200);
    assert_stream_open(&mut stream).await;
    let id = me["session"]["id"].as_str().unwrap();
    let resp = owner
        .delete(url(&server, &format!("/api/sessions/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_stream_ends(stream, "session revocation").await;
    assert_eq!(events(&revoked).await.unwrap().status(), 401);

    let (logged_out, _) = login(&server).await;
    let stream = events(&logged_out).await.unwrap();
    let resp = logged_out
        .post(url(&server, "/api/auth/logout"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_stream_ends(stream, "logout").await;

    let pairing: Value = owner
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
    let mut stream = reqwest::Client::new()
        .get(url(&server, "/api/events"))
        .bearer_auth(paired["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    assert_stream_open(&mut stream).await;
    let resp = owner
        .delete(url(
            &server,
            &format!("/api/devices/{}", paired["deviceId"]),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_stream_ends(stream, "device revocation").await;

    let (idle, _) = login(&server).await;
    let me: Value = idle
        .get(url(&server, "/api/auth/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hash = hex::decode(me["session"]["id"].as_str().unwrap()).unwrap();
    let mut stream = events(&idle).await.unwrap();
    assert_stream_open(&mut stream).await;
    server
        .store()
        .config()
        .write(move |tx| {
            tx.execute(
                "UPDATE session SET last_seen_at = last_seen_at - 31 * 86400 WHERE id_hash = ?1",
                [hash],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_stream_ends(stream, "idle expiry").await;
    assert_eq!(status(&owner, &server, "/api/status").await, 200);
    server.shutdown().await.unwrap();
}
