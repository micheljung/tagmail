//! Labels and ops against the Dovecot test servers (AC 5, Gmail-mode
//! archive, rename job across a restart). Each test owns an `opsN` user;
//! see `dovecot.rs` for how to run them. With `TAGMAIL_IMAP_SERVER=stalwart`
//! and `TAGMAIL_IMAP_ADDR` at Stalwart they run there too, except Gmail
//! mode, which only the Dovecot variant has (docs/servers.md).
#![cfg(feature = "dovecot-tests")]

mod common;

use std::time::Duration;

use common::*;
use tagmail_api_core::compose::{Compose, DraftInput};
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::ops_dto::{LabelPatchDto, NewLabelDto, OpStatusDto};
use tagmail_api_core::{OpRequest, Submitted, WriteService};
use tagmail_core::ThreadId;
use tagmail_core::ops::OpKind;
use tagmail_imap::*;

/// The engine's label registry annotation.
const REGISTRY_ENTRY: &str = "/private/vendor/tagmail/labels";
/// Budget for an op or job to reach the server.
const APPLY: Duration = Duration::from_secs(20);

fn op(id: &str, kind: OpKind, thread: ThreadId, label: Option<&str>) -> OpRequest {
    OpRequest {
        op_id: id.to_owned(),
        kind,
        thread_ids: vec![thread],
        email_ids: Vec::new(),
        label: label.map(str::to_owned),
        mailbox: None,
    }
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

/// `FLAGS` of every message in `mailbox` as a fresh IMAP session sees them.
async fn raw_flags(server: Server, user: &str, mailbox: &str) -> Vec<String> {
    let mut raw = Raw::login_at(server, user).await;
    raw.cmd(&format!("SELECT \"{mailbox}\"")).await;
    let lines = raw.cmd("UID FETCH 1:* (FLAGS)").await;
    raw.cmd("LOGOUT").await;
    lines.into_iter().filter(|l| l.contains("FETCH")).collect()
}

async fn count(server: Server, user: &str, mailbox: &str) -> usize {
    let mut raw = Raw::login_at(server, user).await;
    raw.cmd(&format!("SELECT \"{mailbox}\"")).await;
    let lines = raw.cmd("UID SEARCH ALL").await;
    raw.cmd("LOGOUT").await;
    lines
        .iter()
        .find_map(|l| l.strip_prefix("* SEARCH"))
        .map_or(0, |rest| rest.split_whitespace().count())
}

async fn status(running: &Running, id: &str) -> OpStatusDto {
    running.engine.op(id.to_owned()).await.unwrap().status
}

#[tokio::test]
async fn label_reaches_every_copy_as_a_keyword_for_other_clients() {
    let user = "ops1";
    let fixture = Fixture::new(user).await;
    let mut s = second(user).await;
    let id = format!("ac5-{}@it", nanos());
    s.append("INBOX", &[], &message(&id, "Labelled", &[]))
        .await
        .unwrap();
    s.append(
        "Archive",
        &[Flag::Seen, Flag::Keyword("$label_Other".into())],
        &message(&id, "Labelled", &[]),
    )
    .await
    .unwrap();

    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    running
        .engine
        .create_label(NewLabelDto {
            name: "Work".into(),
            color: Some("#336699".into()),
        })
        .await
        .unwrap();
    let thread = thread_of(&running, &id).await;
    match running
        .engine
        .submit_op(op("ac5-add", OpKind::LabelAdd, thread, Some("work")))
        .await
        .unwrap()
    {
        Submitted::Created(_) => {}
        Submitted::Existing(o) => panic!("{o:?}"),
    }
    eventually("keyword on both copies", APPLY, || async {
        let inbox = raw_flags(Server::Dovecot, user, "INBOX").await;
        let archive = raw_flags(Server::Dovecot, user, "Archive").await;
        inbox.iter().all(|l| l.contains("$label_Work"))
            && archive
                .iter()
                .all(|l| l.contains("$label_Work") && l.contains("$label_Other"))
            && !inbox.is_empty()
    })
    .await;
    eventually("op done", APPLY, || async {
        status(&running, "ac5-add").await == OpStatusDto::Done
    })
    .await;

    running
        .engine
        .submit_op(op("ac5-remove", OpKind::LabelRemove, thread, Some("other")))
        .await
        .unwrap();
    running
        .engine
        .submit_op(op("ac5-star", OpKind::Star, thread, None))
        .await
        .unwrap();
    eventually(
        "removed from the divergent copy, starred everywhere",
        APPLY,
        || async {
            let inbox = raw_flags(Server::Dovecot, user, "INBOX").await;
            let archive = raw_flags(Server::Dovecot, user, "Archive").await;
            archive
                .iter()
                .all(|l| !l.contains("$label_Other") && l.contains("\\Flagged"))
                && inbox.iter().all(|l| l.contains("\\Flagged"))
        },
    )
    .await;

    if stalwart() {
        assert!(
            !second(user).await.capabilities().metadata(),
            "without METADATA the registry stays local"
        );
    } else {
        let mut raw = Raw::login(user).await;
        let registry = raw
            .cmd("GETMETADATA \"\" /private/vendor/tagmail/labels")
            .await
            .join("\n");
        assert!(registry.contains("\"name\":\"Work\""), "{registry}");
    }

    running
        .engine
        .update_label(
            "work".into(),
            LabelPatchDto {
                name: Some("Job".into()),
                color: None,
            },
        )
        .await
        .unwrap();
    running.stop().await;

    let running = fixture.start(no_override(), None).await;
    eventually("rename finished after the restart", APPLY * 2, || async {
        running.engine.label_jobs().await.unwrap().is_empty()
    })
    .await;
    for mailbox in ["INBOX", "Archive"] {
        let flags = raw_flags(Server::Dovecot, user, mailbox).await;
        assert!(
            flags
                .iter()
                .all(|l| l.contains("$label_Job") && !l.contains("$label_Work")),
            "{mailbox}: {flags:?}"
        );
    }
    running.stop().await;
}

#[tokio::test]
async fn gmail_mode_archive_only_leaves_inbox() {
    if skipped_on_stalwart(
        "gmail_mode_archive_only_leaves_inbox",
        "a Gmail-mode server",
    ) {
        return;
    }
    let user = "ops2";
    let fixture = Fixture::on(Server::Gmail, user).await;
    let mut s = second_at(Server::Gmail, user, PASSWORD).await;
    let id = format!("gm-{}@it", nanos());
    s.append("INBOX", &[], &message(&id, "Gmail mode", &[]))
        .await
        .unwrap();
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    let thread = thread_of(&running, &id).await;

    running
        .engine
        .submit_op(op("gm-archive", OpKind::Archive, thread, None))
        .await
        .unwrap();
    eventually("out of INBOX, still in \\All", APPLY, || async {
        count(Server::Gmail, user, "INBOX").await == 0
            && count(Server::Gmail, user, "[Gmail]/All").await == 1
    })
    .await;
    eventually("op done", APPLY, || async {
        status(&running, "gm-archive").await == OpStatusDto::Done
    })
    .await;

    running
        .engine
        .submit_op(op("gm-unarchive", OpKind::Unarchive, thread, None))
        .await
        .unwrap();
    eventually("back in INBOX", APPLY, || async {
        count(Server::Gmail, user, "INBOX").await == 1
    })
    .await;
    running.stop().await;
}

/// Empties `user` on any server: deletes its own mailboxes, expunges the
/// rest and drops the label registry annotation where there is METADATA.
async fn reset_anywhere(user: &str) {
    let mut s = second(user).await;
    for mailbox in s.list().await.unwrap() {
        if mailbox.special_use.is_none() && !mailbox.name.eq_ignore_ascii_case("INBOX") {
            s.delete_mailbox(&mailbox.name).await.unwrap();
            continue;
        }
        s.select(&mailbox.name, SelectOptions::default())
            .await
            .unwrap();
        let all = s.uid_search(SearchCriteria::All).await.unwrap();
        if !all.is_empty() {
            s.store_flags(&all, StoreOp::Add, &[Flag::Deleted], None)
                .await
                .unwrap();
            s.expunge_uids(&all).await.unwrap();
        }
    }
    if s.capabilities().metadata() {
        s.set_metadata(REGISTRY_ENTRY, None).await.unwrap();
    }
    s.logout().await.unwrap();
}

async fn drafts_mailbox(user: &str) -> String {
    let mut s = second(user).await;
    s.list()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.special_use == Some(SpecialUse::Drafts))
        .expect("a \\Drafts mailbox")
        .name
}

