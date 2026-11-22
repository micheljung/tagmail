//! AC 9 RSS gate (plan §3.13, §6 row 9), M1 scenario: one account with the
//! 100k seed in `user1` is synced to the end of the text pass, then the
//! server idles. VmRSS is sampled every second; the gate is the maximum over
//! the last `window` of the idle period.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::harness::{self, output, run};

const SCENARIO: &str = "m1: 1 account x 100k messages synced, then idle";
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const LOG_TAIL_LINES: usize = 50;
const KIB: f64 = 1024.0;
/// Where Dovecot listens inside its container (`test/compose.yml`).
const DOVECOT_INNER_ADDR: &str = "127.0.0.1:31143";
const MUSL_TARGET: &str = "x86_64-unknown-linux-musl";
const RUNNER_IMAGE: &str = "busybox:1.37";
const RUNNER_DIR: &str = "/bench";

#[derive(Debug, Clone)]
pub struct Options {
    /// Prebuilt `tagmail-server`; built in release mode when absent.
    pub server: Option<PathBuf>,
    pub imap: SocketAddr,
    pub user: String,
    pub password: String,
    pub idle: Duration,
    pub window: Duration,
    pub limit_mib: u64,
    pub sync_timeout: Duration,
    pub out: PathBuf,
}

/// Builds static binaries and runs the benchmark in a container sharing the
/// Dovecot container's network namespace, for Docker daemons whose
/// published ports are not reachable from here.
pub fn run_in_dovecot_network(opts: &Options) -> Result<()> {
    let root = workspace_root();
    let mut build = cargo();
    build
        .args(["build", "--release", "--target", MUSL_TARGET])
        .args(["-p", "tagmail-api-server", "--bin", "tagmail-server"])
        .args(["-p", "xtask", "--bin", "xtask"])
        .env("CC_x86_64_unknown_linux_musl", "gcc")
        .env("LIBSQLITE3_FLAGS", "-DSQLITE_DISABLE_LFS")
        .env("CFLAGS_x86_64_unknown_linux_musl", "-U_FORTIFY_SOURCE");
    run(&mut build)?;
    let bin = root.join("target").join(MUSL_TARGET).join("release");

    let dovecot = harness::container(harness::DOVECOT)?;
    let inner = |name: &str| format!("{RUNNER_DIR}/{name}");
    let script = format!(
        "{xtask} bench-rss --server {server} --imap {DOVECOT_INNER_ADDR} --user {user} \
         --password {password} --idle-secs {idle} --window-secs {window} --limit-mib {limit} \
         --sync-timeout-secs {timeout} --out {out} > {log} 2>&1",
        xtask = inner("xtask"),
        server = inner("tagmail-server"),
        user = opts.user,
        password = opts.password,
        idle = opts.idle.as_secs(),
        window = opts.window.as_secs(),
        limit = opts.limit_mib,
        timeout = opts.sync_timeout.as_secs(),
        out = inner("result.json"),
        log = inner("bench.log"),
    );
    let runner = output(Command::new("docker").args([
        "create",
        "--network",
        &format!("container:{dovecot}"),
        RUNNER_IMAGE,
        "sh",
        "-c",
        &script,
    ]))?;
    let runner = runner.trim().to_owned();
    let result = run_runner(&runner, &bin, opts);
    let _ = Command::new("docker")
        .args(["rm", "--force", &runner])
        .stdout(Stdio::null())
        .status();
    result
}

fn run_runner(runner: &str, bin: &Path, opts: &Options) -> Result<()> {
    let staging = workspace_root().join("target").join("bench-rss-runner");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    for name in ["xtask", "tagmail-server"] {
        fs::copy(bin.join(name), staging.join(name))?;
    }
    run(Command::new("docker").args([
        "cp",
        &format!("{}/.", staging.display()),
        &format!("{runner}:{RUNNER_DIR}"),
    ]))?;
    run(Command::new("docker").args(["start", runner]))?;
    let code = output(Command::new("docker").args(["wait", runner]))?;
    let log = staging.join("bench.log");
    run(Command::new("docker").args([
        "cp",
        &format!("{runner}:{RUNNER_DIR}/bench.log"),
        &log.display().to_string(),
    ]))?;
    print!("{}", fs::read_to_string(&log)?);
    let copied = Command::new("docker")
        .args([
            "cp",
            &format!("{runner}:{RUNNER_DIR}/result.json"),
            &opts.out.display().to_string(),
        ])
        .status()?;
    if copied.success() {
        println!("result: {}", opts.out.display());
    }
    if code.trim() != "0" {
        bail!(
            "bench-rss in the Dovecot network exited with {}",
            code.trim()
        );
    }
    Ok(())
}

