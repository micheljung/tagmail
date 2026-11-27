//! Message reading over HTTP against the SQLite store and blob cache.

use std::path::Path;

use serde_json::{Value, json};
use tagmail_api_core::messages::RENDER_CSP;
use tagmail_api_server::sqlite_store::{URL_KEY_ROTATION_SECS, url_keys_at};
use tagmail_api_server::{Config, Server};
use tagmail_core::{AccountId, EmailId};
use tagmail_store::{BlobKey, DEFAULT_RAW_CAP};
use tempfile::TempDir;

const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUg==";

fn config(data: &Path, keys: &Path, render_author_css: bool) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data.to_owned(),
        master_key: keys.join("master.key"),
        init_key: true,
        insecure_plain_imap: false,
        max_sse_clients: 2,
        auth: Default::default(),
        web_dir: None,
        push: Default::default(),
        render_author_css,
    }
}

fn raw() -> String {
    format!(
        concat!(
            "From: Ann <ann@x.example>\r\n",
            "Subject: Logo\r\n",
            "Content-Type: multipart/related; boundary=\"r\"\r\n",
            "\r\n",
            "--r\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<style>p.x{{color:red;background:url(https://cdn.example/b.png)}}</style>",
            "<p onmouseover=\"x()\">Hi</p><p class=\"x\">Styled</p><img src=\"cid:logo@x\">",
            "<img src=\"https://cdn.example/t.gif\"><script>alert(1)</script>\r\n",
            "--r\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <logo@x>\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "{}\r\n",
            "--r--\r\n",
        ),
        PNG_B64
    )
}

/// Thread 1 was merged into 2; message 11 is tombstoned; message 10's raw
/// body is cached, 12's is not and no engine runs.
async fn seed(server: &Server) {
    server
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO mailbox (id, account_id, name, uidvalidity, sync_state) VALUES
                     (1, 1, 'INBOX', 1, 'live');
                 INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred) VALUES
                     (2, 1, 'logo', 300, 2, 1, 1);
                 INSERT INTO thread (id, account_id, subject_norm, merged_into) VALUES (1, 1, 'logo', 2);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, to_json, has_attachment) VALUES
                     (10, 1, 'g:1010', 2, 100, 'Logo', 'Hi', '[{\"name\":\"Ann\",\"email\":\"ann@x.example\"}]', '[{\"email\":\"me@x.example\"}]', 1),
                     (12, 1, 'g:1012', 2, 300, 'Re: Logo', 'ok', '[{\"email\":\"bob@x.example\"}]', NULL, 0);
                 INSERT INTO message (id, account_id, email_id, thread_id, date, subject, tombstoned_at) VALUES
                     (11, 1, 'g:1011', 2, 200, 'gone', 1);
                 INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES
                     (1, 1, 10, 1), (1, 2, 12, 2), (1, 3, 11, 0);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let blobs = server.store().blobs().clone();
    tokio::task::spawn_blocking(move || {
        let key = BlobKey::for_message(AccountId(1), &EmailId::parse("g:1010").unwrap());
        let mut writer = blobs.raw_writer(key, DEFAULT_RAW_CAP).unwrap();
        writer.write_chunk(raw().as_bytes()).unwrap();
        writer.finish().unwrap();
    })
    .await
    .unwrap();
}

const PASSWORD: &str = "correct horse battery";

/// Sets up the owner (or logs in after a restart): a client with the
/// session cookie and the CSRF header.
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
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("x-requested-with", "tagmail".parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

async fn start() -> (Server, TempDir, TempDir) {
    start_with(false).await
}

async fn start_with(render_author_css: bool) -> (Server, TempDir, TempDir) {
    let (data, keys) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let server = Server::start(&config(data.path(), keys.path(), render_author_css), None)
        .await
        .unwrap();
    seed(&server).await;
    (server, data, keys)
}

fn url(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.local_addr())
}

async fn get(client: &reqwest::Client, server: &Server, path: &str) -> reqwest::Response {
    client.get(url(server, path)).send().await.unwrap()
}

#[tokio::test]
async fn renders_cached_messages_with_csp_and_signed_parts() {
    let (server, _d, _k) = start().await;
    let anonymous = reqwest::Client::new();
    for path in [
        "/api/messages/10/render",
        "/api/messages/10",
        "/api/threads/2",
    ] {
        assert_eq!(get(&anonymous, &server, path).await.status(), 401, "{path}");
    }
    let c = owner(&server).await;
    let resp = get(&c, &server, "/api/messages/10/render").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-security-policy"], RENDER_CSP);
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let html = resp.text().await.unwrap();
    assert!(html.contains("<p>Hi</p>"), "{html}");
    assert!(
        !html.contains("p.x") && !html.contains("class="),
        "author CSS is off by default: {html}"
    );
    assert!(
        !html.contains("script") && !html.contains("onmouseover") && !html.contains("cdn.example")
    );
    let start = html.find("/api/parts/").expect("signed cid URL");
    let part = html[start..].split('"').next().unwrap().to_owned();

    let resp = get(&c, &server, &part).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "image/png");
    assert!(resp.bytes().await.unwrap().starts_with(b"\x89PNG"));

    let mut forged = part.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    let resp = get(&c, &server, &forged).await;
    assert_eq!(resp.status(), 403);

    let detail: Value = get(&c, &server, "/api/messages/10")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["threadId"], 2);
    assert_eq!(detail["unread"], false);
    assert_eq!(detail["remoteImages"], 1);
    assert_eq!(
        detail["to"],
        json!([{ "name": null, "email": "me@x.example" }])
    );

    let resp = get(&c, &server, "/api/messages/12/render").await;
    assert_eq!(resp.status(), 503, "not cached and no engine");
    assert_eq!(
        get(&c, &server, "/api/messages/11").await.status(),
        404,
        "tombstoned"
    );
    server.shutdown().await.unwrap();
}

