//! AC 19 on the real stack (`cargo xtask dovecot up`: Dovecot and Mailpit):
//! the desktop app, paired to a tagmail-server that syncs a Dovecot account,
//! keeps reading, searching and composing with the server down, sends once
//! the server is back, and sends exactly once when it is killed between
//! handing a send to the server and recording the answer.
//! `TAGMAIL_IMAP_ADDR`, `TAGMAIL_SMTP_ADDR` and `TAGMAIL_MAILPIT_API` (as
//! in the engine's send tests) point elsewhere. Run under
//! `dbus-run-session` and `xvfb-run` with `--features app,test-hooks,stack-tests`.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use app::{LIVE, Lines, app_command};
use serde_json::{Value, json};
use tagmail_api_server::{Config, Server};
use tagmail_imap::{
    CapabilityOverride, ImapSession, Password, SearchCriteria, Security, SelectOptions,
    ServerConfig, TokioSession, UidSet,
};
use tempfile::TempDir;

const USER: &str = "engine8";
const IMAP_PASSWORD: &str = "engine";
const OWNER_PASSWORD: &str = "correct horse battery";
const RECENT: &str = "Q3 recent harbor";
const OLD: &str = "Q3 ancient lantern";
const FIRST: &str = "Q3 offline send first";
const SECOND: &str = "Q3 offline send second";
const SECOND_OP: &str = "q3-send-second";
const OLDER_MESSAGE: &str = "Older message needs a connection";
const SYNC: Duration = Duration::from_secs(60);
const START: Duration = Duration::from_secs(90);
const STEP: Duration = Duration::from_secs(30);
/// The replica retries an unreachable server once a minute; the probe then
/// waits up to two minutes for the send to finish.
const ONLINE: Duration = Duration::from_secs(240);
/// Long enough for a duplicate send or Sent copy to show up.
const SETTLE: Duration = Duration::from_secs(10);

fn addr(var: &str, default: &str) -> (String, u16) {
    let addr = std::env::var(var).unwrap_or_else(|_| default.to_owned());
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"))
}

fn imap_addr() -> (String, u16) {
    addr("TAGMAIL_IMAP_ADDR", "127.0.0.1:10143")
}

fn smtp_addr() -> (String, u16) {
    addr("TAGMAIL_SMTP_ADDR", "127.0.0.1:10025")
}

fn mailpit_addr() -> String {
    std::env::var("TAGMAIL_MAILPIT_API").unwrap_or_else(|_| "127.0.0.1:10825".into())
}

async fn imap() -> TokioSession {
    let (host, port) = imap_addr();
    TokioSession::connect(&{
        let mut config = ServerConfig::new(
            host,
            port,
            Security::Plain,
            USER,
            Password::new(IMAP_PASSWORD),
        );
        config.capability_override = CapabilityOverride::default();
        config
    })
    .await
    .expect("IMAP login")
}

/// Empties the mailboxes this test reads and writes, then appends the two
/// INBOX messages: one dated now (in the window), one from 2024.
async fn seed_imap() {
    let mut s = imap().await;
    for mailbox in ["INBOX", "Sent", "Drafts"] {
        s.select(mailbox, SelectOptions::default()).await.unwrap();
        let all = s.uid_search(SearchCriteria::All).await.unwrap();
        if !all.is_empty() {
            s.expunge_uids(&all).await.unwrap();
        }
    }
    let recent = format!(
        "From: Ann <ann@example.com>\r\nTo: {USER}@example.com\r\nSubject: {RECENT}\r\n\
         Message-ID: <q3-recent@example.com>\r\n\r\nA seagull in the harbor.\r\n"
    );
    let old = format!(
        "From: Ann <ann@example.com>\r\nTo: {USER}@example.com\r\nSubject: {OLD}\r\n\
         Message-ID: <q3-old@example.com>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\r\n\
         A lighthouse with a lantern.\r\n"
    );
    for raw in [recent, old] {
        s.append("INBOX", &[], raw.as_bytes()).await.unwrap();
    }
}

/// Messages in `mailbox` with `subject`, seen by a second IMAP client.
async fn imap_count(mailbox: &str, subject: &str) -> usize {
    let mut s = imap().await;
    s.select(mailbox, SelectOptions::default()).await.unwrap();
    let found: UidSet = s
        .uid_search(SearchCriteria::Header {
            name: "Subject".into(),
            value: subject.into(),
        })
        .await
        .unwrap();
    found.iter().count()
}

