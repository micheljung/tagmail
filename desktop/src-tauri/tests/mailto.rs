//! AC 18 on Linux: with the app as the default `mailto:` handler, a link
//! opened by `xdg-open` starts a second instance that hands the link to
//! the running one, which opens a prefilled compose window.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;
mod common;

use std::process::Command;
use std::time::Duration;

use app::{LIVE, Lines, app_command, isolate};
use common::Env;
use serde_json::Value;

const START: Duration = Duration::from_secs(60);
const OPEN: Duration = Duration::from_secs(20);
const LINK: &str = "mailto:ann@x.example,bob@x.example?cc=cy@x.example&subject=Hello%20there\
                    &body=Line%201%0D%0ALine%202&attach=%2Fetc%2Fpasswd";

#[tokio::test(flavor = "multi_thread")]
async fn a_mailto_link_opens_compose_in_the_running_app() {
    let env = Env::new();
    let owner = env.start().await;
    let root = env.desktop.path();
    let mut command = app_command(root);
    let pair = format!("{},{}", owner.url(), owner.pairing_code().await);
    command
        .env("TAGMAIL_DESKTOP_PAIR", pair)
        .env("TAGMAIL_DESKTOP_PROBE", "compose");
    let mut app = Lines::spawn(command);
    app.wait("the live replica", START, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;

    let applications = root.join("xdg-data/applications");
    std::fs::create_dir_all(&applications).unwrap();
    std::fs::write(
        applications.join("tagmail-test.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Tagmail\nExec={} %u\n\
             MimeType=x-scheme-handler/mailto;\n",
            env!("CARGO_BIN_EXE_tagmail-desktop")
        ),
    )
    .unwrap();
    let xdg = |program: &str, args: &[&str]| {
        let mut command = Command::new(program);
        command.args(args);
        isolate(&mut command, root);
        let status = command.status().unwrap();
        assert!(status.success(), "{program} {args:?}: {status}");
    };
    xdg(
        "xdg-mime",
        &["default", "tagmail-test.desktop", "x-scheme-handler/mailto"],
    );
    xdg("xdg-open", &[LINK]);

    let line = app
        .wait("the compose probe", OPEN, |l| {
            l.starts_with("DESKTOP_PROBE ")
        })
        .await;
    let fields: Value = serde_json::from_str(line.trim_start_matches("DESKTOP_PROBE ")).unwrap();
    assert_eq!(
        fields["to"],
        serde_json::json!(["ann@x.example", "bob@x.example"])
    );
    assert_eq!(fields["cc"], serde_json::json!(["cy@x.example"]));
    assert_eq!(fields["subject"], "Hello there");
    assert_eq!(fields["body"].as_str().unwrap().trim(), "Line 1\nLine 2");
}
