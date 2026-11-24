//! In-webview self-test (feature `test-hooks`): fixture data in the local
//! stores, a script injected into the real SPA that checks transport, CSP,
//! iframe and event behaviour, and the routes it reports through. Ports the
//! M1 spike's checks onto the shipped SPA and render route.

use std::collections::BTreeMap;
use std::sync::Mutex;

use http::{Method, StatusCode, header};
use serde_json::{Value, json};
use tagmail_api_core::api::APP_CSP;
use tagmail_core::{AccountId, EmailId};
use tagmail_store::{BlobKey, ChangeOp, DEFAULT_RAW_CAP, Entity, NewChange, changes};
use tokio::sync::mpsc;

use crate::protocol::HttpResponse;
use crate::state::Local;

pub const PREFIX: &str = "/__selftest/";
const PROBE_PREFIX: &str = "/__probe/";
/// `TAGMAIL_DESKTOP_PROBE=compose` reports compose windows; `writes` runs
/// the offline write check step by step.
pub fn probe_head(kind: &str) -> String {
    let script = if kind == "writes" {
        "writes.js"
    } else {
        "probe.js"
    };
    format!(r#"<script type="module" src="/__probe/{script}"></script>"#)
}
pub const INDEX_HEAD: &str = r#"<script type="module" src="/__selftest/selftest.js"></script>"#;
pub const THREADS: i64 = 60;
pub const RENDER_MESSAGE: i64 = 1;
const PIXEL_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

pub struct SelfTest {
    local: Local,
    beacons: Mutex<BTreeMap<String, u64>>,
    reports: mpsc::UnboundedSender<Value>,
}

impl SelfTest {
    pub fn new(local: Local) -> (SelfTest, mpsc::UnboundedReceiver<Value>) {
        let (reports, rx) = mpsc::unbounded_channel();
        let test = SelfTest {
            local,
            beacons: Mutex::default(),
            reports,
        };
        (test, rx)
    }

    /// `None` for paths outside [`PREFIX`].
    pub async fn handle(&self, req: &http::Request<Vec<u8>>) -> Option<HttpResponse> {
        let rest = req.uri().path().strip_prefix(PREFIX)?;
        let resp = match (req.method(), rest) {
            (&Method::GET, "selftest.js") => {
                file("text/javascript", include_str!("../selftest/selftest.js"))
            }
            (&Method::GET, "control.html") => {
                file("text/html", include_str!("../selftest/control.html"))
            }
            (&Method::GET, "beacon.js") => {
                file("text/javascript", include_str!("../selftest/beacon.js"))
            }
            (&Method::GET, "beacons") => {
                json_response(&json!(*self.beacons.lock().expect("beacons lock")))
            }
            (&Method::GET, beacon) if beacon.starts_with("beacon/") => {
                let key = beacon.trim_start_matches("beacon/").to_owned();
                *self
                    .beacons
                    .lock()
                    .expect("beacons lock")
                    .entry(key)
                    .or_default() += 1;
                http::Response::builder()
                    .header(header::CONTENT_TYPE, "image/gif")
                    .body(PIXEL_GIF.to_vec())
                    .expect("valid response")
            }
            (&Method::POST, "touch") => match touch(&self.local).await {
                Ok(id) => json_response(&json!({ "threadId": id })),
                Err(e) => status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            },
            (&Method::POST, "report") => match serde_json::from_slice::<Value>(req.body()) {
                Ok(report) => {
                    let _ = self.reports.send(report);
                    json_response(&json!({}))
                }
                Err(e) => status(StatusCode::BAD_REQUEST, &e.to_string()),
            },
            _ => status(StatusCode::NOT_FOUND, "no such self-test route"),
        };
        Some(resp)
    }
}

/// Probe routes: scripts, steps a test unlocks by creating
/// `<data dir>/probe-<name>`, and reports printed as `DESKTOP_PROBE {json}`
/// lines. `None` outside its paths.
pub fn probe(req: &http::Request<Vec<u8>>) -> Option<HttpResponse> {
    let rest = req.uri().path().strip_prefix(PROBE_PREFIX)?;
    Some(match (req.method(), rest) {
        (&Method::GET, "probe.js") => file("text/javascript", include_str!("../selftest/probe.js")),
        (&Method::GET, "writes.js") => {
            file("text/javascript", include_str!("../selftest/writes.js"))
        }
        (&Method::GET, "step") => {
            let name = form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.into_owned())
                .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric()));
            let dir = std::env::var_os("TAGMAIL_DESKTOP_DATA_DIR");
            match (name, dir) {
                (Some(name), Some(dir))
                    if std::path::Path::new(&dir)
                        .join(format!("probe-{name}"))
                        .exists() =>
                {
                    status(StatusCode::OK, "go")
                }
                _ => status(StatusCode::NO_CONTENT, ""),
            }
        }
        (&Method::POST, "report") => {
            println!("DESKTOP_PROBE {}", String::from_utf8_lossy(req.body()));
            json_response(&json!({}))
        }
        _ => status(StatusCode::NOT_FOUND, "no such probe route"),
    })
}

