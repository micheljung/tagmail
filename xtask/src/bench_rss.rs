//! AC 9 RSS gate (plan §3.13, §6 row 9), full scenario: two accounts with
//! the 100k seeds in `user1` and `user2` are synced to the end of the text
//! pass; then one SSE client connects and stays connected, one search runs
//! and the 25 MB message of `seed --huge-mb` is opened and rendered, and the
//! server idles. VmRSS is sampled every second; the gate is the maximum over
//! the last `window` of the idle period.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::bench_replica;
use crate::bench_search::{self, ms};
use crate::harness::{self, output, run};
use crate::seed::HUGE_SUBJECT;

const SCENARIO: &str =
    "m4: 2 accounts x 100k synced, 1 SSE client, 1 search, 1 25 MB message rendered, then idle";
/// A broad query of `xtask seed`'s vocabulary.
const SEARCH_QUERY: &str = "meeting";
/// Sender addresses of the test users: `<user>@tagmail.test`.
const EMAIL_DOMAIN: &str = "tagmail.test";
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const LOG_TAIL_LINES: usize = 50;
const KIB: f64 = 1024.0;
const BYTES_PER_KIB: u64 = 1024;
const BYTES_PER_MB: f64 = 1_000_000.0;
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
    /// `user:password` per account.
    pub accounts: Vec<(String, String)>,
    pub idle: Duration,
    pub window: Duration,
    /// Gate in decimal megabytes (10^6 bytes), as the spec's "64 MB".
    pub limit_mb: u64,
    pub sync_timeout: Duration,
    pub out: PathBuf,
    /// `tagmail-desktop` with `app,test-hooks`, measured after the gate.
    pub desktop: Option<PathBuf>,
    pub desktop_idle: Duration,
}

/// Builds static binaries and runs the benchmark in a container sharing the
/// Dovecot container's network namespace, for Docker daemons whose
/// published ports are not reachable from here.
pub fn run_in_dovecot_network(opts: &Options) -> Result<()> {
    let accounts: Vec<String> = opts
        .accounts
        .iter()
        .map(|(u, p)| format!("--account {u}:{p}"))
        .collect();
    let args = format!(
        "{accounts} --idle-secs {idle} --window-secs {window} \
         --limit-mb {limit} --sync-timeout-secs {timeout}",
        accounts = accounts.join(" "),
        idle = opts.idle.as_secs(),
        window = opts.window.as_secs(),
        limit = opts.limit_mb,
        timeout = opts.sync_timeout.as_secs(),
    );
    run_xtask_in_dovecot_network("bench-rss", &args, &opts.out)
}

/// Runs `cargo xtask <command> <args> --server … --imap … --out …` with
/// static binaries in a container sharing the Dovecot container's network
/// namespace and copies the JSON result to `out`.
pub(crate) fn run_xtask_in_dovecot_network(command: &str, args: &str, out: &Path) -> Result<()> {
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
        "{xtask} {command} --server {server} --imap {DOVECOT_INNER_ADDR} {args} \
         --out {result} > {log} 2>&1",
        xtask = inner("xtask"),
        server = inner("tagmail-server"),
        result = inner("result.json"),
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
    let result = run_runner(&runner, &bin, command, out);
    let _ = Command::new("docker")
        .args(["rm", "--force", &runner])
        .stdout(Stdio::null())
        .status();
    result
}

fn run_runner(runner: &str, bin: &Path, command: &str, out: &Path) -> Result<()> {
    let staging = workspace_root()
        .join("target")
        .join(format!("{command}-runner"));
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
            &out.display().to_string(),
        ])
        .status()?;
    if copied.success() {
        println!("result: {}", out.display());
    }
    if code.trim() != "0" {
        bail!(
            "{command} in the Dovecot network exited with {}",
            code.trim()
        );
    }
    Ok(())
}

pub fn run_local(opts: &Options) -> Result<()> {
    let server = server_binary(opts.server.as_deref())?;
    in_work_dir("bench-rss", |work| bench(opts, &server, work))
}

/// `server`, or a fresh release build of `tagmail-server`.
pub(crate) fn server_binary(server: Option<&Path>) -> Result<PathBuf> {
    Ok(match server {
        Some(path) => path.to_owned(),
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
    })
}

