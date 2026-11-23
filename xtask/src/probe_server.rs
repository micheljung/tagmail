//! `cargo xtask probe-server`: records how an IMAP server behaves where
//! Tagmail's design depends on it (plan §4 M3): capabilities, keyword
//! persistence and limits, object ids, special-use mailboxes and whether
//! `RFC822.SIZE` survives a COPY. Everything happens in scratch mailboxes
//! that are deleted afterwards.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tagmail_imap::{
    FetchSpec, Flag, ImapSession as _, Password, Security, SelectOptions, ServerConfig, StoreOp,
    TokioSession, Uid, UidSet,
};

/// Keywords tried in one message before reporting "no limit found".
const KEYWORD_ATTEMPTS: usize = 128;
const PERSISTENCE_KEYWORD: &str = "$tagmailprobe";
const LIMIT_KEYWORD_PREFIX: &str = "$tmprobe";

/// Things no IMAP session can show.
const MANUAL: [&str; 2] = [
    "Sent-saving: send one message through the account's SMTP submission \
     server and check whether the Sent mailbox gains a copy without an \
     APPEND (decides `server_saves_sent`).",
    "Archive semantics: whether `\\Archive` or Gmail's `\\All` is the \
     archive target is read from the special-use list above.",
];

#[derive(Debug, Clone)]
pub struct Options {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub user: String,
    pub password: String,
    pub json: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let report = runtime.block_on(probe(opts))?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(())
}

fn server_config(opts: &Options) -> ServerConfig {
    ServerConfig::new(
        opts.host.clone(),
        opts.port,
        opts.security,
        opts.user.clone(),
        Password::new(opts.password.clone()),
    )
}

fn message(id: &str) -> Vec<u8> {
    format!(
        "From: probe@example.com\r\nTo: probe@example.com\r\nSubject: tagmail probe {id}\r\n\
         Message-ID: <{id}@tagmail-probe>\r\nDate: Sun, 22 Nov 2026 10:00:00 +0000\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nprobe {id}\r\n"
    )
    .into_bytes()
}

fn one(uid: Uid) -> UidSet {
    [uid].into_iter().collect()
}

fn failed(error: impl std::fmt::Display) -> Value {
    json!({ "error": error.to_string() })
}

fn flag_names(flags: &[Flag]) -> Vec<String> {
    flags.iter().map(|f| f.as_imap().to_owned()).collect()
}

async fn probe(opts: &Options) -> Result<Value> {
    let config = server_config(opts);
    let mut session = TokioSession::connect(&config)
        .await
        .context("connecting and logging in")?;
    let mut scratch = Vec::new();
    let report = run_probes(opts, &config, &mut session, &mut scratch).await;
    cleanup(&mut session, &scratch).await;
    let _ = session.logout().await;
    report
}

async fn run_probes(
    opts: &Options,
    config: &ServerConfig,
    session: &mut TokioSession,
    scratch: &mut Vec<String>,
) -> Result<Value> {
    let caps = session.capabilities().clone();
    let mut report = json!({
        "host": opts.host,
        "port": opts.port,
        "capabilities": caps.iter().collect::<Vec<_>>(),
        "features": {
            "condstore": caps.condstore(),
            "qresync": session.qresync_enabled(),
            "objectid": caps.objectid(),
            "gmail_extensions": caps.gmail(),
            "move": caps.r#move(),
            "uidplus": caps.uidplus(),
            "idle": caps.idle(),
            "special_use": caps.special_use(),
        },
    });
    report["special_use"] = match session.list().await {
        Ok(boxes) => Value::Array(
            boxes
                .iter()
                .filter_map(|b| {
                    b.special_use
                        .map(|role| json!({ "mailbox": b.name, "role": format!("{role:?}") }))
                })
                .collect(),
        ),
        Err(e) => failed(e),
    };

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let first = format!("tagmail-probe-{stamp}-a");
    let second = format!("tagmail-probe-{stamp}-b");
    for name in [&first, &second] {
        session
            .create_mailbox(name)
            .await
            .with_context(|| format!("creating scratch mailbox {name}"))?;
        scratch.push(name.clone());
    }

    let sent = session
        .append(&first, &[], &message(&format!("a-{stamp}")))
        .await
        .context("appending the probe message")?;
    let uid = sent.uid.context("server did not report APPENDUID")?;
    let selected = session
        .select(&first, SelectOptions::default())
        .await
        .context("selecting the scratch mailbox")?;
    report["permanent_flags"] = json!({
        "flags": flag_names(&selected.permanent_flags),
        "arbitrary_keywords": selected.keyword_capable,
    });

    report["keyword_persistence"] = keyword_persistence(session, config, &first, uid)
        .await
        .unwrap_or_else(failed);
    // The persistence probe reconnected; the old session is still usable.
    report["keyword_limit"] = keyword_limit(session, &first).await.unwrap_or_else(failed);
    report["copy"] = copy_probe(session, &first, &second, uid, caps.objectid())
        .await
        .unwrap_or_else(failed);
    report["manual"] = json!(MANUAL);
    Ok(report)
}

