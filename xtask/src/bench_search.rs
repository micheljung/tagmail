//! AC 11 search gate (plan §6 row 11): two accounts with 100k seeded
//! messages each are synced to the end of the text pass; then 30 named
//! queries run through the HTTP API. A query's latency is its view index
//! plus the first page of thread rows, i.e. what the list waits for. The
//! first run after a server restart is reported as cold; the gate is the
//! p95 over the warm runs.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use tagmail_api_core::id_index;

use crate::bench_rss::{
    self, Running, add_account, check_alive, in_work_dir, round, server_binary, sleep_until_next,
    spawn_server, step_summary, stop,
};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
/// Rows fetched with the index, as the list's first page.
const FIRST_PAGE: usize = 50;
const PERCENTILE: f64 = 0.95;

/// Name and query; the vocabulary is `xtask seed`'s.
pub const QUERIES: [(&str, &str); 30] = [
    ("broad_text", "the"),
    ("broad_text_2", "meeting"),
    ("narrow_text", "Zürich"),
    ("phrase_subject", "\"quarterly report\""),
    ("phrase_body", "\"the deadline\""),
    ("prefix", "migr*"),
    ("subject", "subject:invoice"),
    ("subject_phrase", "subject:\"budget review\""),
    ("from_address", "from:contact42@example.com"),
    ("from_name", "from:\"Alice Smith\""),
    ("to_me", "to:user1@tagmail.test"),
    ("cc", "cc:contact7"),
    ("label", "label:work"),
    ("label_rare", "label:receipts"),
    ("in_sent", "in:sent"),
    ("in_archive_text", "in:archive budget"),
    ("unread", "is:unread"),
    ("starred", "is:starred"),
    ("attachment", "has:attachment"),
    ("date_range", "after:2024/01/01 before:2024/02/01"),
    (
        "broad_text_narrow_filter",
        "invoice from:contact42@example.com after:2020/01/01",
    ),
    ("broad_text_recent", "the after:2024/06/01"),
    ("negation_only_label", "-label:work"),
    ("negation_only_text", "-the"),
    ("or", "invoice OR contract"),
    ("or_group_starred", "(trip OR dinner) is:starred"),
    ("text_minus_text", "budget -meeting"),
    ("label_and_text", "label:travel schedule"),
    ("anywhere_text", "in:anywhere server"),
    (
        "mixed_filters",
        "from:example.org has:attachment -is:unread",
    ),
];

#[derive(Debug, Clone)]
pub struct Options {
    pub server: Option<PathBuf>,
    pub imap: SocketAddr,
    /// `user:password` per account.
    pub accounts: Vec<(String, String)>,
    pub warm_runs: usize,
    pub limit_ms: u64,
    pub sync_timeout: Duration,
    pub out: PathBuf,
}

pub fn run_in_dovecot_network(opts: &Options) -> Result<()> {
    let accounts: Vec<String> = opts
        .accounts
        .iter()
        .map(|(u, p)| format!("--account {u}:{p}"))
        .collect();
    let args = format!(
        "{} --warm-runs {} --limit-ms {} --sync-timeout-secs {}",
        accounts.join(" "),
        opts.warm_runs,
        opts.limit_ms,
        opts.sync_timeout.as_secs()
    );
    bench_rss::run_xtask_in_dovecot_network("bench-search", &args, &opts.out)
}

pub fn run_local(opts: &Options) -> Result<()> {
    let server = server_binary(opts.server.as_deref())?;
    in_work_dir("bench-search", |work| bench(opts, &server, work))
}

struct Sample {
    hits: usize,
    elapsed: Duration,
}

