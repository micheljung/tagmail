//! The main webview stays on the app: web and mail links open in the
//! system's handlers (here a script that records them), other links and
//! navigations away are dropped.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;

use std::process::Command;
use std::time::{Duration, Instant};

use app::{Lines, app_command, isolate};
use serde_json::Value;

const START: Duration = Duration::from_secs(60);
const OPENED: Duration = Duration::from_secs(20);

fn probe(field: String) -> impl Fn(&str) -> bool {
    move |l| l.starts_with("DESKTOP_PROBE ") && l.contains(&field)
}

#[tokio::test(flavor = "multi_thread")]
async fn links_leave_the_app_only_for_the_system_handlers() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let opened = root.join("opened");
    let handler = root.join("handler.sh");
    std::fs::write(
        &handler,
        format!("#!/bin/sh\necho \"$1\" >> {}\n", opened.display()),
    )
    .unwrap();
    std::fs::set_permissions(
        &handler,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let applications = root.join("xdg-data/applications");
    std::fs::create_dir_all(&applications).unwrap();
    std::fs::write(
        applications.join("recorder.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Recorder\nExec={} %u\n\
             MimeType=x-scheme-handler/http;x-scheme-handler/https;x-scheme-handler/mailto;\n",
            handler.display()
        ),
    )
    .unwrap();
    for scheme in ["http", "https", "mailto"] {
        let mut command = Command::new("xdg-mime");
        command.args([
            "default",
            "recorder.desktop",
            &format!("x-scheme-handler/{scheme}"),
        ]);
        isolate(&mut command, root);
        assert!(command.status().unwrap().success());
    }

    let mut command = app_command(root);
    command
        .env_remove("BROWSER")
        .env("TAGMAIL_DESKTOP_PROBE", "links");
    let mut app = Lines::spawn(command);
    app.wait("the links probe", START, probe(r#""stage":"ready""#.into()))
        .await;
    for left in (0..3).rev() {
        let clicked = Command::new("xdotool")
            .args([
                "search",
                "--sync",
                "--name",
                "^Tagmail$",
                "mousemove",
                "--window",
                "%1",
                "200",
                "200",
                "click",
                "1",
            ])
            .status()
            .unwrap();
        assert!(clicked.success());
        app.wait("the click", OPENED, probe(format!(r#""left":{left}"#)))
            .await;
    }
    let line = app
        .wait("the links probe", OPENED, probe(r#""stage":"done""#.into()))
        .await;
    let report: Value = serde_json::from_str(line.trim_start_matches("DESKTOP_PROBE ")).unwrap();
    assert!(
        report["href"]
            .as_str()
            .unwrap()
            .starts_with("app://localhost/"),
        "{report}"
    );

    let want = [
        "https://example.com/new-window",
        "mailto:ann@x.example",
        "https://example.com/top",
    ];
    let deadline = Instant::now() + OPENED;
    let mut seen = String::new();
    while Instant::now() < deadline {
        seen = std::fs::read_to_string(&opened).unwrap_or_default();
        if want.iter().all(|w| seen.lines().any(|l| l == *w)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut lines: Vec<&str> = seen.lines().collect();
    lines.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    assert_eq!(lines, want, "only web and mail links reach the system");
}