/// `--render-author-css` (B1): the sanitized sheet and `class` are served,
/// still `no-store`, so turning the flag off takes effect on the next load.
#[tokio::test]
async fn author_css_is_served_sanitized_and_uncached_when_enabled() {
    let (server, _d, _k) = start_with(true).await;
    let c = owner(&server).await;
    let resp = get(&c, &server, "/api/messages/10/render").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["content-security-policy"], RENDER_CSP);
    let html = resp.text().await.unwrap();
    assert!(
        html.contains("<body><style>p.x{color:red}</style>"),
        "{html}"
    );
    assert!(html.contains("<p class=\"x\">Styled</p>"), "{html}");
    assert!(
        !html.contains("cdn.example") && !html.contains("script"),
        "{html}"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn thread_detail_follows_merges_and_skips_tombstones() {
    let (server, _d, _k) = start().await;
    let c = owner(&server).await;
    let body: Value = get(&c, &server, "/api/threads/1")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["thread"]["id"], 2);
    assert_eq!(body["thread"]["starred"], true);
    let messages = body["messages"].as_array().unwrap();
    let ids: Vec<i64> = messages.iter().map(|m| m["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [10, 12], "oldest first, tombstoned excluded");
    assert_eq!(messages[0]["hasRemoteImages"], true);
    assert_eq!(messages[1]["hasRemoteImages"], Value::Null);
    assert_eq!(messages[1]["unread"], true);
    assert_eq!(messages[1]["starred"], true);
    assert_eq!(messages[0]["from"]["email"], "ann@x.example");
    assert_eq!(get(&c, &server, "/api/threads/3").await.status(), 404);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn image_senders_persist_across_restarts() {
    let (data, keys) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let cfg = config(data.path(), keys.path(), false);
    let server = Server::start(&cfg, None).await.unwrap();
    let resp = owner(&server)
        .await
        .post(url(&server, "/api/image-senders"))
        .json(&json!({ "address": "Ann@X.example", "allow": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    server.shutdown().await.unwrap();

    let server = Server::start(&cfg, None).await.unwrap();
    let c = owner(&server).await;
    let body: Value = get(&c, &server, "/api/image-senders")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["addresses"], json!(["ann@x.example"]));
    seed(&server).await;
    let html = get(&c, &server, "/api/messages/10/render")
        .await
        .text()
        .await
        .unwrap();
    assert!(html.contains("/api/proxy/img/"), "{html}");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn url_keys_are_created_kept_and_rotated() {
    let (data, keys) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let server = Server::start(&config(data.path(), keys.path(), false), None)
        .await
        .unwrap();
    let t0 = 1_800_000_000;
    let config_db = server.store().config();
    let first = config_db
        .write(move |tx| url_keys_at(tx, t0))
        .await
        .unwrap();
    assert!(first.previous.is_none());
    let same = config_db
        .write(move |tx| url_keys_at(tx, t0 + URL_KEY_ROTATION_SECS - 1))
        .await
        .unwrap();
    assert_eq!(*same.current, *first.current);
    let rotated = config_db
        .write(move |tx| url_keys_at(tx, t0 + URL_KEY_ROTATION_SECS))
        .await
        .unwrap();
    assert_ne!(*rotated.current, *first.current);
    assert_eq!(*rotated.previous.unwrap(), *first.current);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn image_proxy_is_rate_limited_per_client_ip() {
    let (data, keys) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut cfg = config(data.path(), keys.path(), false);
    cfg.auth.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
    let server = Server::start(&cfg, None).await.unwrap();
    let c = owner(&server).await;
    let proxied = |ip: &'static str| {
        c.get(url(&server, "/api/proxy/img/not-a-token"))
            .header("x-forwarded-for", ip)
            .send()
    };
    const BURST: usize = 60;
    for _ in 0..BURST {
        let resp = proxied("198.51.100.7").await.unwrap();
        assert_ne!(resp.status(), 429);
    }
    let limited = proxied("198.51.100.7").await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    let body: Value = limited.json().await.unwrap();
    assert_eq!(body["error"], "rate_limited");
    assert_ne!(proxied("198.51.100.8").await.unwrap().status(), 429);
    let other_route = get(&c, &server, "/api/image-senders").await;
    assert_eq!(other_route.status(), 200);
    server.shutdown().await.unwrap();
}