async fn mailpit(method: reqwest::Method, path: &str) -> Value {
    reqwest::Client::new()
        .request(method, format!("http://{}{path}", mailpit_addr()))
        .send()
        .await
        .expect("Mailpit API")
        .json()
        .await
        .unwrap_or(Value::Null)
}

fn subject_query(subject: &str) -> String {
    let query = format!("subject:\"{subject}\"");
    percent_encoding::utf8_percent_encode(&query, percent_encoding::NON_ALPHANUMERIC).to_string()
}

async fn mailpit_count(subject: &str) -> u64 {
    let found = mailpit(
        reqwest::Method::GET,
        &format!("/api/v1/search?query={}", subject_query(subject)),
    )
    .await;
    found["messages_count"].as_u64().unwrap_or(0)
}

async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

/// tagmail-server with its sync engine on a fixed port, restartable on the
/// same data.
struct Stack {
    data: TempDir,
    keys: TempDir,
    listen: SocketAddr,
    server: Option<Server>,
    cookie: String,
    http: Option<reqwest::Client>,
}

impl Stack {
    fn config(&self) -> Config {
        Config {
            listen: self.listen,
            data_dir: self.data.path().to_owned(),
            master_key: self.keys.path().join("master.key"),
            init_key: true,
            insecure_plain_imap: true,
            max_sse_clients: 4,
            auth: Default::default(),
            web_dir: None,
        }
    }

    async fn new() -> Stack {
        let listen = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut stack = Stack {
            data: tempfile::tempdir().unwrap(),
            keys: tempfile::tempdir().unwrap(),
            listen,
            server: None,
            cookie: String::new(),
            http: None,
        };
        // Starting the server installs the TLS provider reqwest needs.
        stack.start().await;
        stack.http = Some(reqwest::Client::new());
        let token = stack.server().auth().setup_token().unwrap();
        let resp = stack
            .post(
                "/api/auth/setup",
                json!({ "token": token, "password": OWNER_PASSWORD }),
            )
            .await;
        assert_eq!(resp.status(), 201);
        let cookie = resp.headers()["set-cookie"].to_str().unwrap();
        stack.cookie = cookie.split(';').next().unwrap().to_owned();
        stack
    }

    async fn start(&mut self) {
        let engine = tagmail_engine::EngineConfig::default();
        self.server = Some(
            Server::start_with_engine(&self.config(), engine)
                .await
                .unwrap(),
        );
    }

    async fn stop(&mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
    }

    fn http(&self) -> &reqwest::Client {
        self.http.as_ref().expect("client after the first start")
    }

    fn server(&self) -> &Server {
        self.server.as_ref().expect("server running")
    }

