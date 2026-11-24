//! AC 17 on Linux: mail the replica pulls into the INBOX raises a D-Bus
//! notification and the tray count follows the unread INBOX.
#![cfg(target_os = "linux")]

#[path = "common/app.rs"]
mod app;
mod common;

use std::process::Command;
use std::time::Duration;

use app::{LIVE, Lines, app_command};
use common::Env;

const START: Duration = Duration::from_secs(60);
const DELIVERY: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn new_inbox_mail_notifies_and_updates_the_tray() {
    let env = Env::new();
    let owner = env.start().await;
    let mut monitor = Command::new("dbus-monitor");
    monitor.args([
        "--session",
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
    ]);
    let mut monitor = Lines::spawn(monitor);

    let mut command = app_command(env.desktop.path());
    let pair = format!("{},{}", owner.url(), owner.pairing_code().await);
    command.env("TAGMAIL_DESKTOP_PAIR", pair);
    let mut app = Lines::spawn(command);
    app.wait("the live replica", START, |l| {
        l.starts_with("DESKTOP_STATUS") && l.contains(LIVE)
    })
    .await;

    owner.arrive(50, "Quarterly report", "ann@x.example").await;
    monitor
        .wait("the Notify call", DELIVERY, |l| {
            l.contains("\"Quarterly report\"")
        })
        .await;
    app.wait("the tray count", DELIVERY, |l| l == "DESKTOP_TRAY 1")
        .await;
}