/// Runs `f` in a fresh temporary directory; on failure the directory is
/// kept and the server log tail printed.
pub(crate) fn in_work_dir(name: &str, f: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let work = std::env::temp_dir().join(format!("tagmail-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let result = f(&work);
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

pub(crate) struct Running {
    pub(crate) child: Child,
    pub(crate) addr: SocketAddr,
    /// Owner session cookie, `name=value`.
    pub(crate) session: String,
}

impl Running {
    pub(crate) fn get(&self, path: &str) -> Result<(u16, Vec<u8>)> {
        let (status, _, body) = request(self.addr, "GET", path, None, Some(&self.session))?;
        Ok((status, body))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `tagmail-server` on a free loopback port with its data in `work`,
/// appending to `work/server.log`, waits until it answers and logs in as
/// the owner (setting the owner up on the first start).
pub(crate) fn spawn_server(server: &Path, work: &Path) -> Result<Running> {
    let addr = free_loopback_addr()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(work.join("server.log"))?;
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
    let mut running = Running {
        child,
        addr,
        session: String::new(),
    };
    wait_until_serving(&mut running)?;
    running.session = owner_session(addr, &work.join("server.log"))?;
    Ok(running)
}

const OWNER_PASSWORD: &str = "bench-owner-password";

fn owner_session(addr: SocketAddr, log: &Path) -> Result<String> {
    let login = json!({ "password": OWNER_PASSWORD }).to_string();
    let (status, head, _) = request(addr, "POST", "/api/auth/login", Some(&login), None)?;
    if status == 200 {
        return session_cookie(&head);
    }
    set_up_owner(addr, log)
}

/// Sets the owner password with the setup token from the server log and
/// returns the session cookie.
fn set_up_owner(addr: SocketAddr, log: &Path) -> Result<String> {
    const MARKER: &str = "one-time setup token ";
    let log = fs::read_to_string(log)?;
    let token: String = log
        .rsplit_once(MARKER)
        .context("no setup token in the server log")?
        .1
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let setup = json!({ "token": token, "password": OWNER_PASSWORD }).to_string();
    let (status, head, body) = request(addr, "POST", "/api/auth/setup", Some(&setup), None)?;
    if status != 201 {
        bail!(
            "POST /api/auth/setup: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    session_cookie(&head)
}

fn session_cookie(head: &str) -> Result<String> {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().split(';').next().unwrap_or("").to_owned())
        })
        .context("no session cookie")
}

/// Adds an account for `user` on the IMAP server at `imap`.
pub(crate) fn add_account(
    running: &Running,
    imap: SocketAddr,
    user: &str,
    password: &str,
) -> Result<()> {
    let account = json!({
        "name": user,
        "imapHost": imap.ip().to_string(),
        "imapPort": imap.port(),
        "imapTls": "starttls",
        "smtpHost": imap.ip().to_string(),
        "smtpPort": 25,
        "smtpTls": "starttls",
        "username": user,
        "password": password,
        "email": format!("{user}@{EMAIL_DOMAIN}"),
    });
    let (status, body) = http(
        running.addr,
        "POST",
        "/api/accounts",
        Some(&account.to_string()),
        Some(&running.session),
    )?;
    if status != 201 {
        bail!("POST /api/accounts: {status} {body}");
    }
    Ok(())
}

fn bench(opts: &Options, server: &Path, work: &Path) -> Result<()> {
    let mut running = spawn_server(server, work)?;
    let pid = running.child.id();
    for (user, password) in &opts.accounts {
        add_account(&running, opts.imap, user, password)?;
    }

    println!("syncing {} accounts at {}", opts.accounts.len(), opts.imap);
    let sync = wait_for_sync(&mut running, opts.accounts.len(), opts.sync_timeout)?;
    println!(
        "synced in {:.0} s (peak RSS {:.1} MiB)",
        sync.duration.as_secs_f64(),
        mib(sync.peak_kib)
    );

    let sse = SseClient::open(&running)?;
    let searched = bench_search::search(&running, SEARCH_QUERY)?;
    let render = render_huge(&running, pid)?;
    println!(
        "SSE connected; search {SEARCH_QUERY:?}: {} threads in {:.1} ms; \
         {HUGE_SUBJECT} opened and rendered ({} bytes of HTML) in {:.1} ms; idling {} s",
        searched.hits,
        ms(searched.elapsed),
        render.html_bytes,
        ms(render.elapsed),
        opts.idle.as_secs()
    );
    let (sync_duration, sync_peak_kib) = (sync.duration, sync.peak_kib);

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
    let sse_connected = !sse.reader.is_finished();
    let pass =
        sse_connected && window_max_kib * BYTES_PER_KIB <= opts.limit_mb * BYTES_PER_MB as u64;
    let replica = bench_replica::measure(&mut running, pid, work);
    let desktop = opts
        .desktop
        .as_deref()
        .map(|app| bench_replica::desktop(&running, app, work, opts.desktop_idle));
    stop(&mut running);
    let (replica_json, replica_row) = match replica {
        Ok(cost) => (cost.json(), cost.summary()),
        Err(e) => {
            let log = fs::read_to_string(work.join("server.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(LOG_TAIL_LINES).collect();
            for line in tail.iter().rev() {
                eprintln!("{line}");
            }
            (
                json!({ "error": format!("{e:#}") }),
                format!("failed: {e:#}"),
            )
        }
    };
    println!("replica bootstrap (not gated): {replica_row}");
    let (desktop_json, desktop_row) = match desktop {
        None => (Value::Null, "not run (no --desktop)".to_owned()),
        Some(Ok(run)) => (run.json(), run.summary()),
        Some(Err(e)) => (
            json!({ "error": format!("{e:#}") }),
            format!("failed: {e:#}"),
        ),
    };
    println!("desktop app (not gated): {desktop_row}");
    let sse_bytes = sse.reader.join().unwrap_or(0);
    let result = json!({
        "scenario": SCENARIO,
        "pass": pass,
        "limitMb": opts.limit_mb,
        "limitMib": round(opts.limit_mb as f64 * BYTES_PER_MB / (KIB * KIB)),
        "idleWindowMaxRssMb": round(mb(window_max_kib)),
        "idleWindowMaxRssMib": round(mib(window_max_kib)),
        "idleMaxRssMib": round(mib(idle_max_kib)),
        "syncPeakRssMib": round(mib(sync_peak_kib)),
        "vmHwmMib": round(mib(hwm_kib)),
        "syncSeconds": round(sync_duration.as_secs_f64()),
        "idleSeconds": opts.idle.as_secs(),
        "windowSeconds": opts.window.as_secs(),
        "accounts": opts.accounts.len(),
        "mailboxes": sync.mailboxes,
        "sseConnected": sse_connected,
        "sseBytes": sse_bytes,
        "searchQuery": SEARCH_QUERY,
        "searchThreads": searched.hits,
        "searchMs": ms(searched.elapsed),
        "renderMs": ms(render.elapsed),
        "renderedHtmlBytes": render.html_bytes,
        "renderPeakMib": round(mib(render.peak_kib)),
        "renderPeakDeltaMib": round(mib(render.peak_kib.saturating_sub(render.before_kib))),
        "replicaBootstrap": replica_json,
        "desktop": desktop_json,
        "idleRssKib": samples.iter().map(|(_, kib)| kib).collect::<Vec<_>>(),
    });
    if let Some(dir) = opts.out.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&opts.out, serde_json::to_string_pretty(&result)? + "\n")?;
    let verdict = if pass { "PASS" } else { "FAIL" };
    let limit_mib = opts.limit_mb as f64 * BYTES_PER_MB / (KIB * KIB);
    println!(
        "{SCENARIO}\n  idle max RSS, last {} s: {:.1} MB = {:.1} MiB (limit {} MB = {limit_mib:.2} MiB) \
         {verdict}\n  idle max RSS: {:.1} MiB\n  peak RSS during sync: {:.1} MiB (not gated)\n  \
         VmHWM since the render: {:.1} MiB\n  sync: {:.0} s\n  SSE connected through the idle period: {sse_connected}",
        opts.window.as_secs(),
        mb(window_max_kib),
        mib(window_max_kib),
        opts.limit_mb,
        mib(idle_max_kib),
        mib(sync_peak_kib),
        mib(hwm_kib),
        sync_duration.as_secs_f64(),
    );
    step_summary(&format!(
        "### Idle RSS gate (AC 9): {verdict}\n\n{SCENARIO}\n\n\
         | Measure | Value |\n|---|---|\n\
         | Idle max RSS, last {} s (gated) | {:.1} MB ({:.1} MiB) |\n\
         | Limit | {} MB ({limit_mib:.2} MiB) |\n\
         | Idle max RSS | {:.1} MiB |\n\
         | Peak RSS during sync (not gated) | {:.1} MiB |\n\
         | VmHWM since the render | {:.1} MiB |\n\
         | Sync | {:.0} s |\n\
         | Search `{SEARCH_QUERY}` | {:.1} ms |\n\
         | Open + render {HUGE_SUBJECT} | {:.1} ms |\n\
         | Peak RSS during render (not gated) | {:.1} MiB (+{:.1} MiB) |\n\
         | SSE connected through idle | {sse_connected} |\n\
         | Replica bootstrap (not gated) | {replica_row} |\n\
         | Desktop app (not gated) | {desktop_row} |\n",
        opts.window.as_secs(),
        mb(window_max_kib),
        mib(window_max_kib),
        opts.limit_mb,
        mib(idle_max_kib),
        mib(sync_peak_kib),
        mib(hwm_kib),
        sync_duration.as_secs_f64(),
        ms(searched.elapsed),
        ms(render.elapsed),
        mib(render.peak_kib),
        mib(render.peak_kib.saturating_sub(render.before_kib)),
    ))?;
    if !sse_connected {
        bail!("the SSE stream closed during the idle period");
    }
    if !pass {
        bail!("idle RSS gate failed");
    }
    Ok(())
}

pub(crate) struct Synced {
    pub(crate) duration: Duration,
    pub(crate) peak_kib: u64,
    pub(crate) mailboxes: u64,
}

fn accounts(running: &Running) -> Result<Vec<Value>> {
    let (status, body) = running.get("/api/status")?;
    if status != 200 {
        bail!(
            "GET /api/status: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    let body: Value = serde_json::from_slice(&body)?;
    Ok(body["accounts"].as_array().cloned().unwrap_or_default())
}

/// Waits until all `expected` accounts are live with the text pass done.
pub(crate) fn wait_for_sync(
    running: &mut Running,
    expected: usize,
    timeout: Duration,
) -> Result<Synced> {
    let pid = running.child.id();
    let started = Instant::now();
    let mut peak_kib = 0;
    let mut next_progress = started;
    let mut tick = started;
    loop {
        check_alive(running)?;
        peak_kib = peak_kib.max(proc_status_kib(pid, "VmRSS")?);
        let accounts = accounts(running)?;
        let done = accounts.len() == expected
            && accounts
                .iter()
                .all(|a| a["phase"] == "live" && a["textPending"] == 0);
        if done {
            return Ok(Synced {
                duration: started.elapsed(),
                peak_kib,
                mailboxes: accounts
                    .iter()
                    .filter_map(|a| a["mailboxesTotal"].as_u64())
                    .sum(),
            });
        }
        if tick >= next_progress {
            let states: Vec<String> = accounts
                .iter()
                .map(|a| {
                    format!(
                        "{} {} {}/{} text pending {}{}",
                        a["name"],
                        a["phase"],
                        a["mailboxesLive"],
                        a["mailboxesTotal"],
                        a["textPending"],
                        a["lastError"]
                            .as_str()
                            .map(|e| format!(" last error: {e}"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            println!(
                "  {:>5}s rss {:.1} MiB; {}",
                started.elapsed().as_secs(),
                mib(proc_status_kib(pid, "VmRSS")?),
                states.join("; ")
            );
            next_progress += PROGRESS_INTERVAL;
        }
        if started.elapsed() > timeout {
            bail!("sync not finished after {timeout:?}: {accounts:?}");
        }
        tick = sleep_until_next(tick);
    }
}

/// An `/api/events` stream whose reader thread drains it until the server
/// closes it and returns the bytes read.
struct SseClient {
    reader: JoinHandle<u64>,
}

impl SseClient {
    fn open(running: &Running) -> Result<SseClient> {
        let mut stream = TcpStream::connect_timeout(&running.addr, HTTP_TIMEOUT)?;
        stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
        write!(
            stream,
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\
             Cookie: {}\r\n\r\n",
            running.addr, running.session
        )?;
        let mut head = Vec::new();
        let mut byte = [0u8];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        if !head.starts_with("HTTP/1.1 200") {
            bail!("GET /api/events: {head}");
        }
        stream.set_read_timeout(None)?;
        let reader = std::thread::spawn(move || {
            let mut sink = std::io::sink();
            std::io::copy(&mut stream, &mut sink).unwrap_or(0)
        });
        Ok(SseClient { reader })
    }
}

struct Render {
    html_bytes: usize,
    elapsed: Duration,
    /// VmRSS before and VmHWM after, the high-water mark reset first.
    before_kib: u64,
    peak_kib: u64,
}

/// Opens and renders the `seed --huge-mb` message, as the reader does.
fn render_huge(running: &Running, pid: u32) -> Result<Render> {
    let ids = bench_search::search_ids(running, &format!("subject:{HUGE_SUBJECT}"))?;
    let [thread] = ids[..] else {
        bail!("expected one {HUGE_SUBJECT} thread, found {}", ids.len());
    };
    let thread: Value = get_json(running, &format!("/api/threads/{thread}"))?;
    let message = thread["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["subject"] == HUGE_SUBJECT))
        .and_then(|m| m["id"].as_i64())
        .with_context(|| format!("no {HUGE_SUBJECT} message in {thread}"))?;
    reset_peak_rss(pid)?;
    let before_kib = proc_status_kib(pid, "VmRSS")?;
    let started = Instant::now();
    get_json(running, &format!("/api/messages/{message}"))?;
    let (status, html) = running.get(&format!("/api/messages/{message}/render"))?;
    let elapsed = started.elapsed();
    let peak_kib = proc_status_kib(pid, "VmHWM")?;
    if status != 200 || html.is_empty() {
        bail!("render {message}: {status}, {} bytes", html.len());
    }
    Ok(Render {
        html_bytes: html.len(),
        elapsed,
        before_kib,
        peak_kib,
    })
}

fn get_json(running: &Running, path: &str) -> Result<Value> {
    let (status, body) = running.get(path)?;
    if status != 200 {
        bail!("GET {path}: {status} {}", String::from_utf8_lossy(&body));
    }
    Ok(serde_json::from_slice(&body)?)
}

fn wait_until_serving(running: &mut Running) -> Result<()> {
    let started = Instant::now();
    loop {
        check_alive(running)?;
        if matches!(
            http(running.addr, "GET", "/api/version", None, None),
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

pub(crate) fn check_alive(running: &mut Running) -> Result<()> {
    if let Some(status) = running.child.try_wait()? {
        bail!("tagmail-server exited with {status}");
    }
    Ok(())
}

/// SIGTERM for a graceful shutdown; killed by [`Running`]'s drop otherwise.
pub(crate) fn stop(running: &mut Running) {
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

pub(crate) fn sleep_until_next(tick: Instant) -> Instant {
    let next = tick + SAMPLE_INTERVAL;
    std::thread::sleep(next.saturating_duration_since(Instant::now()));
    next
}

/// A `kB` field of `/proc/<pid>/status`.
/// Resets VmHWM of `pid` to its current RSS.
pub(crate) fn reset_peak_rss(pid: u32) -> Result<()> {
    let reset_peak = "5";
    fs::write(format!("/proc/{pid}/clear_refs"), reset_peak)
        .with_context(|| format!("resetting VmHWM of {pid}"))
}

pub(crate) fn proc_status_kib(pid: u32, field: &str) -> Result<u64> {
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

fn mb(kib: u64) -> f64 {
    (kib * BYTES_PER_KIB) as f64 / BYTES_PER_MB
}

/// Appends Markdown to the GitHub Actions job summary, if there is one.
pub(crate) fn step_summary(markdown: &str) -> Result<()> {
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(markdown.as_bytes())?;
    }
    Ok(())
}

pub(crate) fn round(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn free_loopback_addr() -> Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

/// One HTTP/1.0 exchange, so the response is neither chunked nor kept alive.
pub(crate) fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    session: Option<&str>,
) -> Result<(u16, String)> {
    let (status, _, body) = request(addr, method, path, body, session)?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// Status, head and body. Every request carries `X-Requested-With`, which
/// the server requires on POSTs.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    session: Option<&str>,
) -> Result<(u16, String, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let body = body.unwrap_or("");
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         X-Requested-With: tagmail\r\n{cookie}Content-Length: {}\r\n\r\n{body}",
        body.len(),
        cookie = session
            .map(|c| format!("Cookie: {c}\r\n"))
            .unwrap_or_default(),
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP status line")?;
    Ok((status, head.into_owned(), response[split + 4..].to_vec()))
}

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

pub(crate) fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}
