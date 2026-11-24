//! Sync algorithm tests against the in-memory server, for each capability
//! path (QRESYNC, CONDSTORE only, neither, NOMODSEQ, OBJECTID).

use std::collections::BTreeSet;

use tagmail_api_core::{Arrival, EventSource};
use tagmail_imap::{Flag, ImapSession};
use tagmail_store::fts;

use crate::cache::{self, SyncState};
use crate::fake::{FakeConnector, FakeMsg, FakeSession, Shared, lock, server};
use crate::sync::{self, Progress};
use crate::testutil::{TestCache, open};

const QRESYNC: &[&str] = &["IMAP4rev1", "ENABLE", "IDLE", "CONDSTORE", "QRESYNC"];
const CONDSTORE: &[&str] = &["IMAP4rev1", "IDLE", "CONDSTORE"];
const PLAIN: &[&str] = &["IMAP4rev1", "IDLE"];
const OBJECTID: &[&str] = &[
    "IMAP4rev1",
    "ENABLE",
    "IDLE",
    "CONDSTORE",
    "QRESYNC",
    "OBJECTID",
];

const DAY: i64 = 86_400;
const T0: i64 = 1_790_000_000;

fn never() -> bool {
    false
}

async fn sync_box(t: &TestCache, s: &mut FakeSession, name: &str) -> Progress {
    let rows = sync::ordered(&t.ctx).await.unwrap();
    let row = rows.iter().find(|r| r.name == name).unwrap();
    sync::sync_mailbox(s, &t.ctx, row, &never).await.unwrap()
}

async fn sync_all(t: &TestCache, s: &mut FakeSession) {
    for row in sync::list(s, &t.ctx).await.unwrap() {
        assert_eq!(
            sync::sync_mailbox(s, &t.ctx, &row, &never).await.unwrap(),
            Progress::Done
        );
    }
}

/// (mailbox, uid, msgid, flags, keywords)
type Loc = (String, u32, String, u8, String);