/// Engine smoke test that also runs against Stalwart: a label round trip
/// as keywords, and a draft saved without UIDPLUS into a Drafts mailbox
/// holding other messages, where Stalwart's `UID SEARCH HEADER` finds
/// nothing, still records the draft's UID.
#[tokio::test]
async fn label_round_trip_and_draft_uid_without_uidplus() {
    const USER: &str = "ops4";
    const KEYWORD: &str = "$label_Smoke";
    reset_anywhere(USER).await;
    let fixture = Fixture::existing(USER, PASSWORD);
    let drafts = drafts_mailbox(USER).await;
    let mut s = second(USER).await;
    let id = format!("smoke-{}@it", nanos());
    s.append("INBOX", &[], &message(&id, "Smoke", &[]))
        .await
        .unwrap();
    for n in 0..2 {
        let other = format!("other-draft-{n}-{}@it", nanos());
        s.append(
            &drafts,
            &[Flag::Draft],
            &message(&other, "Other draft", &[]),
        )
        .await
        .unwrap();
    }

    let running = fixture
        .start(
            CapabilityOverride {
                remove: vec!["UIDPLUS".into()],
                add: Vec::new(),
            },
            None,
        )
        .await;
    running.wait_live().await;
    running
        .engine
        .create_label(NewLabelDto {
            name: "Smoke".into(),
            color: None,
        })
        .await
        .unwrap();
    let thread = thread_of(&running, &id).await;
    running
        .engine
        .submit_op(op("smoke-add", OpKind::LabelAdd, thread, Some("smoke")))
        .await
        .unwrap();
    eventually("label added", APPLY, || async {
        status(&running, "smoke-add").await == OpStatusDto::Done
    })
    .await;
    let flags = raw_flags(Server::Dovecot, USER, "INBOX").await;
    assert!(flags.iter().all(|l| l.contains(KEYWORD)), "{flags:?}");
    running
        .engine
        .submit_op(op(
            "smoke-remove",
            OpKind::LabelRemove,
            thread,
            Some("smoke"),
        ))
        .await
        .unwrap();
    eventually("label removed", APPLY, || async {
        status(&running, "smoke-remove").await == OpStatusDto::Done
    })
    .await;
    let flags = raw_flags(Server::Dovecot, USER, "INBOX").await;
    assert!(flags.iter().all(|l| !l.contains(KEYWORD)), "{flags:?}");

    let identity = running.engine.identities().await.unwrap()[0].id;
    let created = running
        .engine
        .create_draft(DraftInput {
            identity_id: identity,
            to: vec![AddressDto {
                name: None,
                email: "ann@example.com".into(),
            }],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Smoke draft".into(),
            text: Some("Draft body\n".into()),
            html: None,
            attachment_ids: Vec::new(),
            reply: None,
            forward_of: None,
            save_now: true,
            client_id: None,
            forwarded: Vec::new(),
        })
        .await
        .unwrap()
        .draft;
    eventually("draft saved", APPLY, || async {
        running.engine.draft(created.id).await.unwrap().saved
    })
    .await;
    let draft = created.id;
    let (msgid, mailbox, uid): (String, String, u32) = running
        .config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT msgid_hdr, saved_mailbox, saved_uid FROM draft WHERE id = ?1",
                [draft],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mailbox, drafts);
    let mut s = second(USER).await;
    s.select(&drafts, SelectOptions::default()).await.unwrap();
    let mut saved = Vec::new();
    s.fetch_body(Uid(uid), "HEADER", &mut saved, 1 << 16)
        .await
        .unwrap();
    let saved = String::from_utf8_lossy(&saved);
    assert!(
        saved.contains(&msgid),
        "UID {uid} is not the draft: {saved}"
    );
    running.stop().await;
}
