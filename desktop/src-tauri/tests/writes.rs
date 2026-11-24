//! The spike's offline-write checks on the real path: a write taken while
//! the server is down shows at once and waits as pending; re-sending its op
//! id once the server is back does not make a second op.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;
mod common;

use std::time::Duration;

use app::{LIVE, Lines, app_command};
use common::{Env, eventually};
use serde_json::Value;

const START: Duration = Duration::from_secs(60);
const STEP: Duration = Duration::from_secs(20);
/// The replica retries an unreachable server once a minute.
const RECONNECT: Duration = Duration::from_secs(90);
const OP_ID: &str = "5b0d8f3e-2a8c-4c1e-9f6a-1d2e3f4a5b6c";

fn probe(line: &str) -> Value {
    serde_json::from_str(line.trim_start_matches("DESKTOP_PROBE ")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offline_write_shows_at_once_and_reaches_the_server_once() {
    let env = Env::new();
    let mut owner = env.start().await;
    let root = env.desktop.path();
    let mut command = app_command(root);
    let pair = format!("{},{}", owner.url(), owner.pairing_code().await);
    command
        .env("TAGMAIL_DESKTOP_PAIR", pair)
        .env("TAGMAIL_DESKTOP_PROBE", "writes");
    let mut app = Lines::spawn(command);
    app.wait("the live replica", START, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;

    owner.stop().await;
    std::fs::write(root.join("data/probe-offline"), "").unwrap();
    let offline = probe(
        &app.wait("the offline write", STEP, |l| {
            l.starts_with("DESKTOP_PROBE ")
        })
        .await,
    );
    assert_eq!(offline["stage"], "offline", "{offline}");
    assert!(offline["status"].as_u64().unwrap() < 300, "{offline}");
    assert_eq!(offline["starred"], true, "{offline}");
    assert_eq!(
        offline["pending"],
        serde_json::json!(["pending"]),
        "{offline}"
    );

    owner.restart_with_engine(&env).await;
    eventually("the flushed op", RECONNECT, || async {
        owner.ops().await.iter().any(|o| o["opId"] == OP_ID)
    })
    .await;
    std::fs::write(root.join("data/probe-online"), "").unwrap();
    let replay = probe(
        &app.wait("the replayed write", STEP, |l| {
            l.starts_with("DESKTOP_PROBE ")
        })
        .await,
    );
    assert_eq!(replay["stage"], "replay", "{replay}");
    assert!(replay["status"].as_u64().unwrap() < 300, "{replay}");
    let ops: Vec<Value> = owner
        .ops()
        .await
        .into_iter()
        .filter(|o| o["opId"] == OP_ID)
        .collect();
    assert_eq!(ops.len(), 1, "{ops:?}");
}
