//! AC 13 against Dovecot and Mailpit (`cargo xtask dovecot up`): two
//! accounts (`engine8`, `engine9`) in one engine. A label op from the
//! unified view on a thread of the second account reaches that account's
//! server, and a reply to mail delivered to the second account's alias goes
//! out from the alias with its signature. Run with
//! `cargo test -p tagmail-engine --features mailpit-tests --test multi_account`.
#![cfg(feature = "mailpit-tests")]

mod common;

use std::time::Duration;

use common::*;
use serde_json::Value;
use tagmail_api_core::MessageRecord;
use tagmail_api_core::OpRequest;
use tagmail_api_core::WriteService;
use tagmail_api_core::compose::{Compose, DraftInput, Original, ReplyRefs, reply_fields};
use tagmail_api_core::compose_dto::ReplyMode;
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::identities::{NewIdentity, reply_headers};
use tagmail_api_core::ops_dto::{NewLabelDto, OpStatusDto};
use tagmail_core::ops::OpKind;
use tagmail_core::{AccountId, MessageRowId, ThreadId};
use tagmail_imap::ImapSession;

const FIRST: &str = "engine8";
const SECOND: &str = "engine9";
const SECOND_ID: i64 = 2;
const BUDGET: Duration = Duration::from_secs(30);
/// Both tests own `engine8` and `engine9`.
static USERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn addresses(json: Option<String>) -> Vec<AddressDto> {
    let list = match json.map(|j| serde_json::from_str::<Value>(&j)) {
        Some(Ok(Value::Array(list))) => list,
        _ => Vec::new(),
    };
    list.iter()
        .filter_map(|a| {
            Some(AddressDto {
                name: a["name"].as_str().map(str::to_owned),
                email: a["email"].as_str()?.to_owned(),
            })
        })
        .collect()
}

