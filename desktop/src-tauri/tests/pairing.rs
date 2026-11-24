//! Pairing, syncing and unpairing through the desktop's local routes against
//! an in-process server, without the webview.
#![cfg(unix)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Env, SEEDED_THREADS, eventually};
use http::Method;
use serde_json::{Value, json};
use tagmail_api_core::id_index;
use tagmail_desktop::desktop::{Desktop, STATUS};
use tagmail_desktop::events::{Emit, EventControl};
use tagmail_desktop::protocol::{Asset, Assets, HttpResponse, Shell};

const SYNC: Duration = Duration::from_secs(10);

struct NoAssets;

impl Assets for NoAssets {
    fn get(&self, _path: &str) -> Option<Asset> {
        None
    }
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(String, Value)>>>);

impl Emit for Recorder {
    fn emit(&self, event: &str, payload: Value) {
        self.0.lock().unwrap().push((event.to_owned(), payload));
    }
}

async fn call(shell: &Shell<Desktop>, method: Method, path: &str, body: Value) -> HttpResponse {
    let req = http::Request::builder()
        .method(method)
        .uri(format!("app://localhost{path}"))
        .body(if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        })
        .unwrap();
    shell.handle(&req).await
}

async fn upload(shell: &Shell<Desktop>, name: &str, content: &str) -> HttpResponse {
    let body = format!(
        "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: text/plain\r\n\r\n{content}\r\n--b--\r\n"
    );
    let req = http::Request::builder()
        .method(Method::POST)
        .uri("app://localhost/api/attachments")
        .header("content-type", "multipart/form-data; boundary=b")
        .body(body.into_bytes())
        .unwrap();
    shell.handle(&req).await
}

fn json_of(resp: &HttpResponse) -> Value {
    serde_json::from_slice(resp.body()).unwrap()
}

async fn open(env: &Env, emitted: &Recorder) -> (Arc<Desktop>, Shell<Desktop>) {
    let desktop = Desktop::open(env.desktop.path(), false, Arc::new(emitted.clone()))
        .await
        .unwrap();
    let (control, _) = EventControl::new();
    (
        Arc::clone(&desktop),
        Shell::new(desktop, Box::new(NoAssets), control),
    )
}

async fn inbox(shell: &Shell<Desktop>) -> Option<Vec<i64>> {
    let resp = call(shell, Method::GET, "/api/views/inbox/index", Value::Null).await;
    (resp.status() == 200).then(|| id_index::decode(resp.body()).unwrap())
}

#[tokio::test]
async fn pairs_syncs_resumes_and_unpairs() {
    let env = Env::new();
    let owner = env.start().await;
    let emitted = Recorder::default();
    let (desktop, shell) = open(&env, &emitted).await;
    assert_eq!(desktop.unread_inbox().await, None);

    let before = call(&shell, Method::GET, "/api/views/inbox/index", Value::Null).await;
    assert_eq!(json_of(&before)["error"], "unpaired");

    let wrong = call(
        &shell,
        Method::POST,
        "/api/desktop/pair",
        json!({ "server": owner.url(), "code": "WRONG123" }),
    )
    .await;
    assert_eq!(wrong.status(), 400, "{:?}", json_of(&wrong));
    assert_eq!(json_of(&wrong)["error"], "invalid_code");

    let code = owner.pairing_code().await;
    let paired = call(
        &shell,
        Method::POST,
        "/api/desktop/pair",
        json!({ "server": owner.url(), "code": code }),
    )
    .await;
    assert_eq!(paired.status(), 200, "{:?}", json_of(&paired));
    assert_eq!(json_of(&paired)["paired"], true);
    assert_eq!(json_of(&paired)["server"], owner.url());
    assert_eq!(owner.devices().await.len(), 1);

    eventually("the bootstrap", SYNC, || async {
        inbox(&shell)
            .await
            .is_some_and(|ids| ids.len() == SEEDED_THREADS as usize)
    })
    .await;
    eventually("the live phase", SYNC, || async {
        json_of(&call(&shell, Method::GET, "/api/desktop/status", Value::Null).await)["phase"]
            ["kind"]
            == "live"
    })
    .await;
    let status = json_of(&call(&shell, Method::GET, "/api/desktop/status", Value::Null).await);
    assert_eq!(status["online"], true);
    assert_eq!(
        status["warnings"][0],
        "No system keychain is available, so the device token is kept in a file only you can read."
    );
    assert!(emitted.0.lock().unwrap().iter().any(|(e, _)| e == STATUS));

    owner.arrive(50, "Fresh", "bob@x.example").await;
    eventually("the pulled thread", SYNC, || async {
        inbox(&shell)
            .await
            .is_some_and(|ids| ids.first() == Some(&50))
    })
    .await;
    assert_eq!(desktop.unread_inbox().await, Some(1));

    let op = call(
        &shell,
        Method::POST,
        "/api/ops",
        json!({ "opId": "7a1c9a52-50a4-4f63-9d3e-8f3f8f7c0b11", "kind": "star", "threadIds": [1] }),
    )
    .await;
    assert!(op.status().is_success(), "{:?}", json_of(&op));
    let rows = json_of(&call(&shell, Method::GET, "/api/threads?ids=1", Value::Null).await);
    assert_eq!(rows["threads"][0]["starred"], true);

    let attachment = upload(&shell, "notes.txt", "hello").await;
    assert_eq!(attachment.status(), 201, "{:?}", json_of(&attachment));
    assert_eq!(json_of(&attachment)["name"], "notes.txt");
    assert_eq!(json_of(&attachment)["size"], 5);
    let draft = call(
        &shell,
        Method::POST,
        "/api/drafts",
        json!({
            "identityId": 3,
            "subject": "Offline first",
            "attachmentIds": [json_of(&attachment)["id"]],
            "clientId": "0b6c1d36-8a51-4d1e-9c52-2f6a4b0f3c11"
        }),
    )
    .await;
    assert!(draft.status().is_success(), "{:?}", json_of(&draft));
    assert_eq!(json_of(&draft)["attachments"][0]["name"], "notes.txt");

    drop((desktop, shell));
    let (_, resumed) = open(&env, &emitted).await;
    eventually("the resumed replica", SYNC, || async {
        inbox(&resumed)
            .await
            .is_some_and(|ids| ids.len() == SEEDED_THREADS as usize + 1)
    })
    .await;

    let unpaired = call(&resumed, Method::POST, "/api/desktop/unpair", Value::Null).await;
    assert_eq!(unpaired.status(), 204);
    assert!(!env.desktop.path().join("device-token").exists());
    let session = call(&resumed, Method::GET, "/api/auth/session", Value::Null).await;
    assert_eq!(json_of(&session)["error"], "unpaired");
}

