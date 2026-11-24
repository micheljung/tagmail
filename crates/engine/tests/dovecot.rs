//! Engine integration tests against the Dovecot test server (`cargo xtask
//! dovecot up`). Run with `cargo test -p tagmail-engine --features
//! dovecot-tests --test dovecot`; `TAGMAIL_IMAP_ADDR` overrides the default
//! `127.0.0.1:10143`. Each test owns one of the `engineN` users and empties
//! it first.
#![cfg(feature = "dovecot-tests")]

mod common;

use std::time::Duration;

use common::*;
use tagmail_api_core::{AccountRegistry, EventSource};
use tagmail_core::{AccountId, ChangeCursor};
use tagmail_imap::*;
use tokio::time::Instant;

/// The `Date` header of `common::message`.
const MESSAGE_DATE: &str = "Sun, 22 Nov 2026 10:00:00 +0000";

#[tokio::test]
async fn restart_mid_sync_resumes_without_refetching_committed_windows() {
    let fixture = Fixture::new("engine1").await;
    let mut s = second("engine1").await;
    let run = nanos();
    for i in 0..1200 {
        s.append(
            "INBOX",
            &[],
            &message(&format!("r{i}-{run}@it"), "Bulk", &[]),
        )
        .await
        .unwrap();
    }

    let first = fixture.start(no_override(), Some(1)).await;
    eventually(
        "first window committed, then a failure",
        Duration::from_secs(30),
        || async {
            first
                .engine
                .runtime_status(AccountId(1))
                .is_some_and(|s| s.last_error.is_some())
        },
    )
    .await;
    let committed: Vec<u32> = log(&first.log).full.iter().map(|(_, u)| *u).collect();
    let cursor = first
        .query(
            "SELECT sync_state, sync_cursor_uid FROM mailbox WHERE name = 'INBOX'",
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<u32>>(1)?)),
        )
        .await;
    first.stop().await;
    assert_eq!(committed.len(), 500);
    assert_eq!(cursor[0].0, "initial");
    assert_eq!(cursor[0].1, committed.iter().min().copied());

    let second_run = fixture.start(no_override(), None).await;
    second_run.wait_live().await;
    let refetched: Vec<u32> = log(&second_run.log)
        .full
        .iter()
        .filter(|(m, _)| m == "INBOX")
        .map(|(_, u)| *u)
        .collect();
    assert!(
        refetched.iter().all(|u| !committed.contains(u)),
        "committed windows were fetched again"
    );
    assert_eq!(refetched.len(), 700);
    let count = second_run
        .query("SELECT count(*) FROM location", |r| r.get::<_, i64>(0))
        .await;
    assert_eq!(count, [1200]);
    second_run.stop().await;
}

#[tokio::test]
async fn append_by_another_client_reaches_the_change_feed_and_arrivals_within_10s() {
    let fixture = Fixture::new("engine2").await;
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    let mut feed = running.feed.subscribe();
    let mut arrivals = running.feed.subscribe_arrivals();
    let before: ChangeCursor = *feed.borrow_and_update();

    let id = format!("append-{}@it", nanos());
    let fresh = String::from_utf8(message(&id, "Fresh", &[]))
        .unwrap()
        .replace(
            MESSAGE_DATE,
            &mail_builder::headers::date::Date::now().to_rfc822(),
        );
    let mut s = second("engine2").await;
    let appended = Instant::now();
    s.append("INBOX", &[], fresh.as_bytes()).await.unwrap();
    tokio::time::timeout(LIVE_BUDGET, async {
        loop {
            feed.changed().await.unwrap();
            if !running.locations(&id).await.is_empty() {
                break;
            }
        }
    })
    .await
    .expect("APPEND not in the feed within 10 s");
    let elapsed = appended.elapsed();
    assert!(feed.borrow().seq > before.seq);
    println!("APPEND visible after {elapsed:?}");
    let arrival = tokio::time::timeout(LIVE_BUDGET.saturating_sub(elapsed), arrivals.recv())
        .await
        .expect("no arrival within 10 s")
        .unwrap();
    assert_eq!(arrival.len(), 1);
    assert_eq!(arrival[0].subject.as_deref(), Some("Fresh"));
    running.stop().await;
}

