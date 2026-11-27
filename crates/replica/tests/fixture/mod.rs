//! A server with the sync engine on a fixed port that can be stopped and
//! started again on the same data, seeded with in-window, out-of-window
//! and starred mail, and a paired replica served through the router.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_core::{Method, Request, Response};
use tagmail_api_server::{Config, Server};
use tagmail_core::{AccountId, EmailId, MessageRowId};
use tagmail_replica::{Layout, Replica, Router, ServerClient, Settings};
use tagmail_store::{BlobKey, DEFAULT_RAW_CAP, FtsDocument};
use tempfile::TempDir;
use url::Url;

const PASSWORD: &str = "correct horse battery";
pub const DAY: i64 = 86_400;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

pub fn raw(subject: &str, body: &str) -> String {
    format!(
        "From: ann@x.example\r\nSubject: {subject}\r\nContent-Type: text/plain\r\n\r\n{body}\r\n"
    )
}

pub struct Stack {
    pub data: TempDir,
    pub keys: TempDir,
    pub desktop: TempDir,
    pub listen: SocketAddr,
    server: Option<Server>,
    pub owner: reqwest::Client,
    pub token: String,
    pub account: i64,
}

impl Stack {
    fn config(data: &TempDir, keys: &TempDir, listen: SocketAddr) -> Config {
        Config {
            listen,
            data_dir: data.path().to_owned(),
            master_key: keys.path().join("master.key"),
            init_key: true,
            insecure_plain_imap: true,
            max_sse_clients: 4,
            auth: Default::default(),
            web_dir: None,
            push: Default::default(),
            render_author_css: false,
        }
    }

    async fn boot(data: &TempDir, keys: &TempDir, listen: SocketAddr) -> Server {
        let engine = tagmail_engine::EngineConfig {
            draft_debounce: Duration::from_secs(3600),
            ..tagmail_engine::EngineConfig::default()
        };
        Server::start_with_engine(&Stack::config(data, keys, listen), engine)
            .await
            .unwrap()
    }

    /// Seeded as described by [`Stack::seed`].
    pub async fn start() -> Stack {
        let stack = Stack::empty().await;
        stack.seed().await;
        stack
    }