#[tokio::test]
async fn plain_http_pairs_only_with_loopback() {
    let env = Env::new();
    let emitted = Recorder::default();
    let (_, shell) = open(&env, &emitted).await;
    for server in ["http://mail.example.com", "http://192.168.1.10:8080"] {
        let refused = call(
            &shell,
            Method::POST,
            "/api/desktop/pair",
            json!({ "server": server, "code": "ABCD1234" }),
        )
        .await;
        assert_eq!(refused.status(), 422, "{server}");
        assert_eq!(json_of(&refused)["field"], "server");
    }
}

#[tokio::test]
async fn a_stored_plain_http_server_is_not_resumed() {
    let env = Env::new();
    let durable =
        tagmail_replica::Durable::open(&tagmail_replica::Layout::new(env.desktop.path()).durable())
            .unwrap();
    durable
        .set_meta("server_url", "http://mail.example.com/")
        .unwrap();
    drop(durable);
    std::fs::write(env.desktop.path().join("device-token"), "token").unwrap();

    let (desktop, shell) = open(&env, &Recorder::default()).await;
    assert!(desktop.replica().is_none());
    let status = json_of(&call(&shell, Method::GET, "/api/desktop/status", Value::Null).await);
    assert_eq!(status["paired"], false);
    assert!(
        status["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!(tagmail_desktop::desktop::INSECURE_WARNING)),
        "{status}"
    );
}

async fn pair(shell: &Shell<Desktop>, owner: &common::Owner) -> Value {
    let code = owner.pairing_code().await;
    let paired = call(
        shell,
        Method::POST,
        "/api/desktop/pair",
        json!({ "server": owner.url(), "code": code }),
    )
    .await;
    assert_eq!(paired.status(), 200, "{:?}", json_of(&paired));
    json_of(&paired)
}

fn host(owner: &common::Owner) -> String {
    owner.url().trim_start_matches("http://").to_owned()
}

#[tokio::test]
async fn another_server_keeps_unsent_work_local_and_says_so() {
    let env_a = Env::new();
    let a = env_a.start().await;
    let env_b = Env::new();
    let b = env_b.start().await;
    let emitted = Recorder::default();
    let (_, shell) = open(&env_a, &emitted).await;
    pair(&shell, &a).await;

    eventually("the identities", SYNC, || async {
        let resp = call(&shell, Method::GET, "/api/identities", Value::Null).await;
        resp.status() == 200 && !json_of(&resp)["identities"][0]["id"].is_null()
    })
    .await;
    let identities = call(&shell, Method::GET, "/api/identities", Value::Null).await;
    let draft = call(
        &shell,
        Method::POST,
        "/api/drafts",
        json!({
            "identityId": json_of(&identities)["identities"][0]["id"],
            "to": [{ "email": "bob@x.example" }],
            "cc": [],
            "bcc": [],
            "subject": "For server A",
            "text": "hello",
            "attachmentIds": [],
            "saveNow": false,
        }),
    )
    .await;
    assert!(draft.status().is_success(), "{:?}", json_of(&draft));

    let warned = format!(
        "1 unsent change for {} stays on this device and is not sent to {}.",
        host(&a),
        host(&b)
    );
    let status = pair(&shell, &b).await;
    assert!(
        status["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!(warned)),
        "{status}"
    );

    call(&shell, Method::POST, "/api/desktop/unpair", Value::Null).await;
    let again = pair(&shell, &b).await;
    assert!(
        !again["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!(warned)),
        "{again}"
    );
}

#[tokio::test]
async fn the_data_dir_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let fresh = root.path().join("fresh");
    let existing = root.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
    for dir in [fresh, existing] {
        Desktop::open(&dir, false, Arc::new(Recorder::default()))
            .await
            .unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
}

#[tokio::test]
async fn pairing_and_unpairing_wait_for_the_old_replica_to_stop() {
    let env = Env::new();
    let owner = env.start().await;
    let (desktop, shell) = open(&env, &Recorder::default()).await;
    pair(&shell, &owner).await;
    let first = Arc::downgrade(&desktop.replica().unwrap());

    pair(&shell, &owner).await;
    assert!(first.upgrade().is_none(), "the first replica still runs");
    let second = Arc::downgrade(&desktop.replica().unwrap());

    let unpaired = call(&shell, Method::POST, "/api/desktop/unpair", Value::Null).await;
    assert_eq!(unpaired.status(), 204);
    assert!(second.upgrade().is_none(), "the second replica still runs");
}
