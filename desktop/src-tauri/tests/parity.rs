//! Parity row on the desktop: with 100k threads over two years the app's
//! inbox is as deep as the server's and jumps to row 99,000 within 300 ms,
//! search through the app finds the server's hits, and a message older
//! than the offline window opens from the server.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;
mod common;

use std::time::Duration;

use app::{LIVE, Lines, app_command};
use common::{Env, OLD_BODY, large_subject};
use serde_json::Value;

const THREADS: i64 = 100_000;
const JUMP_BUDGET_MS: f64 = 300.0;
const BOOTSTRAP: Duration = Duration::from_secs(300);
const STAGE: Duration = Duration::from_secs(90);

async fn stage(app: &mut Lines, name: &str) -> Value {
    let line = app
        .wait(name, STAGE, |l| l.starts_with("DESKTOP_PROBE "))
        .await;
    let report: Value = serde_json::from_str(line.trim_start_matches("DESKTOP_PROBE ")).unwrap();
    assert_eq!(report["stage"], name, "{report}");
    report
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_lists_jumps_searches_and_opens_like_the_server() {
    let env = Env::new();
    let owner = env.start_large(THREADS).await;
    let mut command = app_command(env.desktop.path());
    let pair = format!("{},{}", owner.url(), owner.pairing_code().await);
    command
        .env("TAGMAIL_DESKTOP_PAIR", pair)
        .env("TAGMAIL_DESKTOP_PROBE", "parity");
    let mut app = Lines::spawn(command);
    app.wait("the live replica", BOOTSTRAP, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;

    let jump = stage(&mut app, "jump").await;
    let inbox = owner.index("inbox").await;
    assert_eq!(jump["total"], THREADS, "{jump}");
    for j in jump["jumps"].as_array().unwrap() {
        let target = j["target"].as_u64().unwrap() as usize;
        assert_eq!(j["subject"], large_subject(inbox[target]), "{j}");
        assert!(j["ms"].as_f64().unwrap() < JUMP_BUDGET_MS, "{jump}");
    }

    let search = stage(&mut app, "search").await;
    let hits = owner.index("search%3Ainvoice").await;
    assert_eq!(search["total"], hits.len(), "{search}");
    assert_eq!(search["partial"], false, "{search}");

    let open = stage(&mut app, "open").await;
    assert!(open["text"].as_str().unwrap().contains(OLD_BODY), "{open}");
}
