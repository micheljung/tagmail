//! AC 8 and 14 against Dovecot and Mailpit (`cargo xtask dovecot up`):
//! drafts in IMAP Drafts, SMTP submission, the Sent copy in both
//! `server_saves_sent` modes and the restart check. Run with
//! `cargo test -p tagmail-engine --features mailpit-tests --test send`;
//! `TAGMAIL_SMTP_ADDR` (default `127.0.0.1:10025`) and
//! `TAGMAIL_MAILPIT_API` (default `127.0.0.1:10825`) point elsewhere.
#![cfg(feature = "mailpit-tests")]

mod common;

use std::time::Duration;

use common::*;
use serde_json::Value;
use tagmail_api_core::compose::{Compose, DraftInput, ReplyRefs};
use tagmail_api_core::dto::AddressDto;
use tagmail_imap::*;
use tagmail_store::rusqlite::params;

const SEND_BUDGET: Duration = Duration::from_secs(30);

fn addr(email: &str) -> AddressDto {
    AddressDto {
        name: None,
        email: email.to_owned(),
    }
}

fn draft(identity: i64, subject: &str, reply: Option<&str>) -> DraftInput {
    DraftInput {
        identity_id: identity,
        to: vec![addr("ann@example.com")],
        cc: vec![],
        bcc: vec![addr("hidden@example.org")],
        subject: subject.to_owned(),
        text: Some("See you then.\n".to_owned()),
        html: Some("<p>See you <b>then</b>.</p><script>x()</script>".to_owned()),
        attachment_ids: vec![],
        reply: reply.map(|id| ReplyRefs {
            in_reply_to: id.to_owned(),
            references: vec![id.to_owned()],
        }),
        forward_of: None,
        save_now: false,
        forwarded: vec![],
    }
}

/// UIDs of the message in `mailbox`, seen by a second client.
async fn uids(user: &str, mailbox: &str, msgid: &str) -> Vec<u32> {
    let mut s = second(user).await;
    s.select(mailbox, SelectOptions::default()).await.unwrap();
    let found = s
        .uid_search(SearchCriteria::Header {
            name: "Message-ID".into(),
            value: format!("<{msgid}>"),
        })
        .await
        .unwrap();
    found.iter().map(|u| u.0).collect()
}

async fn flags(user: &str, mailbox: &str, msgid: &str) -> Vec<Flag> {
    let found = uids(user, mailbox, msgid).await;
    let mut s = second(user).await;
    s.select(mailbox, SelectOptions::default()).await.unwrap();
    let set: UidSet = found.into_iter().map(Uid).collect();
    s.fetch(&set, &FetchSpec::flags())
        .await
        .unwrap()
        .into_iter()
        .flat_map(|m| m.flags.unwrap_or_default())
        .collect()
}

async fn raw(user: &str, mailbox: &str, uid: u32) -> Vec<u8> {
    let mut s = second(user).await;
    s.select(mailbox, SelectOptions::default()).await.unwrap();
    let mut out = Vec::new();
    s.fetch_body(Uid(uid), "", &mut out, 1 << 20).await.unwrap();
    out
}

/// Every outbox item is sent and in Sent.
async fn settled(run: &Running) -> bool {
    run.config
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM outbox WHERE copied_at IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap()
        == 0
}

