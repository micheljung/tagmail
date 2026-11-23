//! Cost of bootstrapping a desktop replica from the benchmark server
//! (plan §4 M5, reported, not gated): a device is paired, the snapshot is
//! streamed once to measure its size and the server's peak RSS, then
//! imported by `tagmail-replica` as the desktop app does.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tagmail_replica::{Layout, Phase, Replica, ServerClient, Settings};

use crate::bench_rss::{Running, http, proc_status_kib, reset_peak_rss};

const SNAPSHOT_READ_TIMEOUT: Duration = Duration::from_secs(120);
const DEVICE_NAME: &str = "bench";

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

/// Pairs a device through the owner session; returns its bearer token.
pub(crate) fn pair(running: &Running) -> Result<String> {
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
    let code = code["code"].as_str().context("pairing code")?;
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
    Ok((body_bytes, elapsed, proc_status_kib(pid, "VmHWM")?))
}

fn disk_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
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
        let replica: Arc<Replica> =
            Replica::open(Layout::new(dir), client, Settings::default()).await?;
        let started = Instant::now();
        replica.rebootstrap().await?;
        let elapsed = started.elapsed().as_secs_f64();
        let records = match *replica.phase().borrow() {
            Phase::Bootstrapping { records } => records,
            ref other => bail!("replica phase after the bootstrap: {other:?}"),
        };
        Ok((elapsed, records))
    })
}

pub(crate) fn measure(running: &Running, pid: u32, work: &Path) -> Result<Cost> {
    let token = pair(running)?;
    let (ndjson_bytes, snapshot_seconds, server_peak_kib) = stream_snapshot(running, pid, &token)?;
    let dir = work.join("replica-bench");
    let (bootstrap_seconds, records) = bootstrap(running, &token, &dir)?;
    Ok(Cost {
        ndjson_bytes,
        snapshot_seconds,
        server_peak_kib,
        bootstrap_seconds,
        records,
        replica_disk_bytes: disk_bytes(&dir)?,
    })
}