/// Stores a keyword, reads it back in the same session, then reconnects and
/// reads it back again.
async fn keyword_persistence(
    session: &mut TokioSession,
    config: &ServerConfig,
    mailbox: &str,
    uid: Uid,
) -> Result<Value> {
    let keyword = Flag::Keyword(PERSISTENCE_KEYWORD.to_owned());
    let mut result = json!({ "keyword": PERSISTENCE_KEYWORD });
    if let Err(e) = session
        .store_flags(
            &one(uid),
            StoreOp::Add,
            std::slice::from_ref(&keyword),
            None,
        )
        .await
    {
        result["store_error"] = json!(e.to_string());
        result["persisted_after_reconnect"] = json!(false);
        return Ok(result);
    }
    let flags = fetch_flags(session, uid).await?;
    result["visible_in_same_session"] = json!(flags.contains(&keyword));

    let mut second = TokioSession::connect(config)
        .await
        .context("reconnecting")?;
    second.select(mailbox, SelectOptions::default()).await?;
    let after = fetch_flags(&mut second, uid).await?;
    result["persisted_after_reconnect"] = json!(after.contains(&keyword));
    result["flags_after_reconnect"] = json!(flag_names(&after));
    let _ = second.logout().await;
    Ok(result)
}

async fn fetch_flags(session: &mut TokioSession, uid: Uid) -> Result<Vec<Flag>> {
    let metas = session.fetch(&one(uid), &FetchSpec::flags()).await?;
    Ok(metas
        .into_iter()
        .find(|m| m.uid == Some(uid))
        .and_then(|m| m.flags)
        .unwrap_or_default())
}

/// Adds distinct keywords to one message until the server refuses, silently
/// drops one, or [`KEYWORD_ATTEMPTS`] are stored.
async fn keyword_limit(session: &mut TokioSession, mailbox: &str) -> Result<Value> {
    let uid = session
        .append(mailbox, &[], &message("limit"))
        .await?
        .uid
        .context("server did not report APPENDUID")?;
    session.select(mailbox, SelectOptions::default()).await?;
    let mut first_error = None;
    let mut attempted = 0;
    for n in 0..KEYWORD_ATTEMPTS {
        let keyword = Flag::Keyword(format!("{LIMIT_KEYWORD_PREFIX}{n}"));
        attempted = n + 1;
        if let Err(e) = session
            .store_flags(&one(uid), StoreOp::Add, &[keyword], None)
            .await
        {
            first_error = Some(e.to_string());
            break;
        }
    }
    let stored = fetch_flags(session, uid)
        .await?
        .iter()
        .filter(|f| matches!(f, Flag::Keyword(k) if k.starts_with(LIMIT_KEYWORD_PREFIX)))
        .count();
    Ok(json!({
        "attempts_bound": KEYWORD_ATTEMPTS,
        "attempted": attempted,
        "stored": stored,
        "error": first_error,
        "limit_found": stored < attempted,
    }))
}

/// COPYs the probe message and compares what the server reports for the
/// original and the copy.
async fn copy_probe(
    session: &mut TokioSession,
    first: &str,
    second: &str,
    uid: Uid,
    objectid: bool,
) -> Result<Value> {
    let spec = FetchSpec {
        size: true,
        identity: true,
        ..FetchSpec::default()
    };
    session.select(first, SelectOptions::default()).await?;
    let copied = session.copy_messages(&one(uid), second).await?;
    let original = session.fetch(&one(uid), &spec).await?;
    let original = original
        .into_iter()
        .find(|m| m.uid == Some(uid))
        .context("original not fetched")?;

    session.select(second, SelectOptions::default()).await?;
    let all = session
        .uid_search(tagmail_imap::SearchCriteria::All)
        .await?;
    let copy = session.fetch(&all, &spec).await?;
    let copy = copy.into_iter().next().context("copy not fetched")?;

    let mut result = json!({
        "copyuid_reported": copied.copy_uid.is_some(),
        "size_original": original.size,
        "size_copy": copy.size,
        "size_differs": original.size != copy.size,
    });
    if objectid {
        result["email_id_original"] = json!(original.email_id);
        result["email_id_copy"] = json!(copy.email_id);
        result["email_id_equal"] = json!(original.email_id == copy.email_id);
    }
    Ok(result)
}

async fn cleanup(session: &mut TokioSession, scratch: &[String]) {
    if scratch.is_empty() {
        return;
    }
    let _ = session
        .select(
            "INBOX",
            SelectOptions {
                read_only: true,
                ..SelectOptions::default()
            },
        )
        .await;
    for name in scratch {
        if let Err(e) = session.delete_mailbox(name).await {
            eprintln!("warning: could not delete scratch mailbox {name}: {e}");
        }
    }
}

fn print_human(report: &Value) {
    println!(
        "Server {}:{}",
        report["host"].as_str().unwrap_or("?"),
        report["port"]
    );
    println!("Capabilities: {}", join(&report["capabilities"]));
    for (name, value) in report["features"].as_object().into_iter().flatten() {
        println!("  {name}: {}", show(value));
    }
    println!("Special-use mailboxes:");
    match report["special_use"].as_array() {
        Some(list) if list.is_empty() => println!("  none"),
        Some(list) => {
            for b in list {
                println!("  {} = {}", show(&b["mailbox"]), show(&b["role"]));
            }
        }
        None => println!("  {}", report["special_use"]),
    }
    println!(
        "PERMANENTFLAGS: {} (\\* arbitrary keywords: {})",
        join(&report["permanent_flags"]["flags"]),
        report["permanent_flags"]["arbitrary_keywords"]
    );
    print_section("Keyword persistence", &report["keyword_persistence"]);
    print_section("Keyword limit", &report["keyword_limit"]);
    print_section("COPY", &report["copy"]);
    println!("Manual checks:");
    for item in report["manual"].as_array().into_iter().flatten() {
        println!("  - {}", item.as_str().unwrap_or(""));
    }
}

fn print_section(title: &str, value: &Value) {
    println!("{title}:");
    for (key, v) in value.as_object().into_iter().flatten() {
        println!("  {key}: {}", show(v));
    }
}

fn show(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn join(value: &Value) -> String {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