/// The cached message, as the API's store reads it.
async fn record(running: &Running, msgid: &str) -> MessageRecord {
    let rows = running
        .query(
            "SELECT msgid_hdr, id, account_id, thread_id, date, subject, \
             from_json, to_json, cc_json FROM message",
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .await;
    let (_, id, account, thread, date, subject, from, to, cc) = rows
        .into_iter()
        .find(|r| r.0.as_deref() == Some(msgid))
        .expect("message cached");
    MessageRecord {
        id: MessageRowId(id),
        account_id: AccountId(account),
        thread_id: ThreadId(thread),
        date,
        subject,
        snippet: String::new(),
        from: addresses(from),
        to: addresses(to),
        cc: addresses(cc),
        unread: true,
        starred: false,
        has_attachment: false,
    }
}

async fn has_message(running: &Running, msgid: &str) -> bool {
    !running.locations(msgid).await.is_empty()
}

/// Mail sent to a list, delivered to `delivered_to`, copied to `cc`.
fn mail(msgid: &str, delivered_to: &str, cc: &str) -> Vec<u8> {
    format!(
        "Delivered-To: {delivered_to}\r\nFrom: Ann <ann@example.com>\r\n\
         To: friends@lists.example.org\r\nCc: {cc}\r\nSubject: Dinner\r\n\
         Message-ID: <{msgid}>\r\nDate: Sun, 22 Nov 2026 10:00:00 +0000\r\n\r\n\
         Dinner on Friday?\r\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn label_from_the_unified_view_reaches_the_threads_account() {
    let _users = USERS.lock().await;
    let fixture = Fixture::new(FIRST).await.with_account(SECOND).await;
    let id = format!("multi-label-{}@it", nanos());
    second(SECOND)
        .await
        .append("INBOX", &[], &message(&id, "Second account", &[]))
        .await
        .unwrap();
    let running = fixture.start(no_override(), None).await;
    running.wait_live().await;
    eventually("message cached", BUDGET, || has_message(&running, &id)).await;
    running
        .engine
        .create_label(NewLabelDto {
            name: "Shared".into(),
            color: None,
        })
        .await
        .unwrap();
    running
        .config
        .write(|tx| {
            tx.execute(
                "DELETE FROM label_registry WHERE account_id = ?1",
                [SECOND_ID],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let cached = record(&running, &id).await;
    assert_eq!(cached.account_id, AccountId(SECOND_ID));
    let thread = cached.thread_id;
    running
        .engine
        .submit_op(OpRequest {
            op_id: "multi-label".into(),
            kind: OpKind::LabelAdd,
            thread_ids: vec![thread],
            email_ids: Vec::new(),
            label: Some("shared".into()),
            mailbox: None,
        })
        .await
        .unwrap();
    eventually("op done", BUDGET, || async {
        running
            .engine
            .op("multi-label".into())
            .await
            .unwrap()
            .status
            == OpStatusDto::Done
    })
    .await;

    let mut raw = Raw::login(SECOND).await;
    raw.cmd("SELECT INBOX").await;
    let flags = raw.cmd("UID FETCH 1:* (FLAGS)").await;
    assert!(
        flags.iter().any(|l| l.contains("$label_Shared")),
        "{flags:?}"
    );
    let registered = running
        .config
        .read(|c| {
            Ok(c.query_row(
                "SELECT display_name FROM label_registry \
                 WHERE account_id = ?1 AND label_key = 'shared'",
                [SECOND_ID],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(registered, "Shared");
    let mut raw = Raw::login(FIRST).await;
    raw.cmd("SELECT INBOX").await;
    let first = raw.cmd("UID FETCH 1:* (FLAGS)").await;
    assert!(
        first.iter().all(|l| !l.contains("$label_Shared")),
        "{first:?}"
    );
    running.stop().await;
}

#[tokio::test]
async fn reply_to_an_alias_goes_out_from_the_alias_with_its_signature() {
    let _users = USERS.lock().await;
    const SIGNATURE: &str = "<p>Nine <b>of Nine</b></p>";
    let alias = format!("alias-{SECOND}@tagmail.test");
    let fixture = Fixture::new(FIRST).await.with_account(SECOND).await;
    let run_id = nanos();
    let original = format!("multi-reply-{run_id}@example.com");
    let raw = mail(&original, &alias, &email_of(FIRST));
    second(SECOND)
        .await
        .append("INBOX", &[], &raw)
        .await
        .unwrap();
    let running = fixture
        .start_sending(Sending {
            server_saves_sent: Some(false),
        })
        .await;
    running.wait_live().await;
    eventually("message cached", BUDGET, || {
        has_message(&running, &original)
    })
    .await;
    running
        .engine
        .create_identity(NewIdentity {
            account_id: AccountId(SECOND_ID),
            address: alias.clone(),
            name: Some("Nine".into()),
            signature: Some(SIGNATURE.into()),
            is_default: false,
        })
        .await
        .unwrap();

    let parsed = mail_parser::MessageParser::default()
        .parse(raw.as_slice())
        .expect("mail");
    let identities = running.engine.identities().await.unwrap();
    let reply = reply_fields(
        &Original {
            record: record(&running, &original).await,
            headers: reply_headers(&parsed),
            reply_to: Vec::new(),
            text: "Dinner on Friday?".into(),
        },
        ReplyMode::Reply,
        &identities,
    );
    let identity = identities
        .iter()
        .find(|i| Some(i.id) == reply.identity_id)
        .expect("reply identity");
    assert_eq!(identity.address, alias);
    assert_eq!(identity.account_id, SECOND_ID);
    let signature = identity.signature.clone().expect("signature");
    let created = running
        .engine
        .create_draft(DraftInput {
            identity_id: identity.id,
            to: reply.to,
            cc: reply.cc,
            bcc: Vec::new(),
            subject: reply.subject,
            text: None,
            html: Some(format!("<p>Friday works.</p>{signature}{}", reply.html)),
            attachment_ids: Vec::new(),
            reply: Some(ReplyRefs {
                in_reply_to: original.clone(),
                references: vec![original.clone()],
            }),
            forward_of: None,
            save_now: false,
            client_id: None,
            forwarded: Vec::new(),
        })
        .await
        .unwrap()
        .draft;
    let msgid: String = running
        .config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT msgid_hdr FROM draft WHERE id = ?1",
                [created.id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    running
        .engine
        .send(format!("multi-reply-{run_id}"), created.id)
        .await
        .unwrap();

    eventually("Mailpit received the reply", BUDGET, || async {
        mailpit_id(&msgid).await.is_some()
    })
    .await;
    let id = mailpit_id(&msgid).await.expect("Mailpit id");
    let summary: Value =
        serde_json::from_slice(&mailpit(&format!("/api/v1/message/{id}")).await).unwrap();
    assert_eq!(summary["From"]["Address"], alias.as_str(), "{summary}");
    let html = summary["HTML"].as_str().unwrap_or_default();
    assert!(html.contains("<b>of Nine</b>"), "{html}");
    running.stop().await;
}