pub fn run_local(opts: &Options) -> Result<()> {
    let server = match &opts.server {
        Some(path) => path.clone(),
        None => {
            run(cargo().args([
                "build",
                "--release",
                "-p",
                "tagmail-api-server",
                "--bin",
                "tagmail-server",
            ]))?;
            workspace_root()
                .join("target")
                .join("release")
                .join("tagmail-server")
        }
    };
    let work = std::env::temp_dir().join(format!("tagmail-bench-rss-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let result = bench(opts, &server, &work);
    if result.is_ok() {
        let _ = fs::remove_dir_all(&work);
    } else {
        let log = fs::read_to_string(work.join("server.log")).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(LOG_TAIL_LINES).collect();
        for line in tail.iter().rev() {
            eprintln!("{line}");
        }
        eprintln!("server log and data kept in {}", work.display());
    }
    result
}

struct Running {
    child: Child,
    addr: SocketAddr,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bench(opts: &Options, server: &Path, work: &Path) -> Result<()> {
    let addr = free_loopback_addr()?;
    let log = fs::File::create(work.join("server.log"))?;
    let child = Command::new(server)
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--data-dir")
        .arg(work.join("data"))
        .arg("--master-key")
        .arg(work.join("master.key"))
        .args(["--init-key", "--insecure-plain-imap"])
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("starting {}", server.display()))?;
    let mut running = Running { child, addr };
    let pid = running.child.id();
    wait_until_serving(&mut running)?;

    let account = json!({
        "name": "bench",
        "imapHost": opts.imap.ip().to_string(),
        "imapPort": opts.imap.port(),
        "imapTls": "starttls",
        "smtpHost": opts.imap.ip().to_string(),
        "smtpPort": 25,
        "smtpTls": "starttls",
        "username": opts.user,
        "password": opts.password,
    });
    let (status, body) = http(addr, "POST", "/api/accounts", Some(&account.to_string()))?;
    if status != 201 {
        bail!("POST /api/accounts: {status} {body}");
    }

    println!("syncing {} at {}", opts.user, opts.imap);
    let sync_started = Instant::now();
    let mut sync_peak_kib = 0;
    let mut next_progress = sync_started;
    let mut tick = Instant::now();
    let settled = loop {
        check_alive(&mut running)?;
        sync_peak_kib = sync_peak_kib.max(proc_status_kib(pid, "VmRSS")?);
        let account = account_status(addr)?;
        let live = account["phase"] == "live";
        let pending = account["textPending"].as_u64().unwrap_or(u64::MAX);
        if live && pending == 0 {
            break account;
        }
        if tick >= next_progress {
            println!(
                "  {:>5}s phase {} mailboxes {}/{} text pending {pending} rss {:.1} MiB{}",
                sync_started.elapsed().as_secs(),
                account["phase"],
                account["mailboxesLive"],
                account["mailboxesTotal"],
                mib(proc_status_kib(pid, "VmRSS")?),
                account["lastError"]
                    .as_str()
                    .map(|e| format!(" last error: {e}"))
                    .unwrap_or_default(),
            );
            next_progress += PROGRESS_INTERVAL;
        }
        if sync_started.elapsed() > opts.sync_timeout {
            bail!("sync not finished after {:?}: {account}", opts.sync_timeout);
        }
        tick = sleep_until_next(tick);
    };
    let sync_duration = sync_started.elapsed();
    println!(
        "synced in {:.0} s (peak RSS {:.1} MiB); idling {} s",
        sync_duration.as_secs_f64(),
        mib(sync_peak_kib),
        opts.idle.as_secs()
    );

    let idle_started = Instant::now();
    let mut samples = Vec::new();
    let mut tick = idle_started;
    while idle_started.elapsed() < opts.idle {
        check_alive(&mut running)?;
        samples.push((
            idle_started.elapsed().as_secs_f64(),
            proc_status_kib(pid, "VmRSS")?,
        ));
        tick = sleep_until_next(tick);
    }
    let hwm_kib = proc_status_kib(pid, "VmHWM")?;
    let window_start = opts.idle.saturating_sub(opts.window).as_secs_f64();
    let window_max_kib = samples
        .iter()
        .filter(|(t, _)| *t >= window_start)
        .map(|(_, kib)| *kib)
        .max()
        .context("no RSS samples in the gate window")?;
    let idle_max_kib = samples.iter().map(|(_, kib)| *kib).max().unwrap_or(0);
    let pass = window_max_kib <= opts.limit_mib * 1024;
    stop(&mut running);

    let result = json!({
        "scenario": SCENARIO,
        "pass": pass,
        "limitMib": opts.limit_mib,
        "idleWindowMaxRssMib": round(mib(window_max_kib)),
        "idleMaxRssMib": round(mib(idle_max_kib)),
        "syncPeakRssMib": round(mib(sync_peak_kib)),
        "vmHwmMib": round(mib(hwm_kib)),
        "syncSeconds": round(sync_duration.as_secs_f64()),
        "idleSeconds": opts.idle.as_secs(),
        "windowSeconds": opts.window.as_secs(),
        "mailboxes": settled["mailboxesTotal"],
        "idleRssKib": samples.iter().map(|(_, kib)| kib).collect::<Vec<_>>(),
    });
    if let Some(dir) = opts.out.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&opts.out, serde_json::to_string_pretty(&result)? + "\n")?;
    println!(
        "{SCENARIO}\n  idle max RSS, last {} s: {:.1} MiB (limit {} MiB) {}\n  \
         idle max RSS: {:.1} MiB\n  peak RSS during sync: {:.1} MiB (not gated)\n  \
         VmHWM: {:.1} MiB\n  sync: {:.0} s",
        opts.window.as_secs(),
        mib(window_max_kib),
        opts.limit_mib,
        if pass { "PASS" } else { "FAIL" },
        mib(idle_max_kib),
        mib(sync_peak_kib),
        mib(hwm_kib),
        sync_duration.as_secs_f64(),
    );
    if !pass {
        bail!("idle RSS gate failed");
    }
    Ok(())
}

fn account_status(addr: SocketAddr) -> Result<Value> {
    let (status, body) = http(addr, "GET", "/api/status", None)?;
    if status != 200 {
        bail!("GET /api/status: {status} {body}");
    }
    let body: Value = serde_json::from_str(&body)?;
    Ok(body["accounts"][0].clone())
}

fn wait_until_serving(running: &mut Running) -> Result<()> {
    let started = Instant::now();
    loop {
        check_alive(running)?;
        if matches!(
            http(running.addr, "GET", "/api/version", None),
            Ok((200, _))
        ) {
            return Ok(());
        }
        if started.elapsed() > STARTUP_TIMEOUT {
            bail!("tagmail-server did not answer on {}", running.addr);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn check_alive(running: &mut Running) -> Result<()> {
    if let Some(status) = running.child.try_wait()? {
        bail!("tagmail-server exited with {status}");
    }
    Ok(())
}

/// SIGTERM for a graceful shutdown; killed by [`Running`]'s drop otherwise.
fn stop(running: &mut Running) {
    let terminated = Command::new("kill")
        .args(["-TERM", &running.child.id().to_string()])
        .status()
        .is_ok_and(|s| s.success());
    if !terminated {
        return;
    }
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(running.child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sleep_until_next(tick: Instant) -> Instant {
    let next = tick + SAMPLE_INTERVAL;
    std::thread::sleep(next.saturating_duration_since(Instant::now()));
    next
}

/// A `kB` field of `/proc/<pid>/status`.
fn proc_status_kib(pid: u32, field: &str) -> Result<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|v| v.trim().strip_suffix("kB")?.trim().parse().ok())
        .with_context(|| format!("no {field} in /proc/{pid}/status"))
}

fn mib(kib: u64) -> f64 {
    kib as f64 / KIB
}

fn round(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn free_loopback_addr() -> Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

/// One HTTP/1.0 exchange, so the response is neither chunked nor kept alive.
fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let body = body.unwrap_or("");
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("malformed HTTP response")?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP status line")?;
    Ok((status, body.to_owned()))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}
