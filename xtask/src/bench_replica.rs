//! Cost of bootstrapping a desktop replica from the benchmark server
//! (plan §4 M5, reported, not gated): a device is paired, the snapshot is
//! streamed once to measure its size and the server's peak RSS, then
//! imported by `tagmail-replica` as the desktop app does.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tagmail_replica::{Layout, Phase, Replica, ServerClient, Settings};

use crate::bench_rss::{Running, check_alive, http, proc_status_kib, reset_peak_rss};
use crate::proc_tree;

const SNAPSHOT_READ_TIMEOUT: Duration = Duration::from_secs(120);
const DEVICE_NAME: &str = "bench";
const DESKTOP_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(1800);
const POLL: Duration = Duration::from_secs(1);
/// `comm` of the app's own process (15 characters, untruncated).
const DESKTOP_PROCESS: &str = "tagmail-desktop";
const WEBKIT_PREFIX: &str = "WebKit";
const DESKTOP_LOG_TAIL: usize = 20;

pub(crate) struct Cost {
    pub(crate) ndjson_bytes: u64,
    pub(crate) snapshot_seconds: f64,
    pub(crate) server_peak_kib: u64,
    pub(crate) bootstrap_seconds: f64,
    pub(crate) records: u64,
    pub(crate) replica_disk_bytes: u64,
}

impl Cost {
    pub(crate) fn json(&self) -> Value {
        json!({
            "ndjsonBytes": self.ndjson_bytes,
            "snapshotSeconds": crate::bench_rss::round(self.snapshot_seconds),
            "serverPeakRssMib": crate::bench_rss::round(self.server_peak_kib as f64 / 1024.0),
            "bootstrapSeconds": crate::bench_rss::round(self.bootstrap_seconds),
            "records": self.records,
            "replicaDiskMb": crate::bench_rss::round(self.replica_disk_bytes as f64 / 1e6),
        })
    }
}

impl Cost {
    pub(crate) fn summary(&self) -> String {
        format!(
            "snapshot {:.1} MB NDJSON in {:.1} s, server peak RSS {:.1} MiB; \
             import {:.1} s, {} records, {:.1} MB on disk",
            self.ndjson_bytes as f64 / 1e6,
            self.snapshot_seconds,
            self.server_peak_kib as f64 / 1024.0,
            self.bootstrap_seconds,
            self.records,
            self.replica_disk_bytes as f64 / 1e6,
        )
    }
}

/// A pairing code from the owner session.
fn pairing_code(running: &Running) -> Result<String> {
    let (status, body) = http(
        running.addr,
        "POST",
        "/api/pairing",
        Some("{}"),
        Some(&running.session),
    )?;
    if status != 200 && status != 201 {
        bail!("POST /api/pairing: {status} {body}");
    }
    let code: Value = serde_json::from_str(&body)?;
    Ok(code["code"].as_str().context("pairing code")?.to_owned())
}

/// Pairs a device through the owner session; returns its bearer token.
pub(crate) fn pair(running: &Running) -> Result<String> {
    let code = pairing_code(running)?;
    let exchange = json!({ "code": code, "name": DEVICE_NAME }).to_string();
    let (status, body) = http(
        running.addr,
        "POST",
        "/api/pairing/exchange",
        Some(&exchange),
        None,
    )?;
    if status != 200 && status != 201 {
        bail!("POST /api/pairing/exchange: {status} {body}");
    }
    let token: Value = serde_json::from_str(&body)?;
    Ok(token["token"].as_str().context("device token")?.to_owned())
}

/// Streams the snapshot once, counting its bytes, with the server's VmHWM
/// reset first.
fn stream_snapshot(running: &Running, pid: u32, token: &str) -> Result<(u64, f64, u64)> {
    reset_peak_rss(pid)?;
    let started = Instant::now();
    let mut stream = TcpStream::connect(running.addr)?;
    stream.set_read_timeout(Some(SNAPSHOT_READ_TIMEOUT))?;
    write!(
        stream,
        "GET /api/replica/snapshot HTTP/1.0\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\r\n",
        running.addr
    )?;
    let mut head = Vec::new();
    let mut buf = vec![0; 64 << 10];
    let mut body_bytes = 0u64;
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if head.ends_with(b"\r\n\r\n") {
            body_bytes += n as u64;
            continue;
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            body_bytes += (head.len() - end - 4) as u64;
            head.truncate(end + 4);
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let head = String::from_utf8_lossy(&head);
    if head.split(' ').nth(1) != Some("200") {
        bail!(
            "GET /api/replica/snapshot: {}",
            head.lines().next().unwrap_or("")
        );
    }
    let peak = proc_status_kib(pid, "VmHWM").context("server VmHWM after the snapshot")?;
    Ok((body_bytes, elapsed, peak))
}

/// Bytes of the files below `dir`; SQLite's journal and temp files may
/// vanish while it walks.
fn disk_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        total += if meta.is_dir() {
            disk_bytes(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

/// Imports the snapshot into `dir` with `tagmail-replica`.
fn bootstrap(running: &Running, token: &str, dir: &Path) -> Result<(f64, u64)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = url::Url::parse(&format!("http://{}/", running.addr))?;
        let client = ServerClient::new(&server, Some(token.to_owned()))?;
        let replica: Arc<Replica> = Replica::open(Layout::new(dir), client, Settings::default())
            .await
            .context("opening the replica")?;
        let started = Instant::now();
        replica.rebootstrap().await.context("bootstrapping")?;
        let elapsed = started.elapsed().as_secs_f64();
        let records = match *replica.phase().borrow() {
            Phase::Bootstrapping { records } => records,
            ref other => bail!("replica phase after the bootstrap: {other:?}"),
        };
        Ok((elapsed, records))
    })
}

