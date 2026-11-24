//! Compose routes over HTTP with the engine: streamed uploads and their
//! cap, the draft body cap, send dedupe, the outbox.

use std::time::Duration;

use serde_json::{Value, json};
use tagmail_api_core::compose::{MAX_DRAFT_BODY, MAX_UPLOAD};
use tagmail_api_server::http::MAX_BODY;
use tagmail_api_server::{Config, Server};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

struct Fixture {
    server: Server,
    client: reqwest::Client,
    _data: TempDir,
    _keys: TempDir,
}

fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn fixture() -> Fixture {
    let (data, keys) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data.path().to_owned(),
        master_key: keys.path().join("master.key"),
        init_key: true,
        insecure_plain_imap: true,
        max_sse_clients: 2,
        auth: Default::default(),
        web_dir: None,
        push: Default::default(),
    };
    let engine = tagmail_engine::EngineConfig {
        draft_debounce: Duration::from_secs(3600),
        ..tagmail_engine::EngineConfig::default()
    };
    let server = Server::start_with_engine(&config, engine).await.unwrap();
    let token = server.auth().setup_token().unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://{}/api/auth/setup", server.local_addr()))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let cookie = resp.headers()["set-cookie"].to_str().unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("cookie", cookie.split(';').next().unwrap().parse().unwrap());
    headers.insert("x-requested-with", "tagmail".parse().unwrap());
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    Fixture {
        server,
        client,
        _data: data,
        _keys: keys,
    }
}

const BOUNDARY: &str = "tagmail-test-boundary";

/// A `multipart/form-data` body with one part.
fn multipart(field: &str, file_name: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; \
         filename=\"{file_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

impl Fixture {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.server.local_addr())
    }

    async fn upload(&self, body: Vec<u8>) -> reqwest::Response {
        self.client
            .post(self.url("/api/attachments"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn compose_routes_upload_draft_and_send() {
    let f = fixture().await;
    let resp = f
        .client
        .post(f.url("/api/accounts"))
        .json(&json!({
            "name": "Work",
            "imapHost": "127.0.0.1",
            "imapPort": dead_port(),
            "imapTls": "tls",
            "smtpHost": "127.0.0.1",
            "smtpPort": dead_port(),
            "smtpTls": "starttls",
            "username": "me@example.com",
            "password": "s3cret",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    let identities: Value = f
        .client
        .get(f.url("/api/identities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let identities = &identities["identities"];
    assert_eq!(identities[0]["address"], "me@example.com");
    assert_eq!(identities[0]["isDefault"], true);
    let identity = identities[0]["id"].clone();

    let upload = f
        .upload(multipart(
            "file",
            "../notes.txt",
            "text/plain; charset=utf-8",
            b"attached text",
        ))
        .await;
    assert_eq!(upload.status(), 201);
    let upload: Value = upload.json().await.unwrap();
    assert_eq!(upload["name"], "notes.txt");
    assert_eq!(upload["contentType"], "text/plain");
    assert_eq!(upload["size"], 13);
    assert!(upload["id"].is_string());

    let cap = usize::try_from(MAX_UPLOAD).unwrap();
    let over = f
        .upload(multipart(
            "file",
            "big.bin",
            "application/octet-stream",
            &vec![0; cap + 1],
        ))
        .await;
    assert_eq!(over.status(), 413, "refused while streaming");
    let declared = f.upload(vec![0; cap * 2]).await;
    assert_eq!(declared.status(), 413, "refused by Content-Length");
    let wrong_field = f
        .upload(multipart("other", "a.txt", "text/plain", b"x"))
        .await;
    assert_eq!(wrong_field.status(), 422);
    let raw = f
        .client
        .post(f.url("/api/attachments"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 415);

    let long = "quoted thread ".repeat(MAX_BODY / 7);
    let draft = json!({
        "identityId": identity,
        "to": [{ "email": "ann@example.org" }],
        "bcc": [{ "name": "Hidden", "email": "bob@example.org" }],
        "subject": "Hello",
        "text": long,
        "attachmentIds": [upload["id"]],
    });
    let created = f
        .client
        .post(f.url("/api/drafts"))
        .json(&draft)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    assert_eq!(created["attachments"][0]["name"], "notes.txt");
    let id = created["id"].as_i64().unwrap();

    let keyed = json!({
        "identityId": identity,
        "to": [{ "email": "ann@example.org" }],
        "subject": "Offline",
        "text": "written on the desktop",
        "clientId": "3d1f0c55-desk-top",
    });
    let post_keyed = |body: Value| f.client.post(f.url("/api/drafts")).json(&body).send();
    let first_keyed = post_keyed(keyed.clone()).await.unwrap();
    assert_eq!(first_keyed.status(), 201);
    let first_keyed: Value = first_keyed.json().await.unwrap();
    let lost_ack = post_keyed(keyed.clone()).await.unwrap();
    assert_eq!(lost_ack.status(), 200, "same client id: the same draft");
    let lost_ack: Value = lost_ack.json().await.unwrap();
    assert_eq!(lost_ack["id"], first_keyed["id"]);
    assert_eq!(lost_ack["version"], 1, "unchanged");
    let mut edited = keyed.clone();
    edited["subject"] = json!("Offline, edited");
    let edited = post_keyed(edited).await.unwrap();
    assert_eq!(edited.status(), 200);
    let edited: Value = edited.json().await.unwrap();
    assert_eq!(edited["id"], first_keyed["id"]);
    assert_eq!(edited["subject"], "Offline, edited");
    assert_eq!(edited["version"], 2);
    let mut spaced = keyed.clone();
    spaced["clientId"] = json!("has space");
    let spaced = post_keyed(spaced).await.unwrap();
    assert_eq!(spaced.status(), 422);
    let spaced: Value = spaced.json().await.unwrap();
    assert_eq!(spaced["field"], "clientId");

    let mut bad = draft.clone();
    bad["to"] = json!([{ "email": "not an address" }]);
    let resp = f
        .client
        .put(f.url(&format!("/api/drafts/{id}")))
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let resp = f
        .client
        .post(f.url("/api/drafts"))
        .json(&json!({ "identityId": identity, "text": "x".repeat(MAX_DRAFT_BODY) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "drafts have their own, larger cap");
    let resp = f
        .client
        .post(f.url("/api/image-senders"))
        .json(&json!({ "address": "x".repeat(MAX_BODY) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "other routes keep the small cap");

    let send = json!({ "opId": "0b6c1d6e-7f43-4c55-9a51-3f0b0c9c2a11", "draftId": id });
    let first = f
        .client
        .post(f.url("/api/send"))
        .json(&send)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 202);
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["status"], "queued");
    let again = f
        .client
        .post(f.url("/api/send"))
        .json(&send)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200, "same op id: no second send");

    let resp = f
        .client
        .put(f.url(&format!("/api/drafts/{id}")))
        .json(&draft)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "queued drafts are frozen");

    let outbox: Value = f
        .client
        .get(f.url("/api/outbox"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        outbox["items"].as_array().unwrap().len(),
        1,
        "the repeated send queued nothing"
    );
    let item = &outbox["items"][0];
    assert_eq!(item["opId"], send["opId"]);
    assert_eq!(item["subject"], "Hello");
    assert_eq!(
        item["to"][1],
        json!({ "name": "Hidden", "email": "bob@example.org" })
    );
    assert!(item["id"].is_i64());

    let resp = f
        .client
        .get(f.url("/api/messages/999/reply?mode=all"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = f
        .client
        .get(f.url("/api/messages/1/reply?mode=sideways"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    f.server.shutdown().await.unwrap();
}
