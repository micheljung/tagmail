//! Scheduling against Stalwart (plan §5 AC 5 and 7), which runs RFC 6638
//! scheduling for its own users: users `calN` (attendee) and `calN+1`
//! (organizer). Stalwart delivers an organizer's invitation as an iMIP
//! mail into the attendee's INBOX but never sends the attendee's REPLY
//! itself, so tagmail's REPLY is the only one: it goes to Mailpit (the test
//! account's SMTP), and none reaches the organizer's INBOX from Stalwart.
//! Run with `TAGMAIL_IMAP_SERVER=stalwart TAGMAIL_IMAP_ADDR=127.0.0.1:10145
//! TAGMAIL_CALDAV_ADDR=127.0.0.1:10080`.
#![cfg(feature = "caldav-tests")]

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::cal_fixture::{CalFixture, dav, home_path};
use common::cal_flow::{CalAccount, Invite, QUIET, default_href, exactly_replies};
use common::{Raw, email_of};
use serde_json::json;
use tagmail_api_core::mime::reduce::calendar_parts;
use tagmail_api_core::ops_dto::OpStatusDto;
use tagmail_imap::ImapSession;

const ICS: &str = "text/calendar; charset=utf-8";
const SCHEDULE_DELIVERY: std::time::Duration = std::time::Duration::from_secs(15);

fn invite<'a>(uid: &'a str, organizer: &'a str, me: &'a str) -> Invite<'a> {
    Invite {
        uid,
        method: "REQUEST",
        sequence: 0,
        dtstamp: "20261101T120000Z",
        summary: "Review",
        organizer,
        me,
        partstat: "NEEDS-ACTION",
        headers: "",
        from: organizer,
    }
}

async fn put(user: &str, path: &str, ics: &str) -> String {
    let resp = dav(user, "PUT", path, &[("Content-Type", ICS)], ics).await;
    assert!(resp.status < 300, "PUT {path}: {}", resp.head);
    resp.head
}

async fn etag(user: &str, path: &str) -> Option<String> {
    let resp = dav(user, "GET", path, &[], "").await;
    (resp.status == 200).then(|| {
        resp.head
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("etag"))
                    .map(|(_, v)| v.trim().to_owned())
            })
            .unwrap_or_default()
    })
}

/// Messages in `user`'s INBOX with a decoded iTIP REPLY part about `uid`:
/// Stalwart's SEARCH matches every message it has not indexed yet.
async fn inbox_replies(user: &str, uid: &str) -> usize {
    let mut raw = Raw::login(user).await;
    let exists = raw
        .cmd("SELECT INBOX")
        .await
        .iter()
        .find_map(|l| {
            l.strip_prefix("* ")?
                .strip_suffix(" EXISTS")?
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    let lines = if exists == 0 {
        Vec::new()
    } else {
        raw.cmd("FETCH 1:* (BODY.PEEK[])").await
    };
    raw.cmd("LOGOUT").await;
    let mut messages: Vec<String> = Vec::new();
    for line in lines {
        if line.starts_with("* ") && line.contains(" FETCH (") {
            messages.push(String::new());
        } else if let Some(m) = messages.last_mut() {
            m.push_str(&line);
            m.push_str("\r\n");
        }
    }
    messages
        .iter()
        .filter(|m| {
            calendar_parts(m.as_bytes()).is_ok_and(|c| {
                c.parts.iter().any(|p| {
                    let text = p.text.replace("\r\n ", "").replace("\n ", "");
                    text.contains("METHOD:REPLY") && text.contains(&format!("UID:{uid}"))
                })
            })
        })
        .count()
}

#[tokio::test]
async fn stalwart_scheduling_delivers_exactly_one_reply() {
    let test = "stalwart_scheduling_delivers_exactly_one_reply";
    let cal = CalFixture::for_test(test).await;
    let organizer = cal.organizer.clone().expect("Stalwart pair");
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (email_of(&organizer), account.email());
    let uid = common::cal_flow::unique_uid(test);
    put(
        &organizer,
        &format!("{}default/{uid}.ics", home_path(&organizer)),
        &invite(&uid, &org, &me).stored(),
    )
    .await;
    let message = account.received(|s| s.starts_with("Invitation:")).await;
    account
        .submit(
            "rsvp",
            json!({ "opId": "w-1", "messageId": message.to_string(), "partstat": "accepted" }),
        )
        .await;
    let done = account.settled("w-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    exactly_replies(&org, &uid, 1).await;
    let control = common::cal_flow::unique_uid("control");
    let known_reply = Invite {
        method: "REPLY",
        partstat: "ACCEPTED",
        ..invite(&control, &org, &me)
    }
    .mail("Control");
    let (head, ics) = known_reply.split_once("\r\n\r\n").unwrap();
    let known_reply = format!(
        "{head}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
        STANDARD.encode(ics)
    );
    let mut s = common::second(&organizer).await;
    s.append("INBOX", &[], known_reply.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        inbox_replies(&organizer, &control).await,
        1,
        "a base64 REPLY is found"
    );
    tokio::time::sleep(SCHEDULE_DELIVERY).await;
    assert_eq!(
        inbox_replies(&organizer, &uid).await,
        0,
        "Stalwart sent no REPLY of its own"
    );
    account.stop().await;
}

#[tokio::test]
async fn never_ingested_invite_falls_back_to_outbox_reply() {
    let test = "never_ingested_invite_falls_back_to_outbox_reply";
    let cal = CalFixture::for_test(test).await;
    let organizer = cal.organizer.clone().expect("Stalwart pair");
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (email_of(&organizer), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Appended"), "Appended")
        .await;
    account
        .submit(
            "rsvp",
            json!({ "opId": "n-1", "messageId": message.to_string(), "partstat": "accepted" }),
        )
        .await;
    let done = account.settled("n-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    exactly_replies(&org, &uid, 1).await;
    account.stop().await;
}

#[tokio::test]
async fn server_mode_apply_already_applied_is_noop() {
    let test = "server_mode_apply_already_applied_is_noop";
    let cal = CalFixture::for_test(test).await;
    let organizer = cal.organizer.clone().expect("Stalwart pair");
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (email_of(&organizer), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let path = format!("{}{uid}.ics", default_href(cal.user));
    let mut stored = invite(&uid, &org, &me);
    stored.partstat = "ACCEPTED";
    put(cal.user, &path, &stored.stored()).await;
    account.cached(&uid).await;

    let update = Invite {
        sequence: 1,
        dtstamp: "20261110T120000Z",
        summary: "Moved",
        ..invite(&uid, &org, &me)
    };
    let message = account.deliver(&update.mail("Update"), "Update").await;
    let applied = Invite {
        partstat: "ACCEPTED",
        ..update
    };
    put(cal.user, &path, &applied.stored()).await;
    let before = etag(cal.user, &path).await.expect("stored");

    account
        .submit(
            "apply",
            json!({ "opId": "a-1", "messageId": message.to_string() }),
        )
        .await;
    let done = account.settled("a-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(
        etag(cal.user, &path).await.as_deref(),
        Some(before.as_str()),
        "nothing written"
    );
    tokio::time::sleep(QUIET).await;
    assert!(common::cal_flow::replies(&org, &uid).await.is_empty());
    account.stop().await;
}