async fn locations(t: &TestCache) -> Vec<Loc> {
    t.query(
        "SELECT mb.name, l.uid, m.msgid_hdr, l.flags, l.keywords FROM location l \
         JOIN mailbox mb ON mb.id = l.mailbox_id JOIN message m ON m.id = l.message_id \
         ORDER BY mb.name, l.uid",
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .await
}

/// msgid -> (message id, thread id, tombstoned)
async fn messages(t: &TestCache) -> Vec<(String, i64, i64, bool)> {
    t.query(
        "SELECT msgid_hdr, id, thread_id, tombstoned_at IS NOT NULL FROM message ORDER BY msgid_hdr",
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .await
}

async fn labels(t: &TestCache, msgid: &'static str) -> Vec<String> {
    let all = t
        .query(
            "SELECT m.msgid_hdr, l.label_key FROM message_label ml \
             JOIN message m ON m.id = ml.message_id JOIN label l ON l.id = ml.label_id \
             ORDER BY l.label_key",
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .await;
    all.into_iter()
        .filter(|(m, _)| m == msgid)
        .map(|(_, l)| l)
        .collect()
}

fn session(server: &Shared) -> FakeSession {
    FakeConnector(server.clone()).session()
}

fn seed(server: &Shared) {
    let mut s = lock(server);
    s.append(
        "INBOX",
        FakeMsg::new("a1@x", "Lunch", "ann@x", "me@x", T0).flags(&[Flag::Seen]),
    );
    s.append(
        "INBOX",
        FakeMsg::new("a2@x", "Re: Lunch", "bob@x", "me@x", T0 + DAY).refs(&["a1@x"]),
    );
    s.append(
        "Sent",
        FakeMsg::new("a3@x", "Re: Lunch", "me@x", "ann@x", T0 + 2 * DAY).refs(&["a1@x"]),
    );
    s.append(
        "Archive",
        FakeMsg::new("b1@x", "Invoice", "shop@y", "me@x", T0 + 3 * DAY),
    );
}

#[tokio::test]
async fn initial_sync_threads_across_folders_and_goes_live() {
    for caps in [QRESYNC, CONDSTORE, PLAIN, OBJECTID] {
        let t = open();
        let srv = server(caps);
        seed(&srv);
        let mut s = session(&srv);
        sync_all(&t, &mut s).await;

        let msgs = messages(&t).await;
        assert_eq!(msgs.len(), 4, "{caps:?}");
        let thread = |id: &str| msgs.iter().find(|m| m.0 == id).unwrap().2;
        assert_eq!(thread("a1@x"), thread("a2@x"));
        assert_eq!(thread("a1@x"), thread("a3@x"));
        assert_ne!(thread("a1@x"), thread("b1@x"));
        let rows = sync::ordered(&t.ctx).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["INBOX", "Sent", "Archive"]
        );
        for r in &rows {
            assert_eq!(r.state, SyncState::Live, "{caps:?} {}", r.name);
            assert_eq!(r.highestmodseq.is_some(), caps.contains(&"CONDSTORE"));
        }
        let tm = t
            .query(
                "SELECT count(*) FROM thread_mailbox WHERE thread_id = (SELECT thread_id FROM message WHERE msgid_hdr = 'a1@x')",
                |r| r.get::<_, i64>(0),
            )
            .await;
        assert_eq!(tm, [2], "INBOX and Sent list the conversation");
        let bulk = t
            .query(
                "SELECT count(*) FROM change_log WHERE op LIKE 'bulk:%'",
                |r| r.get::<_, i64>(0),
            )
            .await;
        assert!(bulk[0] >= 3, "one bulk entry per mailbox window");
        let per_message = t
            .query(
                "SELECT count(*) FROM change_log WHERE entity IN ('message', 'thread') AND op = 'upsert'",
                |r| r.get::<_, i64>(0),
            )
            .await;
        assert_eq!(per_message, [0], "initial sync emits bulk entries only");
        let ids = t
            .query("SELECT email_id FROM message", |r| r.get::<_, String>(0))
            .await;
        let prefix = if caps.contains(&"OBJECTID") {
            "o:"
        } else {
            "h:"
        };
        assert!(ids.iter().all(|i| i.starts_with(prefix)), "{ids:?}");
        assert_eq!(
            *t.feed.subscribe().borrow(),
            t.ctx.cache.head().await.unwrap()
        );
    }
}

#[tokio::test]
async fn restart_mid_sync_resumes_without_refetching_committed_windows() {
    let t = open();
    let srv = server(QRESYNC);
    {
        let mut s = lock(&srv);
        for i in 0..1200 {
            s.append(
                "INBOX",
                FakeMsg::new(&format!("m{i}@x"), &format!("S{i}"), "a@x", "b@x", T0 + i),
            );
        }
        s.fail_after_full_fetches = Some(1);
    }
    let mut first = session(&srv);
    let rows = sync::list(&mut first, &t.ctx).await.unwrap();
    let inbox = rows.iter().find(|r| r.is_inbox()).unwrap().clone();
    let err = sync::sync_mailbox(&mut first, &t.ctx, &inbox, &never).await;
    assert!(err.unwrap_err().is_fatal());
    let committed: BTreeSet<u32> = lock(&srv).full_fetched("INBOX").into_iter().collect();
    assert_eq!(committed.len(), 500);
    assert_eq!(committed.iter().min(), Some(&701), "newest window first");
    let row = sync::ordered(&t.ctx).await.unwrap().remove(0);
    assert_eq!((row.state, row.cursor), (SyncState::Initial, Some(701)));

    {
        let mut s = lock(&srv);
        s.fail_after_full_fetches = None;
        s.fetches.clear();
    }
    let mut second = session(&srv);
    assert_eq!(sync_box(&t, &mut second, "INBOX").await, Progress::Done);
    let refetched: BTreeSet<u32> = lock(&srv).full_fetched("INBOX").into_iter().collect();
    assert!(
        refetched.is_disjoint(&committed),
        "committed windows refetched"
    );
    assert_eq!(refetched.len(), 700);
    assert_eq!(messages(&t).await.len(), 1200);
}

#[tokio::test]
async fn bulk_windows_carry_the_uid_range_they_added() {
    let t = open();
    let srv = server(QRESYNC);
    {
        let mut s = lock(&srv);
        for i in 0..1100 {
            s.append(
                "INBOX",
                FakeMsg::new(&format!("m{i}@x"), "S", "a@x", "b@x", T0 + i),
            );
        }
    }
    let head = t.ctx.cache.head().await.unwrap();
    let mut s = session(&srv);
    let rows = sync::list(&mut s, &t.ctx).await.unwrap();
    let inbox = rows.iter().find(|r| r.is_inbox()).unwrap().clone();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let after_one = move || calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1;
    sync::sync_mailbox(&mut s, &t.ctx, &inbox, &after_one)
        .await
        .unwrap();
    for i in 0..600 {
        lock(&srv).append(
            "INBOX",
            FakeMsg::new(&format!("n{i}@x"), "N", "c@x", "b@x", T0 + 5000 + i),
        );
    }
    assert_eq!(sync_box(&t, &mut s, "INBOX").await, Progress::Done);

    let tagmail_store::Changes::Page { changes, .. } =
        t.ctx.cache.changes_since(head, 10_000).await.unwrap()
    else {
        panic!("gone");
    };
    let ranges: Vec<String> = changes
        .iter()
        .filter(|c| matches!(c.op, tagmail_store::ChangeOp::Bulk { .. }))
        .map(|c| c.payload.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        ranges,
        [
            r#"{"uids":[601,1100]}"#,
            r#"{"uids":[101,600]}"#,
            r#"{"uids":[1,100]}"#,
            r#"{"uids":[1201,1700]}"#,
            r#"{"uids":[1101,1200]}"#,
        ]
    );
}

#[tokio::test]
async fn changes_during_initial_sync_are_caught_up() {
    for caps in [QRESYNC, CONDSTORE, PLAIN] {
        let t = open();
        let srv = server(caps);
        {
            let mut s = lock(&srv);
            for i in 0..1100 {
                s.append(
                    "INBOX",
                    FakeMsg::new(&format!("m{i}@x"), "S", "a@x", "b@x", T0 + i),
                );
            }
        }
        let mut s = session(&srv);
        let rows = sync::list(&mut s, &t.ctx).await.unwrap();
        let inbox = rows.iter().find(|r| r.is_inbox()).unwrap().clone();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let after_one = move || calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1;
        assert_eq!(
            sync::sync_mailbox(&mut s, &t.ctx, &inbox, &after_one)
                .await
                .unwrap(),
            Progress::Paused
        );
        {
            let mut srv = lock(&srv);
            srv.set_flags(
                "INBOX",
                1100,
                &[Flag::Flagged, Flag::Keyword("$label_Late".into())],
            );
            srv.expunge("INBOX", 1099);
            srv.expunge("INBOX", 5);
            srv.append(
                "INBOX",
                FakeMsg::new("new@x", "New", "c@x", "b@x", T0 + 5000),
            );
        }
        assert_eq!(
            sync_box(&t, &mut s, "INBOX").await,
            Progress::Done,
            "{caps:?}"
        );
        let locs = locations(&t).await;
        assert_eq!(locs.len(), 1099, "{caps:?}");
        assert!(locs.iter().all(|l| l.1 != 1099 && l.1 != 5), "{caps:?}");
        let flagged = locs.iter().find(|l| l.1 == 1100).unwrap();
        assert_eq!(
            (flagged.3, flagged.4.as_str()),
            (2, "$label_Late"),
            "{caps:?}"
        );
        assert!(locs.iter().any(|l| l.2 == "new@x"), "{caps:?}");
        let tombstoned = messages(&t).await.iter().filter(|m| m.3).count();
        assert_eq!(
            tombstoned, 1,
            "only the committed expunged message ({caps:?})"
        );
    }
}

#[tokio::test]
async fn external_changes_are_applied_per_capability_path() {
    for (caps, nomodseq) in [
        (QRESYNC, false),
        (CONDSTORE, false),
        (PLAIN, false),
        (QRESYNC, true),
    ] {
        let t = open();
        let srv = server(caps);
        seed(&srv);
        if nomodseq {
            lock(&srv).mailbox("INBOX").nomodseq = true;
        }
        let mut s = session(&srv);
        sync_all(&t, &mut s).await;
        let before = messages(&t).await;
        let b1 = before.iter().find(|m| m.0 == "b1@x").unwrap().1;
        let head = t.ctx.cache.head().await.unwrap();
        {
            let mut srv = lock(&srv);
            srv.set_flags(
                "INBOX",
                1,
                &[Flag::Flagged, Flag::Keyword("$label_Work".into())],
            );
            srv.expunge("INBOX", 2);
            srv.move_msg("Archive", 1, "INBOX");
            srv.append(
                "INBOX",
                FakeMsg::new("c1@x", "Hello", "dan@z", "me@x", T0 + 9 * DAY),
            );
        }
        sync_box(&t, &mut s, "INBOX").await;
        sync_box(&t, &mut s, "Archive").await;

        let tag = format!("{caps:?} nomodseq={nomodseq}");
        let locs = locations(&t).await;
        let a1 = locs.iter().find(|l| l.2 == "a1@x").unwrap();
        assert_eq!((a1.3, a1.4.as_str()), (2, "$label_Work"), "{tag}");
        assert_eq!(labels(&t, "a1@x").await, ["work"], "{tag}");
        assert!(!locs.iter().any(|l| l.2 == "a2@x"), "{tag}");
        let b1_locs: Vec<&Loc> = locs.iter().filter(|l| l.2 == "b1@x").collect();
        assert_eq!(b1_locs.len(), 1, "{tag}");
        assert_eq!(b1_locs[0].0, "INBOX", "{tag}");
        assert!(locs.iter().any(|l| l.2 == "c1@x"), "{tag}");
        let after = messages(&t).await;
        let row = |id: &str| after.iter().find(|m| m.0 == id).unwrap().clone();
        assert_eq!(row("b1@x").1, b1, "moved message keeps its row ({tag})");
        assert!(!row("b1@x").3, "{tag}");
        assert!(row("a2@x").3, "expunged message is tombstoned ({tag})");

        let changes = t.ctx.cache.changes_since(head, 1000).await.unwrap();
        let tagmail_store::Changes::Page { changes, .. } = changes else {
            panic!("gone");
        };
        let thread_upserts = changes
            .iter()
            .filter(|c| c.entity == tagmail_store::Entity::Thread)
            .count();
        assert!(thread_upserts >= 3, "{tag}: {changes:?}");
        assert!(
            changes
                .iter()
                .any(|c| c.entity == tagmail_store::Entity::Message
                    && c.op == tagmail_store::ChangeOp::Delete)
        );
        let last_date = changes
            .iter()
            .find(|c| {
                c.entity == tagmail_store::Entity::Thread && c.op == tagmail_store::ChangeOp::Upsert
            })
            .and_then(|c| c.payload.clone())
            .unwrap();
        assert!(last_date.contains("lastDate"), "{last_date}");
        assert!(
            last_date.contains(&format!("\"accountId\":{}", t.ctx.account.0)),
            "{last_date}"
        );
        assert_eq!(
            *t.feed.subscribe().borrow(),
            t.ctx.cache.head().await.unwrap()
        );

        assert_eq!(
            sync::purge(&t.ctx).await.unwrap(),
            0,
            "Sent not polled since ({tag})"
        );
        sync_box(&t, &mut s, "Sent").await;
        assert_eq!(sync::purge(&t.ctx).await.unwrap(), 1, "{tag}");
        assert!(messages(&t).await.iter().all(|m| m.0 != "a2@x"));
    }
}

#[tokio::test]
async fn move_seen_as_delete_then_add_keeps_the_message_through_grace() {
    let t = open();
    let srv = server(QRESYNC);
    seed(&srv);
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    let a2 = messages(&t)
        .await
        .into_iter()
        .find(|m| m.0 == "a2@x")
        .unwrap();
    lock(&srv).move_msg("INBOX", 2, "Archive");

    sync_box(&t, &mut s, "INBOX").await;
    sync_box(&t, &mut s, "Sent").await;
    assert_eq!(
        sync::purge(&t.ctx).await.unwrap(),
        0,
        "Archive not polled yet"
    );
    let tomb = messages(&t)
        .await
        .into_iter()
        .find(|m| m.0 == "a2@x")
        .unwrap();
    assert!(tomb.3);
    let visible = t
        .query("SELECT msg_count FROM thread WHERE id = (SELECT thread_id FROM message WHERE msgid_hdr = 'a1@x')", |r| r.get::<_, i64>(0))
        .await;
    assert_eq!(visible, [2], "tombstoned message is hidden");

    sync_box(&t, &mut s, "Archive").await;
    let back = messages(&t)
        .await
        .into_iter()
        .find(|m| m.0 == "a2@x")
        .unwrap();
    assert_eq!((back.1, back.2, back.3), (a2.1, a2.2, false));
    assert_eq!(sync::purge(&t.ctx).await.unwrap(), 0);
}

#[tokio::test]
async fn uidvalidity_change_keeps_message_rows() {
    for caps in [QRESYNC, PLAIN] {
        let t = open();
        let srv = server(caps);
        seed(&srv);
        let mut s = session(&srv);
        sync_all(&t, &mut s).await;
        let before = messages(&t).await;
        {
            let mut srv = lock(&srv);
            srv.expunge("INBOX", 1);
            srv.renumber("INBOX");
        }
        sync_box(&t, &mut s, "INBOX").await;
        let after = messages(&t).await;
        let a2_before = before.iter().find(|m| m.0 == "a2@x").unwrap();
        let a2_after = after.iter().find(|m| m.0 == "a2@x").unwrap();
        assert_eq!(a2_before, a2_after, "{caps:?}");
        let inbox: Vec<Loc> = locations(&t)
            .await
            .into_iter()
            .filter(|l| l.0 == "INBOX")
            .collect();
        assert_eq!(inbox.len(), 1, "{caps:?}");
        assert_eq!((inbox[0].1, inbox[0].2.as_str()), (1, "a2@x"));
        let row = sync::ordered(&t.ctx).await.unwrap().remove(0);
        assert_eq!((row.uidvalidity, row.state), (101, SyncState::Live));
        assert!(
            after.iter().find(|m| m.0 == "a1@x").unwrap().3,
            "a1 gone from INBOX"
        );
    }
}

#[tokio::test]
async fn labels_are_the_union_over_locations() {
    let t = open();
    let srv = server(QRESYNC);
    {
        let mut s = lock(&srv);
        let m = FakeMsg::new("d1@x", "Copy", "ann@x", "me@x", T0);
        s.append(
            "INBOX",
            m.clone().flags(&[Flag::Keyword("$label_A".into())]),
        );
        s.append(
            "Archive",
            m.flags(&[
                Flag::Seen,
                Flag::Keyword("Work".into()),
                Flag::Keyword("$Junk".into()),
            ]),
        );
    }
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    assert_eq!(messages(&t).await.len(), 1, "copies share one message row");
    assert_eq!(labels(&t, "d1@x").await, ["a", "work"]);
    let unread = t
        .query("SELECT unread_count FROM thread", |r| r.get::<_, i64>(0))
        .await;
    assert_eq!(unread, [1], "seen only when every location is seen");

    lock(&srv).set_flags("INBOX", 1, &[Flag::Seen]);
    sync_box(&t, &mut s, "INBOX").await;
    assert_eq!(labels(&t, "d1@x").await, ["work"]);
    let unread = t
        .query("SELECT unread_count FROM thread", |r| r.get::<_, i64>(0))
        .await;
    assert_eq!(unread, [0]);
}

#[tokio::test]
async fn text_pass_stores_text_snippet_and_body_index() {
    let t = open();
    let srv = server(CONDSTORE);
    {
        let mut s = lock(&srv);
        for i in 0..1200 {
            let mut m = FakeMsg::new(&format!("m{i}@x"), "S", "a@x", "b@x", T0 + i);
            m.body = format!("hello zebra{i} world");
            s.append("INBOX", m);
        }
    }
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    let no_snippet = t
        .query("SELECT count(*) FROM message WHERE snippet IS NULL", |r| {
            r.get::<_, i64>(0)
        })
        .await;
    assert_eq!(
        no_snippet,
        [200],
        "snippets for the newest two windows only"
    );
    let mut skip = BTreeSet::new();
    while sync::text_pass(&mut s, &t.ctx, 300, &mut skip)
        .await
        .unwrap()
        > 0
    {}
    let pending = t
        .query(
            "SELECT count(*) FROM message WHERE text_blob IS NULL OR snippet IS NULL",
            |r| r.get::<_, i64>(0),
        )
        .await;
    assert_eq!(pending, [0]);
    let hits = t.ctx.read(|c| fts::search(c, "zebra7", 10)).await.unwrap();
    assert_eq!(hits.len(), 1);
    let key = t
        .query(
            "SELECT text_blob FROM message WHERE msgid_hdr = 'm7@x'",
            |r| r.get::<_, String>(0),
        )
        .await;
    let blob = tagmail_store::BlobKey::parse(&key[0]).unwrap();
    assert_eq!(
        t.ctx.blobs.read_text(&blob).unwrap().as_deref(),
        Some("hello zebra7 world")
    );
}

#[tokio::test]
async fn unchanged_mailboxes_are_skipped_by_status() {
    let t = open();
    let srv = server(CONDSTORE);
    seed(&srv);
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    sync_box(&t, &mut s, "INBOX").await;
    lock(&srv).fetches.clear();
    let before = cache_row(&t, "Archive").await;
    sync_box(&t, &mut s, "Archive").await;
    assert!(lock(&srv).fetches.is_empty());
    assert!(cache_row(&t, "Archive").await.last_poll_at > before.last_poll_at);
    assert!(s.selected() != Some("Archive"), "STATUS only, no SELECT");
}

async fn cache_row(t: &TestCache, name: &'static str) -> cache::MailboxRow {
    sync::ordered(&t.ctx)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap()
}

#[tokio::test]
async fn pending_text_reads_its_partial_index_in_order() {
    let t = open();
    let plan = t
        .ctx
        .read(|c| {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {}", cache::PENDING_TEXT_SQL))?;
            let rows = stmt.query_map([1, 50], |r| r.get::<_, String>(3))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(
        plan.iter()
            .any(|d| d.starts_with("SEARCH m USING INDEX message_pending_text")),
        "{plan:?}"
    );
    assert!(!plan.iter().any(|d| d.contains("TEMP B-TREE")), "{plan:?}");
}

fn now_secs() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<[Arrival]>>) -> Vec<Arrival> {
    let mut out = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        out.extend(batch.iter().cloned());
    }
    out
}

type ServerChange = Box<dyn FnOnce(&mut crate::fake::FakeServer)>;

/// Syncs the seeded account to live, applies `change` on the server and
/// returns the arrivals the next sync of every mailbox published.
async fn arrivals_after(change: impl FnOnce(&mut crate::fake::FakeServer)) -> Vec<Arrival> {
    let t = open();
    let srv = server(QRESYNC);
    seed(&srv);
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    let mut rx = t.feed.subscribe_arrivals();
    change(&mut lock(&srv));
    for name in ["INBOX", "Sent", "Archive"] {
        assert_eq!(sync_box(&t, &mut s, name).await, Progress::Done);
    }
    drain(&mut rx)
}

#[tokio::test]
async fn a_new_unseen_inbox_message_is_one_arrival() {
    let date = now_secs() - 60;
    let got = arrivals_after(|s| {
        s.append(
            "INBOX",
            FakeMsg::new("n1@x", "Re: Lunch", "cat@x", "me@x", date).refs(&["a1@x"]),
        );
    })
    .await;
    assert_eq!(got.len(), 1, "{got:?}");
    let a = &got[0];
    assert_eq!(a.account_id, 1);
    assert_eq!(a.subject.as_deref(), Some("Re: Lunch"));
    assert_eq!(a.from.as_ref().map(|f| f.email.as_str()), Some("cat@x"));
    assert_eq!(a.date, date);
    assert!(a.thread_id.is_some());
}

#[tokio::test]
async fn an_initial_sync_publishes_no_arrivals() {
    let t = open();
    let srv = server(QRESYNC);
    lock(&srv).append(
        "INBOX",
        FakeMsg::new("n1@x", "Hi", "cat@x", "me@x", now_secs()),
    );
    let mut rx = t.feed.subscribe_arrivals();
    sync_all(&t, &mut session(&srv)).await;
    assert_eq!(drain(&mut rx), []);
}

#[tokio::test]
async fn only_new_unseen_recent_inbox_mail_arrives() {
    let now = now_secs();
    let cases: [(&str, ServerChange); 4] = [
        (
            "flag change",
            Box::new(|s| {
                let uid = s.uid_of("INBOX", "a2@x").unwrap();
                s.set_flags("INBOX", uid, &[Flag::Flagged]);
            }),
        ),
        (
            "\\Seen",
            Box::new(move |s| {
                s.append(
                    "INBOX",
                    FakeMsg::new("n1@x", "Hi", "cat@x", "me@x", now).flags(&[Flag::Seen]),
                );
            }),
        ),
        (
            "not the inbox",
            Box::new(move |s| {
                s.append("Archive", FakeMsg::new("n1@x", "Hi", "cat@x", "me@x", now));
            }),
        ),
        (
            "older than a day",
            Box::new(move |s| {
                s.append(
                    "INBOX",
                    FakeMsg::new("n1@x", "Hi", "cat@x", "me@x", now - 2 * DAY),
                );
            }),
        ),
    ];
    for (case, change) in cases {
        assert_eq!(arrivals_after(change).await, [], "{case}");
    }
}

#[tokio::test]
async fn a_known_message_copied_into_the_inbox_is_no_arrival() {
    let t = open();
    let srv = server(QRESYNC);
    seed(&srv);
    lock(&srv).append(
        "Archive",
        FakeMsg::new("n1@x", "Hi", "cat@x", "me@x", now_secs()),
    );
    let mut s = session(&srv);
    sync_all(&t, &mut s).await;
    let mut rx = t.feed.subscribe_arrivals();
    {
        let mut srv = lock(&srv);
        let uid = srv.uid_of("Archive", "n1@x").unwrap();
        srv.copy_msg("Archive", uid, "INBOX");
    }
    assert_eq!(sync_box(&t, &mut s, "INBOX").await, Progress::Done);
    assert_eq!(
        locations(&t).await.iter().filter(|l| l.2 == "n1@x").count(),
        2
    );
    assert_eq!(drain(&mut rx), []);
}

#[tokio::test]
async fn a_rolled_back_transaction_publishes_no_arrivals() {
    let t = open();
    let srv = server(QRESYNC);
    sync_all(&t, &mut session(&srv)).await;
    let inbox = cache_row(&t, "INBOX").await.id;
    let day = (now_secs() - 1_700_000_000) / DAY;
    let fetched = vec![crate::testutil::msg("n1", "Hi", "cat@x", "me@x", day).fetched(1)];
    let mut rx = t.feed.subscribe_arrivals();
    let failed = t
        .ctx
        .write(move |w| {
            cache::add_messages(w, inbox, &fetched)?;
            Err::<(), _>(tagmail_store::Error::Corrupt("rolled back".into()))
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(drain(&mut rx), []);
    assert_eq!(messages(&t).await, []);
}