    /// An account without cache rows, and a paired device.
    pub async fn empty() -> Stack {
        let (data, keys) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let listen = free_port();
        let server = Stack::boot(&data, &keys, listen).await;
        let base = format!("http://{listen}");
        let setup = server.auth().setup_token().unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/auth/setup"))
            .header("x-requested-with", "tagmail")
            .json(&json!({ "token": setup, "password": PASSWORD }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let cookie = resp.headers()["set-cookie"].to_str().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("cookie", cookie.split(';').next().unwrap().parse().unwrap());
        headers.insert("x-requested-with", "tagmail".parse().unwrap());
        let owner = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let account: Value = owner
            .post(format!("{base}/api/accounts"))
            .json(&json!({
                "name": "Work",
                "imapHost": "127.0.0.1",
                "imapPort": free_port().port(),
                "imapTls": "tls",
                "smtpHost": "127.0.0.1",
                "smtpPort": free_port().port(),
                "smtpTls": "starttls",
                "username": "me@x.example",
                "password": "s3cret",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let account = account["id"].as_i64().unwrap();
        let pairing: Value = owner
            .post(format!("{base}/api/pairing"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let device: Value = reqwest::Client::new()
            .post(format!("{base}/api/pairing/exchange"))
            .header("x-requested-with", "tagmail")
            .json(&json!({ "code": pairing["code"], "name": "Laptop" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Stack {
            data,
            keys,
            desktop: tempfile::tempdir().unwrap(),
            listen,
            server: Some(server),
            owner,
            token: device["token"].as_str().unwrap().to_owned(),
            account,
        }
    }

    pub fn server(&self) -> &Server {
        self.server.as_ref().expect("server running")
    }

    pub async fn stop(&mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
    }

    pub async fn restart(&mut self) {
        self.server = Some(Stack::boot(&self.data, &self.keys, self.listen).await);
    }

    /// The server's cache is rebuilt with a new epoch; call while stopped.
    pub fn wipe_cache(&self) {
        tagmail_store::CacheDb::wipe(&self.data.path().join(tagmail_api_server::CACHE_DB)).unwrap();
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listen)
    }

    pub fn key(&self, email_id: &str) -> BlobKey {
        BlobKey::for_message(AccountId(self.account), &EmailId::parse(email_id).unwrap())
    }

    /// INBOX (1), Archive (2). Thread 2: message 10 (recent, unread,
    /// INBOX, "quick fox"), 11 (old, Archive, "old news"), 12 (old,
    /// starred, Archive, "starred gem"). Thread 3: message 13 (old,
    /// INBOX, "ancient scroll"). Every message has text and raw on the
    /// server.
    pub async fn seed(&self) {
        let account = self.account;
        let (recent, old) = (now() - DAY, now() - 400 * DAY);
        let keys: Vec<String> = ["g:10", "g:11", "g:12", "g:13"]
            .iter()
            .map(|e| self.key(e).as_str().to_owned())
            .collect();
        self.server()
            .store()
            .cache()
            .write(move |tx| {
                for (id, subject, body) in [
                    (10, "Hi", "quick fox"),
                    (11, "Old", "old news"),
                    (12, "Starred", "starred gem"),
                    (13, "Other", "ancient scroll"),
                ] {
                    tagmail_store::fts::index(
                        tx,
                        MessageRowId(id),
                        &FtsDocument {
                            subject,
                            from: "ann@x.example",
                            to: "",
                            body,
                        },
                    )?;
                }
                tx.execute_batch(&format!(
                    "INSERT INTO mailbox (id, account_id, name, special_use, uidvalidity, sync_state) VALUES
                         (1, {account}, 'INBOX', NULL, 7, 'live'),
                         (2, {account}, 'Archive', '\\Archive', 8, 'live');
                     INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred) VALUES
                         (2, {account}, 'hi', {recent}, 3, 1, 1), (3, {account}, 'other', {old}, 1, 0, 0);
                     INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, text_blob, raw_blob) VALUES
                         (10, {account}, 'g:10', 2, {recent}, 'Hi', 'hello', '[{{\"email\":\"ann@x.example\"}}]', '{k0}', '{k0}'),
                         (11, {account}, 'g:11', 2, {old}, 'Old', 'old', '[{{\"email\":\"ann@x.example\"}}]', '{k1}', '{k1}'),
                         (12, {account}, 'g:12', 2, {old}, 'Starred', 'star', '[{{\"email\":\"ann@x.example\"}}]', '{k2}', '{k2}'),
                         (13, {account}, 'g:13', 3, {old}, 'Other', 'other', '[{{\"email\":\"ann@x.example\"}}]', '{k3}', '{k3}');
                     INSERT INTO location (mailbox_id, uid, message_id, flags, keywords) VALUES
                         (1, 1, 10, 0, ''), (2, 1, 11, 1, ''), (2, 2, 12, 3, ''), (1, 2, 13, 1, '');
                     INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES
                         (2, 1, {recent}), (2, 2, {old}), (3, 1, {old});",
                    k0 = keys[0],
                    k1 = keys[1],
                    k2 = keys[2],
                    k3 = keys[3],
                ))?;
                Ok(())
            })
            .await
            .unwrap();
        let blobs = self.server().store().blobs().clone();
        let items: Vec<(BlobKey, &str, &str)> = vec![
            (self.key("g:10"), "Hi", "quick fox"),
            (self.key("g:11"), "Old", "old news"),
            (self.key("g:12"), "Starred", "starred gem"),
            (self.key("g:13"), "Other", "ancient scroll"),
        ];
        tokio::task::spawn_blocking(move || {
            for (key, subject, body) in items {
                blobs.put_text(&key, body).unwrap();
                let mut writer = blobs.raw_writer(key, DEFAULT_RAW_CAP).unwrap();
                writer.write_chunk(raw(subject, body).as_bytes()).unwrap();
                writer.finish().unwrap();
            }
        })
        .await
        .unwrap();
    }

    /// A bootstrapped replica of the server.
    pub async fn replica(&self) -> Arc<Replica> {
        let client = ServerClient::new(
            &Url::parse(&format!("http://{}/", self.listen)).unwrap(),
            Some(self.token.clone()),
        )
        .unwrap();
        let replica = Replica::open(
            Layout::new(self.desktop.path()),
            client,
            Settings::default(),
        )
        .await
        .unwrap();
        replica.rebootstrap().await.unwrap();
        replica
    }

    /// The owner's view of a server route.
    pub async fn owner_get(&self, path: &str) -> Value {
        self.owner
            .get(self.url(path))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

pub async fn call(router: &Router, method: Method, path: &str, body: &[u8]) -> Response {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    router
        .handle(
            Request {
                method,
                path,
                query,
                body,
            },
            Some("application/json"),
        )
        .await
}

pub async fn get(router: &Router, path: &str) -> Response {
    call(router, Method::Get, path, b"").await
}

pub fn json(resp: &Response) -> Value {
    serde_json::from_slice(&resp.body).unwrap_or(Value::Null)
}

/// Thread ids of a view index response.
pub fn ids(resp: &Response) -> Vec<i64> {
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    tagmail_api_core::id_index::decode(&resp.body).unwrap()
}

pub fn search_path(q: &str) -> String {
    let encoded: String = percent_encoding::utf8_percent_encode(
        &format!("search:{q}"),
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string();
    format!("/api/views/{encoded}/index")
}