fn bench(opts: &Options, server: &Path, work: &Path) -> Result<()> {
    let mut running = spawn_server(server, work)?;
    for (user, password) in &opts.accounts {
        add_account(&running, opts.imap, user, password)?;
    }
    let sync = wait_for_sync(&mut running, opts)?;
    println!(
        "synced in {:.0} s; restarting for the cold run",
        sync.as_secs_f64()
    );
    stop(&mut running);
    drop(running);

    let mut running = spawn_server(server, work)?;
    let threads = thread_count(&running)?;
    let cold: Vec<Sample> = QUERIES
        .iter()
        .map(|(_, q)| search(&running, q))
        .collect::<Result<_>>()?;
    let mut warm: Vec<Vec<Duration>> = vec![Vec::new(); QUERIES.len()];
    for _ in 0..opts.warm_runs {
        for (i, (_, q)) in QUERIES.iter().enumerate() {
            check_alive(&mut running)?;
            warm[i].push(search(&running, q)?.elapsed);
        }
    }
    stop(&mut running);

    let all: Vec<Duration> = warm.iter().flatten().copied().collect();
    let p95 = percentile(&all, PERCENTILE);
    let cold_all: Vec<Duration> = cold.iter().map(|s| s.elapsed).collect();
    let cold_p95 = percentile(&cold_all, PERCENTILE);
    let pass = p95 < Duration::from_millis(opts.limit_ms);
    let verdict = if pass { "PASS" } else { "FAIL" };

    let rows: Vec<Value> = QUERIES
        .iter()
        .zip(&cold)
        .zip(&warm)
        .map(|(((name, q), c), w)| {
            json!({
                "name": name,
                "query": q,
                "threads": c.hits,
                "coldMs": ms(c.elapsed),
                "warmP50Ms": ms(percentile(w, 0.5)),
                "warmMaxMs": ms(w.iter().copied().max().unwrap_or_default()),
            })
        })
        .collect();
    let result = json!({
        "scenario": format!("{} accounts, {threads} threads", opts.accounts.len()),
        "pass": pass,
        "limitMs": opts.limit_ms,
        "warmP95Ms": ms(p95),
        "coldP95Ms": ms(cold_p95),
        "coldMaxMs": ms(cold_all.iter().copied().max().unwrap_or_default()),
        "warmRuns": opts.warm_runs,
        "syncSeconds": round(sync.as_secs_f64()),
        "queries": rows,
    });
    if let Some(dir) = opts.out.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&opts.out, serde_json::to_string_pretty(&result)? + "\n")?;

    let mut table = String::from(
        "| Query | Threads | Cold ms | Warm p50 ms | Warm max ms |\n|---|---:|---:|---:|---:|\n",
    );
    for row in &rows {
        table.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            row["query"]
                .as_str()
                .unwrap_or_default()
                .replace('|', "\\|"),
            row["threads"],
            row["coldMs"],
            row["warmP50Ms"],
            row["warmMaxMs"],
        ));
    }
    let headline = format!(
        "search over {threads} threads ({} accounts): warm p95 {:.1} ms (limit {} ms) {verdict}; \
         cold p95 {:.1} ms (not gated)",
        opts.accounts.len(),
        ms(p95),
        opts.limit_ms,
        ms(cold_p95),
    );
    println!("{headline}\n{table}");
    step_summary(&format!(
        "### Search gate (AC 11): {verdict}\n\n{headline}\n\n{table}\n"
    ))?;
    if !pass {
        bail!("search latency gate failed");
    }
    Ok(())
}

/// One query: the view index, then the first page of rows.
fn search(running: &Running, query: &str) -> Result<Sample> {
    let view = format!("search:{query}");
    let path = format!(
        "/api/views/{}/index",
        utf8_percent_encode(&view, NON_ALPHANUMERIC)
    );
    let started = Instant::now();
    let (status, body) = running.get(&path)?;
    if status != 200 {
        bail!("{query}: {status} {}", String::from_utf8_lossy(&body));
    }
    let ids = id_index::decode(&body)?;
    if !ids.is_empty() {
        let page: Vec<String> = ids.iter().take(FIRST_PAGE).map(i64::to_string).collect();
        let (status, body) = running.get(&format!("/api/threads?ids={}", page.join(",")))?;
        if status != 200 {
            bail!("{query} rows: {status} {}", String::from_utf8_lossy(&body));
        }
    }
    Ok(Sample {
        hits: ids.len(),
        elapsed: started.elapsed(),
    })
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

fn wait_for_sync(running: &mut Running, opts: &Options) -> Result<Duration> {
    let started = Instant::now();
    let mut next_progress = started;
    let mut tick = started;
    loop {
        check_alive(running)?;
        let accounts = accounts(running)?;
        let done = accounts.len() == opts.accounts.len()
            && accounts
                .iter()
                .all(|a| a["phase"] == "live" && a["textPending"] == 0);
        if done {
            return Ok(started.elapsed());
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
                "  {:>5}s {}",
                started.elapsed().as_secs(),
                states.join("; ")
            );
            next_progress += PROGRESS_INTERVAL;
        }
        if started.elapsed() > opts.sync_timeout {
            bail!("sync not finished after {:?}", opts.sync_timeout);
        }
        tick = sleep_until_next(tick);
    }
}

/// Threads of the whole corpus.
fn thread_count(running: &Running) -> Result<usize> {
    Ok(search(running, "in:anywhere")?.hits)
}

fn percentile(samples: &[Duration], p: f64) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_queries_parse_and_are_unique() {
        let mut names: Vec<&str> = QUERIES.iter().map(|(n, _)| *n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), QUERIES.len());
        for (name, q) in QUERIES {
            assert!(tagmail_core::query::parse(q).is_ok(), "{name}: {q}");
        }
    }

    #[test]
    fn nearest_rank_percentile() {
        let d: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
        assert_eq!(percentile(&d, 0.95), Duration::from_millis(19));
        assert_eq!(percentile(&d, 0.5), Duration::from_millis(10));
        assert_eq!(percentile(&[], 0.95), Duration::ZERO);
    }
}
