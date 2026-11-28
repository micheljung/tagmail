//! An in-process `tagmail-server` for desktop tests: owner set up, mail
//! seeded, pairing codes on demand and new mail delivered like the engine.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_server::{Config, Server};
use tagmail_core::{AccountId, EmailId, MessageRowId};
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobKey, ChangeOp, DEFAULT_RAW_CAP, Entity, FtsDocument, NewChange, changes};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";
pub const INBOX: i64 = 1;
pub const SEEDED_THREADS: i64 = 3;

pub struct Env {
    pub data: TempDir,
    pub keys: TempDir,
    pub desktop: TempDir,
    pub listen: SocketAddr,
}

impl Env {
    pub fn new() -> Env {
        let listen = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        Env {
            data: tempfile::tempdir().unwrap(),
            keys: tempfile::tempdir().unwrap(),
            desktop: tempfile::tempdir().unwrap(),
            listen,
        }
    }

    pub fn config(&self) -> Config {
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

    /// Like [`Env::start`] with `threads` single-message INBOX threads over
    /// two years instead: thread `id` is newer than `id - 1`, every tenth
    /// subject says "invoice", text and search rows exist for all of them
    /// (text blobs only inside the window), and message [`OLD_RAW`] has its
    /// raw body on the server.
    pub async fn start_large(&self, threads: i64) -> Owner {
        let server = Server::start(&self.config(), None).await.unwrap();
        let cookie = setup_owner(&server).await;
        seed_account(&server).await;
        seed_large(&server, threads).await;
        Owner {
            server: Some(server),
            cookie,
        }
    }

    /// Starts the server, sets up the owner and seeds an account with
    /// [`SEEDED_THREADS`] INBOX threads.
    pub async fn start(&self) -> Owner {
        let server = Server::start(&self.config(), None).await.unwrap();
        let cookie = setup_owner(&server).await;
        seed(&server).await;
        Owner {
            server: Some(server),
            cookie,
        }
    }
}

pub struct Owner {
    server: Option<Server>,
    cookie: String,
}

impl Owner {
    pub fn server(&self) -> &Server {
        self.server.as_ref().expect("server running")
    }

    pub async fn stop(&mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
    }

    /// Starts again on the same data and port, now with the sync engine so
    /// writes are accepted (the seeded account has no reachable IMAP
    /// server, so ops stay pending).
    pub async fn restart_with_engine(&mut self, env: &Env) {
        let engine = tagmail_engine::EngineConfig::default();
        let server = Server::start_with_engine(&env.config(), engine)
            .await
            .unwrap();
        self.server = Some(server);
    }

    /// The thread ids of a view, as the web loads them.
    pub async fn index(&self, view: &str) -> Vec<i64> {
        let resp = reqwest::Client::new()
            .get(format!("{}/api/views/{view}/index", self.url()))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{view}");
        tagmail_api_core::id_index::decode(&resp.bytes().await.unwrap()).unwrap()
    }

    /// The server's ops, as the web lists them.
    pub async fn ops(&self) -> Vec<Value> {
        let body: Value = reqwest::Client::new()
            .get(format!("{}/api/ops", self.url()))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["ops"].as_array().cloned().unwrap_or_default()
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.server().local_addr())
    }

    /// A fresh single-use pairing code, as the web's Devices page makes.
    pub async fn pairing_code(&self) -> String {
        let pairing: Value = reqwest::Client::new()
            .post(format!("{}/api/pairing", self.url()))
            .header("cookie", &self.cookie)
            .header("x-requested-with", "tagmail")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        pairing["code"].as_str().unwrap().to_owned()
    }

    /// Devices the server lists, as the web's Devices page shows them.
    pub async fn devices(&self) -> Vec<Value> {
        let body: Value = reqwest::Client::new()
            .get(format!("{}/api/devices", self.url()))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["devices"].as_array().unwrap().clone()
    }