async fn draft_msgid(run: &Running, id: i64) -> String {
    run.config
        .read(move |c| {
            Ok(
                c.query_row("SELECT msgid_hdr FROM draft WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap()
}

async fn saves_sent(run: &Running) -> Option<bool> {
    run.config
        .read(|c| Ok(c.query_row("SELECT server_saves_sent FROM account", [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn drafts_replace_and_send_threads_with_the_conversation() {
    const USER: &str = "engine8";
    let fixture = Fixture::new(USER).await;
    let run_id = nanos();
    let original = format!("orig-{run_id}@example.com");
    second(USER)
        .await
        .append("INBOX", &[], &message(&original, "Lunch", &[]))
        .await
        .unwrap();
    let run = fixture
        .start_sending(Sending {
            server_saves_sent: Some(false),
        })
        .await;
    run.wait_live().await;
    let identity = run.engine.identities().await.unwrap()[0].id;

    let created = run
        .engine
        .create_draft(draft(identity, "Re: Lunch (draft)", Some(&original)))
        .await
        .unwrap();
    eventually("first draft version saved", SEND_BUDGET, || async {
        run.engine.draft(created.id).await.unwrap().saved
    })
    .await;
    let msgid = draft_msgid(&run, created.id).await;
    let first = uids(USER, "Drafts", &msgid).await;
    assert_eq!(first.len(), 1);

    run.engine
        .update_draft(created.id, draft(identity, "Re: Lunch", None))
        .await
        .unwrap();
    eventually("second version replaces the first", SEND_BUDGET, || async {
        let now = uids(USER, "Drafts", &msgid).await;
        now.len() == 1 && now != first
    })
    .await;
    let drafted = raw(USER, "Drafts", uids(USER, "Drafts", &msgid).await[0]).await;
    let drafted = String::from_utf8_lossy(&drafted);
    assert!(drafted.contains("Subject: Re: Lunch\r\n"), "{drafted}");
    assert!(drafted.contains("hidden@example.org"), "drafts keep Bcc");
    assert!(
        drafted.contains(&format!("In-Reply-To: <{original}>")),
        "{drafted}"
    );

    let out = run
        .engine
        .send(format!("op-{run_id}"), created.id)
        .await
        .unwrap();
    assert!(out.created);
    eventually("sent and copied", SEND_BUDGET, || settled(&run)).await;

    let id = mailpit_id(&msgid)
        .await
        .expect("Mailpit received the message");
    let received = mailpit(&format!("/api/v1/message/{id}/raw")).await;
    let sent = uids(USER, "Sent", &msgid).await;
    assert_eq!(sent.len(), 1, "exactly one Sent copy");
    let copy = raw(USER, "Sent", sent[0]).await;
    assert!(
        received.ends_with(&copy),
        "Mailpit got the exact MIME that went to Sent"
    );
    let text = String::from_utf8_lossy(&copy);
    assert!(!text.to_ascii_lowercase().contains("bcc:"), "{text}");
    assert!(!text.contains("hidden@example.org"), "{text}");
    assert!(!text.contains("<script"), "{text}");
    assert!(
        text.contains(&format!("References: <{original}>")),
        "{text}"
    );
    let summary: Value =
        serde_json::from_slice(&mailpit(&format!("/api/v1/message/{id}")).await).unwrap();
    assert_eq!(
        summary["Bcc"][0]["Address"], "hidden@example.org",
        "{summary}"
    );

    eventually("draft gone from Drafts", SEND_BUDGET, || async {
        uids(USER, "Drafts", &msgid).await.is_empty()
    })
    .await;
    eventually(
        "Sent copy threads with the original",
        SEND_BUDGET,
        || async {
            let rows = run
                .query("SELECT msgid_hdr, thread_id FROM message", |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
                })
                .await;
            let thread = |id: &str| {
                rows.iter()
                    .find(|(m, _)| m.as_deref() == Some(id))
                    .map(|(_, t)| *t)
            };
            thread(&msgid).is_some() && thread(&msgid) == thread(&original)
        },
    )
    .await;
    eventually(
        "the original is flagged \\Answered",
        SEND_BUDGET,
        || async {
            flags(USER, "INBOX", &original)
                .await
                .contains(&Flag::Answered)
        },
    )
    .await;
    eventually(
        "the cache holds only the Sent copy",
        SEND_BUDGET,
        || async {
            let locations = run.locations(&msgid).await;
            locations.len() == 1 && locations[0].0 == "Sent"
        },
    )
    .await;
    run.stop().await;
}

#[tokio::test]
async fn sent_copies_are_never_duplicated() {
    const USER: &str = "engine9";
    let run_id = nanos();

    // The server files Sent itself (Gmail preset): no APPEND.
    let fixture = Fixture::new(USER).await;
    let run = fixture
        .start_sending(Sending {
            server_saves_sent: Some(true),
        })
        .await;
    run.wait_live().await;
    let identity = run.engine.identities().await.unwrap()[0].id;
    let created = run
        .engine
        .create_draft(draft(identity, "Preset", None))
        .await
        .unwrap();
    let preset = draft_msgid(&run, created.id).await;
    run.engine
        .send(format!("p-{run_id}"), created.id)
        .await
        .unwrap();
    eventually("sent", SEND_BUDGET, || settled(&run)).await;
    assert!(mailpit_id(&preset).await.is_some());
    assert!(
        uids(USER, "Sent", &preset).await.is_empty(),
        "nothing APPENDed"
    );
    run.stop().await;

    // Unknown: probed, Mailpit does not file Sent, so one APPEND.
    let fixture = Fixture::new(USER).await;
    let run = fixture.start_sending(Sending::default()).await;
    run.wait_live().await;
    let identity = run.engine.identities().await.unwrap()[0].id;
    let created = run
        .engine
        .create_draft(draft(identity, "Probe", None))
        .await
        .unwrap();
    run.engine
        .send(format!("q-{run_id}"), created.id)
        .await
        .unwrap();
    eventually("sent after the probe", SEND_BUDGET, || settled(&run)).await;
    assert_eq!(saves_sent(&run).await, Some(false));

    // A crash between DATA and the commit: Sent already has the message,
    // so the restarted outbox does not submit it again.
    let crashed = format!("crash-{run_id}@example.com");
    let mime = message(&crashed, "Crashed", &[]);
    second(USER)
        .await
        .append("Sent", &[Flag::Seen], &mime)
        .await
        .unwrap();
    let (op, id) = (format!("c-{run_id}"), crashed.clone());
    let file = format!("{op}.eml");
    std::fs::write(fixture.outbox_dir().join(&file), &mime).unwrap();
    let size = i64::try_from(mime.len()).unwrap();
    run.config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO outbox (op_id, account_id, mime_file, mime_size, msgid_hdr, \
                 sender, recipients, subject, state, attempts) \
                 VALUES (?1, 1, ?2, ?3, ?4, 'ann@example.com', \
                 '[{\"email\":\"me@example.com\"}]', 'Crashed', 'sending', 1)",
                params![op, file, size, id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    run.stop().await;
    let run = fixture.start_sending(Sending::default()).await;
    eventually("recovered after restart", SEND_BUDGET, || settled(&run)).await;
    assert_eq!(mailpit_id(&crashed).await, None, "not submitted again");
    assert_eq!(uids(USER, "Sent", &crashed).await.len(), 1, "no duplicate");
    run.stop().await;
}