/// Flags, keywords, MOVE and EXPUNGE by a second session, for each
/// capability path Dovecot can be narrowed to.
#[tokio::test]
async fn external_changes_are_reflected_per_capability_path() {
    let paths: [(&'static str, &[&str]); 3] = [
        ("engine3", &[]),
        ("engine4", &["QRESYNC"]),
        ("engine5", &["CONDSTORE", "QRESYNC"]),
    ];
    for (user, remove) in paths {
        let caps = CapabilityOverride {
            remove: remove.iter().map(|s| (*s).to_owned()).collect(),
            add: Vec::new(),
        };
        let fixture = Fixture::new(user).await;
        let run = nanos();
        let a1 = format!("a1-{run}@it");
        let a2 = format!("a2-{run}@it");
        let b1 = format!("b1-{run}@it");
        let c1 = format!("c1-{run}@it");
        let mut s = second(user).await;
        let u1 = s
            .append("INBOX", &[], &message(&a1, "Lunch", &[]))
            .await
            .unwrap()
            .uid
            .unwrap();
        let u2 = s
            .append("INBOX", &[], &message(&a2, "Re: Lunch", &[&a1]))
            .await
            .unwrap()
            .uid
            .unwrap();
        let ub = s
            .append("Archive", &[], &message(&b1, "Invoice", &[]))
            .await
            .unwrap()
            .uid
            .unwrap();

        let running = fixture.start(caps, None).await;
        running.wait_live().await;
        let status = running.engine.runtime_status(AccountId(1)).unwrap();
        println!(
            "{user}: capabilities {:?} degraded {:?}",
            status.capabilities, status.degraded
        );
        assert_eq!(
            status.capabilities.contains(&"QRESYNC".to_owned()),
            remove.is_empty()
        );
        let b1_row = running.locations(&b1).await[0].1;

        s.select("INBOX", SelectOptions::default()).await.unwrap();
        let changed = Instant::now();
        let inbox_deadline = changed + LIVE_BUDGET;
        s.store_flags(
            &[u1].into_iter().collect(),
            StoreOp::Add,
            &[Flag::Flagged, Flag::Keyword("$label_Work".into())],
            None,
        )
        .await
        .unwrap();
        let mut raw = Raw::login(user).await;
        raw.cmd("SELECT INBOX").await;
        raw.cmd(&format!("UID STORE {} +FLAGS.SILENT (\\Deleted)", u2.0))
            .await;
        raw.cmd("EXPUNGE").await;
        s.select("Archive", SelectOptions::default()).await.unwrap();
        s.move_messages(&[ub].into_iter().collect(), "INBOX")
            .await
            .unwrap();
        s.append("Sent", &[], &message(&c1, "Re: Lunch", &[&a1]))
            .await
            .unwrap();

        let left = || inbox_deadline.saturating_duration_since(Instant::now());
        eventually("flags and keyword", left(), || async {
            running
                .locations(&a1)
                .await
                .first()
                .is_some_and(|l| l.2 == 2 && l.3 == "$label_Work")
        })
        .await;
        eventually("expunge", left(), || async {
            running.locations(&a2).await.is_empty()
        })
        .await;
        eventually("move", left(), || async {
            let locs = running.locations(&b1).await;
            locs.len() == 1 && locs[0].0 == "INBOX" && locs[0].1 == b1_row
        })
        .await;
        eventually("Sent reply after the next poll", LIVE_BUDGET, || async {
            !running.locations(&c1).await.is_empty()
        })
        .await;
        println!(
            "{user}: external changes visible after {:?}",
            changed.elapsed()
        );
        let threads = running
            .query(
                "SELECT count(DISTINCT thread_id) FROM message WHERE subject_norm = 'lunch' \
                 AND tombstoned_at IS NULL",
                |r| r.get::<_, i64>(0),
            )
            .await;
        assert_eq!(threads, [1], "Sent reply threads with the INBOX message");
        let labels = running
            .query("SELECT label_key FROM label", |r| r.get::<_, String>(0))
            .await;
        assert_eq!(labels, ["work"]);
        running.stop().await;
    }
}

#[tokio::test]
async fn recreated_mailbox_keeps_message_rows() {
    let fixture = Fixture::new("engine6").await;
    let run = nanos();
    let ids: Vec<String> = (0..3).map(|i| format!("u{i}-{run}@it")).collect();
    let mut s = second("engine6").await;
    s.create_mailbox("Box").await.unwrap();
    for id in &ids {
        s.append("Box", &[], &message(id, "Kept", &[]))
            .await
            .unwrap();
    }
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    let before = running
        .query("SELECT id, thread_id FROM message ORDER BY id", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .await;
    let uidvalidity = running
        .query("SELECT uidvalidity FROM mailbox WHERE name = 'Box'", |r| {
            r.get::<_, u32>(0)
        })
        .await;

    let mut raw = Raw::login("engine6").await;
    raw.cmd("DELETE Box").await;
    raw.cmd("CREATE Box").await;
    for id in &ids {
        s.append("Box", &[], &message(id, "Kept", &[]))
            .await
            .unwrap();
    }
    eventually(
        "resync with the new UIDVALIDITY",
        Duration::from_secs(20),
        || async {
            let now = running
                .query(
                    "SELECT uidvalidity, sync_state FROM mailbox WHERE name = 'Box'",
                    |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)),
                )
                .await;
            now.first()
                .is_some_and(|(v, s)| *v != uidvalidity[0] && s == "live")
                && running
                    .query("SELECT count(*) FROM location", |r| r.get::<_, i64>(0))
                    .await
                    == [3]
        },
    )
    .await;
    let after = running
        .query(
            "SELECT id, thread_id FROM message WHERE tombstoned_at IS NULL ORDER BY id",
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .await;
    assert_eq!(before, after, "messages matched back by email_id");
    running.stop().await;
}

#[tokio::test]
async fn text_pass_indexes_bodies() {
    let fixture = Fixture::new("engine7").await;
    let id = format!("t-{}@it", nanos());
    let mut s = second("engine7").await;
    s.append("INBOX", &[], &message(&id, "Searchable", &[]))
        .await
        .unwrap();
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    let needle = format!("zebra{}", id.len());
    eventually("body indexed", Duration::from_secs(20), || async {
        let needle = needle.clone();
        !running
            .cache
            .read(move |c| tagmail_store::fts::search(c, &needle, 10))
            .await
            .unwrap()
            .is_empty()
    })
    .await;
    running.stop().await;
}
