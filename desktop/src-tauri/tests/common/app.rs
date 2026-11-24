//! The desktop app as a child process with test hooks, its stdout read
//! line by line. Run under `dbus-run-session` and `xvfb-run`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

pub struct Lines {
    child: Child,
    rx: mpsc::UnboundedReceiver<String>,
    seen: Vec<String>,
}

impl Lines {
    pub fn spawn(mut command: Command) -> Lines {
        let mut child = command
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {command:?}: {e}"));
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Lines {
            child,
            rx,
            seen: Vec::new(),
        }
    }

    /// The first line not yet matched that satisfies `pred`, including
    /// lines that arrived while waiting for something else.
    pub async fn wait(
        &mut self,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> String {
        if let Some(at) = self.seen.iter().position(|l| pred(l)) {
            return self.seen.remove(at);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.recv()).await {
                Ok(Some(line)) if pred(&line) => return line,
                Ok(Some(line)) => self.seen.push(line),
                _ => panic!(
                    "timed out waiting for {what}; unmatched output:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Points `command` at one app instance's data and XDG dirs below `root`,
/// so it never touches the user's desktop files.
pub fn isolate(command: &mut Command, root: &Path) {
    command.env_remove("TAGMAIL_DESKTOP_SELFTEST");
    for (key, dir) in [
        ("TAGMAIL_DESKTOP_DATA_DIR", "data"),
        ("XDG_DATA_HOME", "xdg-data"),
        ("XDG_CONFIG_HOME", "xdg-config"),
    ] {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        command.env(key, path);
    }
}

pub fn app_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tagmail-desktop"));
    isolate(&mut command, root);
    command
}

pub const LIVE: &str = r#""phase":{"kind":"live"}"#;