pub(crate) fn measure(running: &mut Running, pid: u32, work: &Path) -> Result<Cost> {
    let token = pair(running).context("pairing")?;
    let (ndjson_bytes, snapshot_seconds, server_peak_kib) =
        stream_snapshot(running, pid, &token).context("streaming the snapshot")?;
    check_alive(running).context("after the snapshot")?;
    let dir = work.join("replica-bench");
    let (bootstrap_seconds, records) =
        bootstrap(running, &token, &dir).context("importing the snapshot")?;
    check_alive(running).context("after the import")?;
    Ok(Cost {
        ndjson_bytes,
        snapshot_seconds,
        server_peak_kib,
        bootstrap_seconds,
        records,
        replica_disk_bytes: disk_bytes(&dir).context("measuring the replica")?,
    })
}

pub(crate) struct DesktopRun {
    pub(crate) bootstrap_seconds: f64,
    /// VmRSS per process name of the app's process tree after the idle.
    pub(crate) rss_kib: BTreeMap<String, u64>,
}

impl DesktopRun {
    fn rust_kib(&self) -> u64 {
        self.rss_kib.get(DESKTOP_PROCESS).copied().unwrap_or(0)
    }

    fn webkit_kib(&self) -> u64 {
        self.rss_kib
            .iter()
            .filter(|(name, _)| name.starts_with(WEBKIT_PREFIX))
            .map(|(_, kib)| kib)
            .sum()
    }

    pub(crate) fn json(&self) -> Value {
        let mib = |kib: u64| crate::bench_rss::round(kib as f64 / 1024.0);
        json!({
            "bootstrapSeconds": crate::bench_rss::round(self.bootstrap_seconds),
            "rustRssMib": mib(self.rust_kib()),
            "webkitRssMib": mib(self.webkit_kib()),
            "totalRssMib": mib(self.rust_kib() + self.webkit_kib()),
            "processRssMib": self.rss_kib.iter().map(|(n, k)| (n.clone(), mib(*k))).collect::<BTreeMap<_, _>>(),
        })
    }

    pub(crate) fn summary(&self) -> String {
        let per_process: Vec<String> = self
            .rss_kib
            .iter()
            .filter(|(name, _)| name.starts_with(WEBKIT_PREFIX))
            .map(|(name, kib)| format!("{name} {:.1}", *kib as f64 / 1024.0))
            .collect();
        format!(
            "bootstrap {:.1} s; RSS after idle: app {:.1} MiB + WebKit {:.1} MiB ({}) = {:.1} MiB",
            self.bootstrap_seconds,
            self.rust_kib() as f64 / 1024.0,
            self.webkit_kib() as f64 / 1024.0,
            per_process.join(", "),
            (self.rust_kib() + self.webkit_kib()) as f64 / 1024.0,
        )
    }
}

/// Kills `root` and its descendants, children first.
fn kill_tree(root: u32) {
    let pids: Vec<String> = proc_tree::tree(root)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|(pid, _)| pid.to_string())
        .collect();
    let _ = Command::new("kill").args(&pids).status();
}

/// Starts the desktop app under `dbus-run-session` and `xvfb-run`, pairing it through
/// `TAGMAIL_DESKTOP_PAIR`; waits for its first bootstrap to be installed,
/// idles, and reads the RSS of its process tree.
pub(crate) fn desktop(
    running: &Running,
    binary: &Path,
    work: &Path,
    idle: Duration,
) -> Result<DesktopRun> {
    let code = pairing_code(running)?;
    let data = work.join("desktop-data");
    let log = fs::File::create(work.join("desktop.log"))?;
    let mut child = Command::new("dbus-run-session")
        .args(["--", "xvfb-run", "-a"])
        .arg(binary)
        .env("TAGMAIL_DESKTOP_DATA_DIR", &data)
        .env(
            "TAGMAIL_DESKTOP_PAIR",
            format!("http://{},{code}", running.addr),
        )
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("starting the desktop app under xvfb-run")?;
    let pid = child.id();
    let result = (|| {
        let started = Instant::now();
        let replica = Layout::new(&data);
        while !(replica.replica().join("cache.db").exists() && !replica.replica_tmp().exists()) {
            if let Some(status) = child.try_wait()? {
                bail!("the desktop app exited with {status}");
            }
            if started.elapsed() > DESKTOP_BOOTSTRAP_TIMEOUT {
                bail!(
                    "no bootstrap within {} s",
                    DESKTOP_BOOTSTRAP_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(POLL);
        }
        let bootstrap_seconds = started.elapsed().as_secs_f64();
        std::thread::sleep(idle);
        if let Some(status) = child.try_wait()? {
            bail!("the desktop app exited with {status} while idle");
        }
        Ok(DesktopRun {
            bootstrap_seconds,
            rss_kib: proc_tree::tree_rss_kib(pid)?,
        })
    })();
    kill_tree(pid);
    let _ = child.wait();
    result.with_context(|| {
        let log = fs::read_to_string(work.join("desktop.log")).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(DESKTOP_LOG_TAIL).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        format!("desktop log tail:\n{}", tail.join("\n"))
    })
}