    fn url(&self) -> String {
        format!("http://{}", self.listen)
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http()
            .post(format!("{}{path}", self.url()))
            .header("cookie", &self.cookie)
            .header("x-requested-with", "tagmail")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        self.http()
            .get(format!("{}{path}", self.url()))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn add_account(&self) {
        let (imap_host, imap_port) = imap_addr();
        let (smtp_host, smtp_port) = smtp_addr();
        let resp = self
            .post(
                "/api/accounts",
                json!({
                    "name": "Q3",
                    "imapHost": imap_host,
                    "imapPort": imap_port,
                    "imapTls": "starttls",
                    "smtpHost": smtp_host,
                    "smtpPort": smtp_port,
                    "smtpTls": "starttls",
                    "username": USER,
                    "password": IMAP_PASSWORD,
                    "email": format!("{USER}@example.com"),
                }),
            )
            .await;
        assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    }

    async fn inbox_subjects(&self) -> Vec<String> {
        let resp = self
            .http()
            .get(format!("{}/api/views/inbox/index", self.url()))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        let ids = tagmail_api_core::id_index::decode(&resp.bytes().await.unwrap()).unwrap();
        if ids.is_empty() {
            return Vec::new();
        }
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        let rows = self
            .get(&format!("/api/threads?ids={}", ids.join(",")))
            .await;
        rows["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["subject"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn pairing_code(&self) -> String {
        let pairing: Value = self
            .post("/api/pairing", json!({}))
            .await
            .json()
            .await
            .unwrap();
        pairing["code"].as_str().unwrap().to_owned()
    }

    async fn outbox(&self, op_id: &str) -> Vec<Value> {
        let outbox = self.get("/api/outbox").await;
        outbox["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["opId"] == op_id)
            .cloned()
            .collect()
    }
}

fn probe(line: &str) -> Value {
    serde_json::from_str(line.trim_start_matches("DESKTOP_PROBE ")).unwrap()
}

async fn next_probe(app: &mut Lines, stage: &str, timeout: Duration) -> Value {
    let report = probe(
        &app.wait(stage, timeout, |l| l.starts_with("DESKTOP_PROBE "))
            .await,
    );
    assert_eq!(report["stage"], stage, "{report}");
    report
}

#[tokio::test(flavor = "multi_thread")]
async fn the_desktop_works_offline_and_sends_exactly_once() {
    let mut stack = Stack::new().await;
    seed_imap().await;
    for subject in [FIRST, SECOND] {
        mailpit(
            reqwest::Method::DELETE,
            &format!("/api/v1/search?query={}", subject_query(subject)),
        )
        .await;
    }
    stack.add_account().await;
    eventually("the server synced both messages", SYNC, || async {
        let subjects = stack.inbox_subjects().await;
        subjects.contains(&RECENT.to_owned()) && subjects.contains(&OLD.to_owned())
    })
    .await;

    let desktop = tempfile::tempdir().unwrap();
    let launch = |pair: Option<String>| {
        let mut command = app_command(desktop.path());
        match pair {
            Some(pair) => {
                command
                    .env("TAGMAIL_DESKTOP_PAIR", pair)
                    .env("TAGMAIL_DESKTOP_PROBE", "offline");
            }
            None => {
                command.env_remove("TAGMAIL_DESKTOP_PROBE");
            }
        }
        Lines::spawn(command)
    };
    let step = |name: &str| std::fs::write(desktop.path().join(format!("data/probe-{name}")), "");
    let mut app = launch(Some(format!(
        "{},{}",
        stack.url(),
        stack.pairing_code().await
    )));
    app.wait("the live replica", START, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;

    stack.stop().await;
    step("offline").unwrap();
    let offline = next_probe(&mut app, "offline", STEP).await;
    let subjects = offline["subjects"].as_array().unwrap();
    assert!(subjects.contains(&json!(RECENT)), "{offline}");
    assert!(
        subjects.contains(&json!(OLD)),
        "list keeps out-of-window rows: {offline}"
    );
    assert_eq!(offline["recent"]["status"], 200, "{offline}");
    assert!(
        offline["recent"]["body"]
            .as_str()
            .unwrap()
            .contains("A seagull in the harbor."),
        "{offline}"
    );
    let search = &offline["searchRecent"];
    assert_eq!(search["partial"], "offline", "{offline}");
    assert!(
        search["ids"].as_array().unwrap().contains(&search["want"]),
        "{offline}"
    );
    let older = &offline["searchOld"];
    assert_eq!(older["partial"], "offline", "{offline}");
    assert_eq!(
        older["ids"],
        json!([]),
        "out-of-window text is not local: {offline}"
    );
    assert_eq!(offline["old"]["status"], 503, "{offline}");
    assert!(
        offline["old"]["body"]
            .as_str()
            .unwrap()
            .contains(OLDER_MESSAGE),
        "{offline}"
    );
    assert!(offline["send"]["send"].as_u64().unwrap() < 300, "{offline}");
    assert_eq!(offline["send"]["outbox"], json!(["queued"]), "{offline}");
    assert_eq!(mailpit_count(FIRST).await, 0, "nothing sent while offline");

    stack.start().await;
    step("online").unwrap();
    let online = next_probe(&mut app, "online", ONLINE).await;
    assert!(
        [json!([]), json!(["sent"])].contains(&online["outbox"]),
        "{online}"
    );
    eventually("the first send in Mailpit", SYNC, || async {
        mailpit_count(FIRST).await >= 1
    })
    .await;
    eventually("the first send in Sent", SYNC, || async {
        imap_count("Sent", FIRST).await >= 1
    })
    .await;

    stack.stop().await;
    step("offline2").unwrap();
    let offline2 = next_probe(&mut app, "offline2", STEP).await;
    assert_eq!(offline2["send"]["outbox"], json!(["queued"]), "{offline2}");
    stack.start().await;
    eventually("the server taking the second send", ONLINE, || async {
        !stack.outbox(SECOND_OP).await.is_empty()
    })
    .await;
    drop(app);
    let mut app = launch(None);
    app.wait("the relaunched app live", START, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;
    eventually("the second send in Sent", SYNC, || async {
        imap_count("Sent", SECOND).await >= 1
    })
    .await;
    tokio::time::sleep(SETTLE).await;

    for subject in [FIRST, SECOND] {
        assert_eq!(mailpit_count(subject).await, 1, "{subject} submitted once");
        assert_eq!(
            imap_count("Sent", subject).await,
            1,
            "{subject} in Sent once"
        );
    }
    assert_eq!(stack.outbox(SECOND_OP).await.len(), 1);
    drop(app);
    stack.stop().await;
}