    /// Dismisses a reminder as the web does.
    pub async fn dismiss(&self, alarm_key: &str) -> u16 {
        reqwest::Client::new()
            .post(format!("{}/api/reminders/{alarm_key}/dismiss", self.url()))
            .header("cookie", &self.cookie)
            .header("x-requested-with", "tagmail")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// Delivers an unseen INBOX message in a new thread `id`, as a sync
    /// would, and wakes the event streams.
    pub async fn arrive(&self, id: i64, subject: &str, from: &str) {
        let date = now();
        let subject = subject.to_owned();
        let from = format!(r#"[{{"email":"{from}"}}]"#);
        self.server()
            .store()
            .cache()
            .write(move |tx| {
                insert_thread(tx, id, date, &subject, &from, 0)?;
                let log: Vec<NewChange> = [Entity::Thread, Entity::Message]
                    .into_iter()
                    .map(|entity| NewChange {
                        entity,
                        entity_id: id,
                        op: ChangeOp::Upsert,
                        payload: None,
                    })
                    .collect();
                changes::append_all(tx, &log)?;
                Ok(())
            })
            .await
            .unwrap();
        self.server()
            .feed()
            .publish(self.server().store().cache().head().await.unwrap());
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn setup_owner(server: &Server) -> String {
    let token = server.auth().setup_token().unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://{}/api/auth/setup", server.local_addr()))
        .header("x-requested-with", "tagmail")
        .json(&json!({ "token": token, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let cookie = resp.headers()["set-cookie"].to_str().unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

fn insert_thread(
    tx: &tagmail_store::rusqlite::Connection,
    id: i64,
    date: i64,
    subject: &str,
    from_json: &str,
    flags: i64,
) -> tagmail_store::Result<()> {
    tx.execute(
        "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count) \
         VALUES (?1, 1, ?2, ?3, 1, ?4)",
        params![id, subject.to_lowercase(), date, i64::from(flags & 1 == 0)],
    )?;
    tx.execute(
        "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json) \
         VALUES (?1, 1, ?2, ?1, ?3, ?4, 'hello', ?5)",
        params![id, format!("g:{id}"), date, subject, from_json],
    )?;
    tx.execute(
        "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (?1, ?2, ?2, ?3)",
        params![INBOX, id, flags],
    )?;
    tx.execute(
        "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, ?2, ?3)",
        params![id, INBOX, date],
    )?;
    Ok(())
}

async fn seed_account(server: &Server) {
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
                 VALUES (3, 1, 'me@x.example', 1);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn seed(server: &Server) {
    seed_account(server).await;
    let date = now() - 3600;
    server
        .store()
        .cache()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO mailbox (id, account_id, name, uidvalidity, sync_state) \
                 VALUES (?1, 1, 'INBOX', 7, 'live')",
                [INBOX],
            )?;
            for id in 1..=SEEDED_THREADS {
                insert_thread(tx, id, date + id, &format!("Seeded {id}"), "[]", 1)?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

pub const OLD_RAW: i64 = 5;
pub const OLD_BODY: &str = "from the archive";
const SPAN: i64 = 2 * 365 * DAY;
const WINDOW: i64 = 90 * DAY;
pub const DAY: i64 = 86_400;

pub fn large_subject(id: i64) -> String {
    let topic = if id % 10 == 0 {
        "invoice"
    } else {
        "meeting notes"
    };
    format!("{topic} {id}")
}

fn key(id: i64) -> BlobKey {
    BlobKey::for_message(AccountId(1), &EmailId::parse(&format!("g:{id}")).unwrap())
}

async fn seed_large(server: &Server, threads: i64) {
    let start = now();
    let texts: Vec<(i64, String)> = server
        .store()
        .cache()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO mailbox (id, account_id, name, uidvalidity, sync_state) \
                 VALUES (?1, 1, 'INBOX', 7, 'live')",
                [INBOX],
            )?;
            let mut texts = Vec::new();
            for id in 1..=threads {
                let date = start - (threads - id) * SPAN / threads;
                let subject = large_subject(id);
                let body = format!("{subject}: the agenda for week {}", id % 52);
                let in_window = start - date < WINDOW;
                tx.execute(
                    "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count) \
                     VALUES (?1, 1, ?2, ?3, 1, 0)",
                    params![id, subject, date],
                )?;
                tx.execute(
                    "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, \
                     from_json, text_blob) VALUES (?1, 1, ?2, ?1, ?3, ?4, ?5, \
                     '[{\"email\":\"ann@x.example\"}]', ?6)",
                    params![
                        id,
                        format!("g:{id}"),
                        date,
                        subject,
                        &body,
                        in_window.then(|| key(id).as_str().to_owned())
                    ],
                )?;
                tx.execute(
                    "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (?1, ?2, ?2, 1)",
                    params![INBOX, id],
                )?;
                tx.execute(
                    "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, ?2, ?3)",
                    params![id, INBOX, date],
                )?;
                tagmail_store::fts::index(
                    tx,
                    MessageRowId(id),
                    &FtsDocument {
                        subject: &subject,
                        from: "ann@x.example",
                        to: "",
                        body: &body,
                    },
                )?;
                if in_window {
                    texts.push((id, body));
                }
            }
            Ok(texts)
        })
        .await
        .unwrap();
    let blobs = server.store().blobs().clone();
    tokio::task::spawn_blocking(move || {
        for (id, body) in texts {
            blobs.put_text(&key(id), &body).unwrap();
        }
        let raw = format!(
            "From: ann@x.example\r\nSubject: {}\r\nContent-Type: text/plain\r\n\r\n{OLD_BODY}\r\n",
            large_subject(OLD_RAW)
        );
        let mut writer = blobs.raw_writer(key(OLD_RAW), DEFAULT_RAW_CAP).unwrap();
        writer.write_chunk(raw.as_bytes()).unwrap();
        writer.finish().unwrap();
    })
    .await
    .unwrap();
}

pub async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}
