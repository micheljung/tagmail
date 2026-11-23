//! An in-process server with the sync engine and one account whose IMAP
//! and SMTP never answer, seeded with cache rows; and a paired replica.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_server::{Config, Server};
use tagmail_replica::{Layout, Replica, ServerClient, Settings};
use tempfile::TempDir;
use url::Url;

const PASSWORD: &str = "correct horse battery";
pub const DAY: i64 = 86_400;

pub struct Fixture {
    pub server: Server,
    /// The owner's browser session.
    pub owner: reqwest::Client,
    pub token: String,
    pub account: i64,
    pub data: TempDir,
    pub keys: TempDir,
    pub desktop: TempDir,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

impl Fixture {
    pub async fn start() -> Fixture {
        let (data, keys) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: data.path().to_owned(),
            master_key: keys.path().join("master.key"),
            init_key: true,
            insecure_plain_imap: true,
            max_sse_clients: 4,
            auth: Default::default(),
            web_dir: None,
        };
        let engine = tagmail_engine::EngineConfig {
            draft_debounce: Duration::from_secs(3600),
            ..tagmail_engine::EngineConfig::default()
        };
        let server = Server::start_with_engine(&config, engine).await.unwrap();
        let base = format!("http://{}", server.local_addr());
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
        let fixture = Fixture {
            server,
            owner,
            token: device["token"].as_str().unwrap().to_owned(),
            account,
            data,
            keys,
            desktop: tempfile::tempdir().unwrap(),
        };
        fixture.seed().await;
        fixture
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.server.local_addr())
    }

    /// INBOX (1) and Archive (2); thread 2 holds unread message 10 (g:10)
    /// in INBOX and read message 11 (g:11) in Archive.
    async fn seed(&self) {
        let account = self.account;
        let date = now() - DAY;
        self.server
            .store()
            .cache()
            .write(move |tx| {
                tx.execute_batch(&format!(
                    "INSERT INTO mailbox (id, account_id, name, special_use, uidvalidity, sync_state) VALUES
                         (1, {account}, 'INBOX', NULL, 7, 'live'),
                         (2, {account}, 'Archive', '\\Archive', 8, 'live');
                     INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count) VALUES
                         (2, {account}, 'hi', {date}, 2, 1);
                     INSERT INTO message (id, account_id, email_id, thread_id, date, subject, from_json) VALUES
                         (10, {account}, 'g:10', 2, {date}, 'Hi', '[{{\"email\":\"ann@x.example\"}}]'),
                         (11, {account}, 'g:11', 2, {}, 'Re: Hi', NULL);
                     INSERT INTO location (mailbox_id, uid, message_id, flags, keywords) VALUES
                         (1, 1, 10, 0, ''), (2, 1, 11, 1, '');
                     INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES
                         (2, 1, {date}), (2, 2, {});",
                    date - 60,
                    date - 60,
                ))?;
                Ok(())
            })
            .await
            .unwrap();
    }

    pub async fn replica(&self) -> Arc<Replica> {
        let client = ServerClient::new(
            &Url::parse(&format!("http://{}/", self.server.local_addr())).unwrap(),
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

    /// The server's own ops (`GET /api/ops`).
    pub async fn server_ops(&self) -> Vec<Value> {
        let ops: Value = self
            .owner
            .get(self.url("/api/ops"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ops["ops"].as_array().cloned().unwrap_or_default()
    }
}
