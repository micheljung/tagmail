//! AC 6: delete `cache.db` and the blobs, restart, resync. Labels per
//! message and the thread partition come back identical, and ops queued
//! before the wipe (plus a half-done label job) apply to the right
//! mailboxes or fail visibly, checked over raw IMAP.
#![cfg(feature = "dovecot-tests")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use common::*;
use tagmail_api_core::ops_dto::{LabelPatchDto, NewLabelDto, OpStatusDto};
use tagmail_api_core::{OpMailbox, OpRequest, WriteService};
use tagmail_core::ops::OpKind;
use tagmail_core::{MailboxRowId, ThreadId};
use tagmail_imap::*;

const APPLY: Duration = Duration::from_secs(30);

fn op(id: &str, kind: OpKind, thread: ThreadId) -> OpRequest {
    OpRequest {
        op_id: id.to_owned(),
        kind,
        thread_ids: vec![thread],
        email_ids: Vec::new(),
        label: None,
        mailbox: None,
    }
}

/// Label keys per email id.
async fn labels(running: &Running) -> BTreeMap<String, BTreeSet<String>> {
    let rows = running
        .query(
            "SELECT m.email_id, l.label_key FROM message m \
             LEFT JOIN message_label ml ON ml.message_id = m.id \
             LEFT JOIN label l ON l.id = ml.label_id WHERE m.tombstoned_at IS NULL",
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .await;
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (email, key) in rows {
        let set = out.entry(email).or_default();
        set.extend(key);
    }
    out
}

/// Threads as sets of email ids.
async fn partition(running: &Running) -> BTreeSet<BTreeSet<String>> {
    let rows = running
        .query(
            "SELECT thread_id, email_id FROM message WHERE tombstoned_at IS NULL",
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .await;
    let mut threads: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for (t, e) in rows {
        threads.entry(t).or_default().insert(e);
    }
    threads.into_values().collect()
}

async fn thread_of(running: &Running, msgid: &str) -> ThreadId {
    let rows = running
        .query("SELECT thread_id, msgid_hdr FROM message", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .await;
    ThreadId(
        rows.into_iter()
            .find(|(_, m)| m.as_deref() == Some(msgid))
            .expect("message cached")
            .0,
    )
}

async fn mailbox_id(running: &Running, name: &str) -> i64 {
    running
        .query("SELECT id, name FROM mailbox", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .await
        .into_iter()
        .find(|(_, n)| n == name)
        .expect("mailbox cached")
        .0
}

/// `(Message-ID, FLAGS line)` of every message in `mailbox`.
async fn server_state(user: &str, mailbox: &str) -> Vec<String> {
    let mut raw = Raw::login(user).await;
    raw.cmd(&format!("SELECT \"{mailbox}\"")).await;
    let lines = raw
        .cmd("UID FETCH 1:* (FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])")
        .await;
    raw.cmd("LOGOUT").await;
    let mut out = Vec::new();
    let mut flags = String::new();
    for l in lines {
        if l.contains("FETCH") {
            flags = l;
        } else if let Some(id) = l.strip_prefix("Message-ID: ") {
            out.push(format!("{id} {flags}"));
        }
    }
    out
}

/// The UID of the message with this Message-ID, read from the headers
/// because Stalwart's `UID SEARCH HEADER` finds nothing (docs/servers.md).
async fn uid_of(user: &str, mailbox: &str, msgid: &str) -> Uid {
    let mut raw = Raw::login(user).await;
    raw.cmd(&format!("SELECT \"{mailbox}\"")).await;
    let lines = raw
        .cmd("UID FETCH 1:* (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])")
        .await;
    raw.cmd("LOGOUT").await;
    let mut uid = None;
    for l in lines {
        if let Some((_, rest)) = l.split_once("UID ") {
            uid = rest.split_whitespace().next().and_then(|u| u.parse().ok());
        } else if l == format!("Message-ID: <{msgid}>") {
            return Uid(uid.expect("UID before the header"));
        }
    }
    panic!("{msgid} not in {mailbox}")
}

fn has(state: &[String], id: &str, flag: &str) -> bool {
    state
        .iter()
        .any(|l| l.starts_with(&format!("<{id}>")) && l.contains(flag))
}

fn present(state: &[String], id: &str) -> bool {
    state.iter().any(|l| l.starts_with(&format!("<{id}>")))
}

#[tokio::test]
async fn wipe_resync_keeps_labels_and_applies_queued_ops() {
    let user = "ops3";
    let fixture = Fixture::new(user).await;
    let mut s = second(user).await;
    let run = nanos();
    let [root, reply, archived, starred, orphan] =
        ["root", "reply", "arch", "star", "orphan"].map(|n| format!("{n}-{run}@it"));
    s.append(
        "INBOX",
        &[Flag::Keyword("$label_Keep".into())],
        &message(&root, "Thread", &[]),
    )
    .await
    .unwrap();
    s.append(
        sent(),
        &[Flag::Seen],
        &message(&reply, "Re: Thread", &[&root]),
    )
    .await
    .unwrap();
    s.append(
        "Archive",
        &[Flag::Keyword("$label_Keep".into())],
        &message(&root, "Thread", &[]),
    )
    .await
    .unwrap();
    for id in [&archived, &starred, &orphan] {
        s.append("INBOX", &[], &message(id, id, &[])).await.unwrap();
    }
    s.create_mailbox("Doomed").await.unwrap();

    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    running
        .engine
        .create_label(NewLabelDto {
            name: "Vip".into(),
            color: None,
        })
        .await
        .unwrap();
    let mut vip = op(
        "rb-vip",
        OpKind::LabelAdd,
        thread_of(&running, &reply).await,
    );
    vip.label = Some("vip".into());
    running.engine.submit_op(vip).await.unwrap();
    eventually("label applied", APPLY, || async {
        running.engine.op("rb-vip".into()).await.unwrap().status == OpStatusDto::Done
    })
    .await;
    let labels_before = labels(&running).await;
    let partition_before = partition(&running).await;
    let epoch_before = running.cache.epoch();
    assert!(
        labels_before
            .values()
            .any(|l| l.contains("vip") && l.contains("keep")),
        "{labels_before:?}"
    );
    running.stop().await;

    fixture.wipe_cache();
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    assert_ne!(running.cache.epoch(), epoch_before, "old cursors get 410");
    assert_eq!(labels(&running).await, labels_before);
    assert_eq!(partition(&running).await, partition_before);

    let doomed = mailbox_id(&running, "Doomed").await;
    let t_archived = thread_of(&running, &archived).await;
    let t_starred = thread_of(&running, &starred).await;
    let t_orphan = thread_of(&running, &orphan).await;
    running.stop().await;

    let queued = fixture.open(no_override(), None).await;
    queued
        .engine
        .submit_op(op("rb-arch", OpKind::Archive, t_archived))
        .await
        .unwrap();
    queued
        .engine
        .submit_op(op("rb-star", OpKind::Star, t_starred))
        .await
        .unwrap();
    let mut doomed_move = op("rb-move", OpKind::Move, t_orphan);
    doomed_move.mailbox = Some(OpMailbox::Id(MailboxRowId(doomed)));
    queued.engine.submit_op(doomed_move).await.unwrap();
    let job = queued
        .engine
        .update_label(
            "keep".into(),
            LabelPatchDto {
                name: Some("Kept".into()),
                color: None,
            },
        )
        .await
        .unwrap()
        .job_ids[0];
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let root_uid: UidSet = [uid_of(user, "INBOX", &root).await].into_iter().collect();
    s.store_flags(
        &root_uid,
        StoreOp::Add,
        &[Flag::Keyword("$label_Kept".into())],
        None,
    )
    .await
    .unwrap();
    s.store_flags(
        &root_uid,
        StoreOp::Remove,
        &[Flag::Keyword("$label_Keep".into())],
        None,
    )
    .await
    .unwrap();
    queued
        .config
        .write(move |tx| {
            tx.execute(
                "UPDATE label_job SET progress = '[\"INBOX\"]' WHERE id = ?1",
                [job],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    Raw::login(user).await.cmd("DELETE Doomed").await;
    queued.stop().await;

    fixture.wipe_cache();
    let running = fixture.start(no_override(), None).await;
    eventually("queued ops settle", APPLY, || async {
        let ops = running.engine.ops(None).await.unwrap();
        ops.iter().all(|o| o.status == OpStatusDto::Failed)
            && running.engine.label_jobs().await.unwrap().is_empty()
    })
    .await;
    let failed = running.engine.ops(Some(OpStatusDto::Failed)).await.unwrap();
    assert_eq!(
        failed.iter().map(|o| o.op_id.as_str()).collect::<Vec<_>>(),
        ["rb-move"],
        "only the op without a destination fails"
    );
    assert!(
        failed[0]
            .error
            .as_deref()
            .unwrap()
            .contains("mailbox not found")
    );
    for id in ["rb-arch", "rb-star"] {
        assert_eq!(
            running.engine.op(id.into()).await.unwrap().status,
            OpStatusDto::Done
        );
    }

    let inbox = server_state(user, "INBOX").await;
    let archive = server_state(user, "Archive").await;
    assert!(!present(&inbox, &archived) && present(&archive, &archived));
    assert!(has(&inbox, &starred, "\\Flagged"));
    assert!(present(&inbox, &orphan), "the failed move left it in place");
    for state in [&inbox, &archive] {
        assert!(has(state, &root, "$label_Kept"), "{state:?}");
        assert!(
            !state
                .iter()
                .any(|l| l.contains("$label_Keep ") || l.contains("$label_Keep)"))
        );
    }
    running.stop().await;
}