fn file(content_type: &str, body: &str) -> HttpResponse {
    http::Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_SECURITY_POLICY, APP_CSP)
        .body(body.as_bytes().to_vec())
        .expect("valid response")
}

fn json_response(body: &Value) -> HttpResponse {
    http::Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string().into_bytes())
        .expect("valid response")
}

fn status(status: StatusCode, message: &str) -> HttpResponse {
    http::Response::builder()
        .status(status)
        .body(message.as_bytes().to_vec())
        .expect("valid response")
}

/// Untrusted mail stand-in: a tall body, an image and a script that each
/// phone home, so the test can tell what the render path let through.
fn render_raw() -> String {
    concat!(
        "From: Ann <ann@x.example>\r\n",
        "Subject: Self-test 1\r\n",
        "Content-Type: text/html\r\n",
        "\r\n",
        "<div style=\"height:1500px\">tall</div>",
        "<img src=\"/__selftest/beacon/img-render\">",
        "<script src=\"/__selftest/beacon.js\"></script>",
        "<script>fetch('/__selftest/beacon/script-render')</script>\r\n",
    )
    .to_owned()
}

/// Seeds one account with [`THREADS`] single-message INBOX threads, once.
pub async fn seed(local: &Local) -> tagmail_store::Result<()> {
    let seeded = local
        .store
        .config()
        .write(|tx| {
            let exists: i64 = tx.query_row("SELECT count(*) FROM account", [], |r| r.get(0))?;
            if exists > 0 {
                return Ok(true);
            }
            tx.execute_batch(
                "INSERT INTO account (id, name, email, imap_host, imap_port, imap_tls, smtp_host,
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce) VALUES
                     (1, 'Self-test', 'me@x.example', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'', x'');",
            )?;
            Ok(false)
        })
        .await?;
    if seeded {
        return Ok(());
    }
    local
        .store
        .cache()
        .write(|tx| {
            tx.execute(
                "INSERT INTO mailbox (id, account_id, name, uidvalidity, sync_state)
                 VALUES (1, 1, 'INBOX', 1, 'live')",
                [],
            )?;
            for id in 1..=THREADS {
                insert_thread(tx, id)?;
            }
            Ok(())
        })
        .await?;
    let blobs = local.store.blobs().clone();
    tokio::task::spawn_blocking(move || {
        let key = BlobKey::for_message(AccountId(1), &email_id(RENDER_MESSAGE));
        let mut writer = blobs.raw_writer(key, DEFAULT_RAW_CAP)?;
        writer.write_chunk(render_raw().as_bytes())?;
        writer.finish().map(|_| ())
    })
    .await
    .expect("blob write does not panic")
}

fn email_id(id: i64) -> EmailId {
    EmailId::parse(&format!("g:{id}")).expect("valid email id")
}

fn insert_thread(tx: &tagmail_store::rusqlite::Connection, id: i64) -> tagmail_store::Result<()> {
    let date = 1_700_000_000 + id * 60;
    tx.execute(
        "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred)
         VALUES (?1, 1, ?2, ?3, 1, 1, 0)",
        tagmail_store::rusqlite::params![id, format!("self-test {id}"), date],
    )?;
    tx.execute(
        "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, 1, ?2)",
        tagmail_store::rusqlite::params![id, date],
    )?;
    tx.execute(
        "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet, from_json, has_attachment)
         VALUES (?1, 1, ?2, ?1, ?3, ?4, 'fixture', '[{\"name\":\"Ann\",\"email\":\"ann@x.example\"}]', 0)",
        tagmail_store::rusqlite::params![id, email_id(id).to_string(), date, format!("Self-test {id}")],
    )?;
    tx.execute(
        "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (1, ?1, ?1, 0)",
        [id],
    )?;
    Ok(())
}

/// Adds a thread and publishes the change, as a sync would.
pub async fn touch(local: &Local) -> tagmail_store::Result<i64> {
    let epoch = local.store.cache().epoch();
    let (id, head) = local
        .store
        .cache()
        .write(move |tx| {
            let id: i64 = tx.query_row("SELECT max(id) + 1 FROM thread", [], |r| r.get(0))?;
            insert_thread(tx, id)?;
            changes::append(
                tx,
                &NewChange {
                    entity: Entity::Thread,
                    entity_id: id,
                    op: ChangeOp::Upsert,
                    payload: None,
                },
            )?;
            Ok((id, changes::head(tx, epoch)?))
        })
        .await?;
    local.feed.publish(head);
    Ok(id)
}
