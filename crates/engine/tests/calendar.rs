//! The calendar against a real CalDAV server (Radicale in CI) with Dovecot
//! and Mailpit (plan §5): discovery from a manual URL with the IMAP login,
//! `sync-collection`, the etag fallback and a cache rebuild (AC 11, 12),
//! then invitations, RSVP, apply, import and the 412 policies.
#![cfg(feature = "caldav-tests")]

mod common;

use std::time::Duration;

use common::cal_fixture::{CalFixture, caldav_base, dav, home_path};
use common::{Fixture, PASSWORD, Running, eventually, no_override};
use tagmail_api_core::CalendarService;
use tagmail_api_core::calendar_dto::CaldavStatusDto;
use tagmail_core::AccountId;
use tagmail_dav::DiscoveryEnv;
use tagmail_store::calendar::{self, CaldavConfig};

const ACCOUNT: AccountId = AccountId(1);
const SYNC_BUDGET: Duration = Duration::from_secs(20);
const DEFAULT_CALENDAR: &str = "default";

fn ics(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:20261101T120000Z\r\nDTSTART:20261201T090000Z\r\n\
         DTEND:20261201T100000Z\r\nSUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn put_event(user: &str, uid: &str) {
    let path = format!("{}{DEFAULT_CALENDAR}/{uid}.ics", home_path(user));
    let resp = dav(
        user,
        "PUT",
        &path,
        &[("Content-Type", "text/calendar; charset=utf-8")],
        &ics(uid),
    )
    .await;
    assert!(resp.status < 300, "PUT {path}: {}", resp.head);
}

async fn delete_event(user: &str, uid: &str) {
    let path = format!("{}{DEFAULT_CALENDAR}/{uid}.ics", home_path(user));
    let resp = dav(user, "DELETE", &path, &[], "").await;
    assert!(resp.status < 300, "DELETE {path}: {}", resp.head);
}

/// An engine with the test user's account pointed at the CalDAV server
/// over plain http, discovered and syncing.
async fn syncing(fixture: &Fixture, env: DiscoveryEnv) -> Running {
    let running = fixture.open(no_override(), None).await;
    running
        .config
        .write(|tx| {
            if calendar::load_caldav(tx, ACCOUNT)?.is_none() {
                calendar::save_caldav(
                    tx,
                    ACCOUNT,
                    &CaldavConfig {
                        url: Some(format!("{}/", caldav_base())),
                        allow_insecure: true,
                        ..CaldavConfig::default()
                    },
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    running.engine.start_calendar_with(env).unwrap();
    let mut found = running.engine.discover(ACCOUNT).await.unwrap();
    if found.status == CaldavStatusDto::NeedsConfirmation {
        let host = found.host.clone().expect("a host to confirm");
        found = running.engine.confirm_host(ACCOUNT, host).await.unwrap();
    }
    assert_eq!(found.status, CaldavStatusDto::Ok, "{:?}", found.error);
    running
}

async fn hrefs_and_etags(running: &Running) -> Vec<(String, Option<String>)> {
    running
        .query("SELECT href, etag FROM cal_event ORDER BY href", |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .await
}

async fn uids(running: &Running) -> Vec<String> {
    running
        .query("SELECT uid FROM cal_event ORDER BY uid", |r| r.get(0))
        .await
}

async fn wait_for_uids(running: &Running, expected: &[&str]) {
    eventually("calendar sync", SYNC_BUDGET, || async {
        uids(running).await == expected
    })
    .await;
}

async fn sync_state(running: &Running) -> (Option<String>, Option<String>) {
    running
        .query(
            "SELECT ctag, sync_token FROM calendar WHERE href LIKE '%/default/'",
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .await
        .pop()
        .expect("the default calendar")
}

#[tokio::test]
async fn sync_collection_incremental() {
    let cal = CalFixture::for_test("sync_collection_incremental").await;
    put_event(cal.user, "a").await;
    put_event(cal.user, "b").await;
    let fixture = Fixture::existing(cal.user, PASSWORD);
    let running = syncing(&fixture, DiscoveryEnv::system()).await;
    wait_for_uids(&running, &["a", "b"]).await;
    assert!(
        sync_state(&running).await.1.is_some(),
        "a sync token is kept"
    );

    put_event(cal.user, "c").await;
    delete_event(cal.user, "a").await;
    running.engine.settings_changed(ACCOUNT);
    wait_for_uids(&running, &["b", "c"]).await;
    running.stop().await;
}

#[tokio::test]
async fn ctag_etag_fallback_when_sync_collection_disabled() {
    let cal = CalFixture::for_test("ctag_etag_fallback_when_sync_collection_disabled").await;
    put_event(cal.user, "a").await;
    let fixture = Fixture::existing(cal.user, PASSWORD);
    let env = DiscoveryEnv::system()
        .with_disabled_reports(&["sync-collection"])
        .with_disabled_properties(&["getctag", "sync-token"]);
    let running = syncing(&fixture, env).await;
    wait_for_uids(&running, &["a"]).await;
    assert_eq!(sync_state(&running).await, (None, None));

    put_event(cal.user, "b").await;
    delete_event(cal.user, "a").await;
    running.engine.settings_changed(ACCOUNT);
    wait_for_uids(&running, &["b"]).await;
    running.stop().await;
}

#[tokio::test]
async fn cache_wipe_rebuilds_identical_href_etag_set() {
    let cal = CalFixture::for_test("cache_wipe_rebuilds_identical_href_etag_set").await;
    for uid in ["a", "b", "c"] {
        put_event(cal.user, uid).await;
    }
    let fixture = Fixture::existing(cal.user, PASSWORD);
    let running = syncing(&fixture, DiscoveryEnv::system()).await;
    wait_for_uids(&running, &["a", "b", "c"]).await;
    let before = hrefs_and_etags(&running).await;
    running.stop().await;

    fixture.wipe_cache();
    let running = fixture.open(no_override(), None).await;
    running
        .engine
        .start_calendar_with(DiscoveryEnv::system())
        .unwrap();
    wait_for_uids(&running, &["a", "b", "c"]).await;
    assert_eq!(hrefs_and_etags(&running).await, before);
    running.stop().await;
}

// Invitations, RSVP, apply and import (plan §5 AC 5–8, 10, 13). Each
// organizer address is the test's own, so Mailpit queries never see
// another test's mail.

use common::cal_flow::{
    ACCOUNT as CAL, CalAccount, Invite, TRUSTED, default_href, exactly_replies, replies,
};
use serde_json::json;
use tagmail_api_core::WriteService;
use tagmail_api_core::ops_dto::{CalConflictDto, CalReplyStateDto, OpStatusDto};
use tagmail_store::calendar::Scheduling;

const PASS_RESULTS: &str = "Authentication-Results: mx.tagmail.test;\r\n dkim=pass header.d=example.com;\r\n dmarc=pass header.from=example.com\r\n";

/// A local part holds at most 64 octets (RFC 5321 §4.5.3.1.1).
const MAX_LOCAL_PART: usize = 64;
const ORGANIZER_PREFIX: &str = "org-";

fn organizer(test: &str) -> String {
    let name: String = test
        .replace('_', "-")
        .chars()
        .take(MAX_LOCAL_PART - ORGANIZER_PREFIX.len())
        .collect();
    format!("{ORGANIZER_PREFIX}{name}@example.com")
}

async fn put_object(user: &str, name: &str, ics: &str) {
    let path = format!("{}{name}", default_href(user));
    let resp = dav(
        user,
        "PUT",
        &path,
        &[("Content-Type", "text/calendar; charset=utf-8")],
        ics,
    )
    .await;
    assert!(resp.status < 300, "PUT {path}: {}", resp.head);
}

async fn object(user: &str, name: &str) -> Option<String> {
    let resp = dav(
        user,
        "GET",
        &format!("{}{name}", default_href(user)),
        &[],
        "",
    )
    .await;
    (resp.status == 200).then(|| resp.body.replace("\r\n ", ""))
}

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

async fn rsvp(account: &CalAccount, op_id: &str, message: i64, partstat: &str) -> String {
    let op = account
        .submit(
            "rsvp",
            json!({ "opId": op_id, "messageId": message.to_string(), "partstat": partstat }),
        )
        .await;
    op.op_id
}

#[tokio::test]
async fn rsvp_client_mode_sends_one_reply() {
    let test = "rsvp_client_mode_sends_one_reply";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    rsvp(&account, "c-1", message, "accepted").await;
    let (status, _) = account
        .call(
            tagmail_api_core::Method::Post,
            "/api/calendar/rsvp",
            &json!({ "opId": "c-1", "messageId": message.to_string(), "partstat": "accepted" }),
        )
        .await;
    assert_eq!(status, 200, "a replayed op id answers its op");
    let done = account.settled("c-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);

    let reply = exactly_replies(&org, &uid, 1).await.remove(0);
    assert!(reply.contains("METHOD:REPLY"));
    assert_eq!(reply.matches("ATTENDEE").count(), 1, "{reply}");
    assert!(reply.contains("SEQUENCE:0"));
    assert!(reply.contains(&format!("From: <{me}>")) || reply.contains(&format!("<{me}>")));
    assert_eq!(account.outbox_ops().await, ["c-1#reply"]);
    let seen: Vec<String> = account
        .running
        .config
        .read(|c| {
            let mut stmt = c.prepare("SELECT kind FROM op_seen")?;
            Ok(stmt
                .query_map([], |r| r.get(0))?
                .collect::<tagmail_store::rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(seen, ["cal"]);
    account.stop().await;
}

#[tokio::test]
async fn accept_stores_partstat() {
    let test = "accept_stores_partstat";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    rsvp(&account, "a-1", message, "accepted").await;
    assert_eq!(account.settled("a-1").await.status, OpStatusDto::Done);
    let stored = object(cal.user, &format!("{uid}.ics"))
        .await
        .expect("saved");
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    assert!(!stored.contains("METHOD:"), "{stored}");
    let card = account.invitation(message).await;
    assert_eq!(card["stored"]["partstat"], "accepted");
    account.stop().await;
}

#[tokio::test]
async fn decline_keeps_declined() {
    let test = "decline_keeps_declined";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    put_object(cal.user, "kept.ics", &invite(&uid, &org, &me).stored()).await;
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    rsvp(&account, "d-1", message, "declined").await;
    assert_eq!(account.settled("d-1").await.status, OpStatusDto::Done);
    let stored = object(cal.user, "kept.ics").await.expect("kept");
    assert!(stored.contains("PARTSTAT=DECLINED"), "{stored}");
    exactly_replies(&org, &uid, 1).await;
    account.stop().await;
}

#[tokio::test]
async fn decline_tolerates_server_removal() {
    let test = "decline_tolerates_server_removal";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    put_object(cal.user, "gone.ics", &invite(&uid, &org, &me).stored()).await;
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    account.running.engine.stop_calendar().await;
    rsvp(&account, "g-1", message, "declined").await;
    let removed = dav(
        cal.user,
        "DELETE",
        &format!("{}gone.ics", default_href(cal.user)),
        &[],
        "",
    )
    .await;
    assert!(removed.status < 300);
    account
        .running
        .engine
        .start_calendar_with(DiscoveryEnv::system())
        .unwrap();
    let done = account.settled("g-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    exactly_replies(&org, &uid, 1).await;
    account.stop().await;
}

#[tokio::test]
async fn rsvp_server_mode_queues_no_outbox() {
    let test = "rsvp_server_mode_queues_no_outbox";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, Some(Scheduling::Server)).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let delivered = invite(&uid, &org, &me).stored().replace(
        "ORGANIZER;CN=Organizer:",
        "ORGANIZER;CN=Organizer;SCHEDULE-STATUS=1.2:",
    );
    put_object(cal.user, "scheduled.ics", &delivered).await;
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    rsvp(&account, "s-1", message, "accepted").await;
    let done = account.settled("s-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert!(account.outbox_ops().await.is_empty());
    tokio::time::sleep(common::cal_flow::QUIET).await;
    assert!(replies(&org, &uid).await.is_empty());
    let stored = object(cal.user, "scheduled.ics").await.unwrap();
    assert!(stored.contains("PARTSTAT=ACCEPTED") && !stored.contains("SCHEDULE-AGENT=CLIENT"));
    account.stop().await;
}

#[tokio::test]
async fn advertised_but_undelivered_schedule_status_queues_reply() {
    let test = "advertised_but_undelivered_schedule_status_queues_reply";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, Some(Scheduling::Server)).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    account
        .submit(
            "import",
            json!({ "opId": "i-1", "messageId": message.to_string() }),
        )
        .await;
    assert_eq!(account.settled("i-1").await.status, OpStatusDto::Done);
    rsvp(&account, "u-1", message, "accepted").await;
    assert_eq!(account.settled("u-1").await.status, OpStatusDto::Done);
    exactly_replies(&org, &uid, 1).await;
    account.stop().await;
}

#[tokio::test]
async fn client_mode_put_failure_shows_reply_sent_not_saved() {
    let test = "client_mode_put_failure_shows_reply_sent_not_saved";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    account.running.engine.stop_calendar().await;
    let gone = dav(cal.user, "DELETE", &default_href(cal.user), &[], "").await;
    assert!(gone.status < 300, "{}", gone.head);
    rsvp(&account, "f-1", message, "accepted").await;
    account
        .running
        .engine
        .start_calendar_with(DiscoveryEnv::system())
        .unwrap();
    exactly_replies(&org, &uid, 1).await;
    eventually(
        "reply sent, not saved",
        common::cal_flow::BUDGET,
        || async {
            let op = account.op("f-1").await;
            op.status == OpStatusDto::Failed
                && op.calendar.as_ref().and_then(|c| c.reply) == Some(CalReplyStateDto::Sent)
        },
    )
    .await;
    account.stop().await;
}

/// An answered event in the calendar and an account that trusts
/// [`TRUSTED`]; returns the account once the event is cached.
async fn answered(test: &str, user: &'static str, sequence: u32) -> (CalAccount, String) {
    let account = CalAccount::start(user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let mut stored = invite(&uid, &org, &me);
    stored.partstat = "ACCEPTED";
    stored.sequence = sequence;
    put_object(user, "event.ics", &stored.stored()).await;
    common::cal_flow::set_trust(&account.running, CAL, Some(TRUSTED)).await;
    account.running.engine.settings_changed(CAL);
    account.cached(&uid).await;
    (account, uid)
}

fn update<'a>(uid: &'a str, org: &'a str, me: &'a str, headers: &'a str) -> Invite<'a> {
    Invite {
        sequence: 1,
        dtstamp: "20261110T120000Z",
        summary: "Moved",
        headers,
        ..invite(uid, org, me)
    }
}

#[tokio::test]
async fn higher_sequence_auto_applies_when_responded() {
    let test = "higher_sequence_auto_applies_when_responded";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let (org, me) = (organizer(test), account.email());
    let message = account
        .deliver(
            &update(&uid, &org, &me, PASS_RESULTS).mail("Update"),
            "Update",
        )
        .await;
    eventually("update applied", common::cal_flow::BUDGET, || async {
        object(cal.user, "event.ics")
            .await
            .is_some_and(|o| o.contains("Moved"))
    })
    .await;
    let stored = object(cal.user, "event.ics").await.unwrap();
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    assert_eq!(account.cal_ops("apply").await.len(), 1);
    let card = account.invitation(message).await;
    assert_eq!(card["senderAuth"], "verified");
    assert_eq!(card["autoApply"], "applied");
    tokio::time::sleep(common::cal_flow::QUIET).await;
    assert!(
        replies(&org, &uid).await.is_empty(),
        "an update sends no mail"
    );
    account.stop().await;
}

#[tokio::test]
async fn cancel_sets_cancelled() {
    let test = "cancel_sets_cancelled";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let (org, me) = (organizer(test), account.email());
    let cancel = Invite {
        method: "CANCEL",
        ..update(&uid, &org, &me, PASS_RESULTS)
    };
    account
        .deliver(&cancel.mail("Cancelled"), "Cancelled")
        .await;
    eventually("cancel applied", common::cal_flow::BUDGET, || async {
        object(cal.user, "event.ics")
            .await
            .is_some_and(|o| o.contains("STATUS:CANCELLED"))
    })
    .await;
    account.stop().await;
}

#[tokio::test]
async fn lower_sequence_marks_outdated() {
    let test = "lower_sequence_marks_outdated";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 2).await;
    let (org, me) = (organizer(test), account.email());
    let message = account
        .deliver(&update(&uid, &org, &me, PASS_RESULTS).mail("Old"), "Old")
        .await;
    account.inbox_drained().await;
    let card = account.invitation(message).await;
    assert_eq!(card["outdated"], true);
    assert_eq!(card["autoApply"], "none");
    assert!(account.cal_ops("apply").await.is_empty());
    assert!(
        !object(cal.user, "event.ics")
            .await
            .unwrap()
            .contains("Moved")
    );
    account.stop().await;
}

/// Delivers an update that must not be applied without asking, checks the
/// card, then applies it by hand.
async fn manual_update(test: &str, from: &str, headers: &str, sender_auth: &str) {
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let (org, me) = (organizer(test), account.email());
    let mail = Invite {
        from,
        ..update(&uid, &org, &me, headers)
    }
    .mail("Update");
    let message = account.deliver(&mail, "Update").await;
    account.inbox_drained().await;
    assert!(account.cal_ops("apply").await.is_empty());
    assert!(
        !object(cal.user, "event.ics")
            .await
            .unwrap()
            .contains("Moved")
    );
    let card = account.invitation(message).await;
    assert_eq!(card["senderAuth"], sender_auth);
    assert_eq!(card["autoApply"], "manual");

    account
        .submit(
            "apply",
            json!({ "opId": "m-1", "messageId": message.to_string() }),
        )
        .await;
    let done = account.settled("m-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    let stored = object(cal.user, "event.ics").await.unwrap();
    assert!(stored.contains("Moved") && stored.contains("PARTSTAT=ACCEPTED"));
    account.stop().await;
}

#[tokio::test]
async fn spoofed_organizer_is_not_auto_applied() {
    let results =
        "Authentication-Results: mx.tagmail.test; dmarc=pass header.from=evil.example\r\n";
    manual_update(
        "spoofed_organizer_is_not_auto_applied",
        "boss@evil.example",
        results,
        "address_mismatch",
    )
    .await;
}

#[tokio::test]
async fn forged_from_without_authentication_results_is_manual() {
    let test = "forged_from_without_authentication_results_is_manual";
    manual_update(test, &organizer(test), "", "unauthenticated").await;
}

#[tokio::test]
async fn authentication_results_from_untrusted_authserv_id_ignored() {
    let test = "authentication_results_from_untrusted_authserv_id_ignored";
    let results = "Authentication-Results: mx.evil.example; dmarc=pass header.from=example.com\r\n";
    manual_update(test, &organizer(test), results, "unauthenticated").await;
}

#[tokio::test]
async fn dkim_pass_unaligned_domain_is_manual() {
    let test = "dkim_pass_unaligned_domain_is_manual";
    let results =
        "Authentication-Results: mx.tagmail.test; dkim=pass header.d=mailer.other.example\r\n";
    manual_update(test, &organizer(test), results, "unauthenticated").await;
}

#[tokio::test]
async fn organizer_takeover_is_not_auto_applied() {
    let test = "organizer_takeover_is_not_auto_applied";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let me = account.email();
    let thief = "thief@example.com";
    let takeover = Invite {
        from: thief,
        ..update(&uid, thief, &me, PASS_RESULTS)
    };
    let message = account
        .deliver(&takeover.mail("Takeover"), "Takeover")
        .await;
    account.inbox_drained().await;
    assert!(account.cal_ops("apply").await.is_empty());
    let card = account.invitation(message).await;
    assert_eq!(card["senderAuth"], "verified");
    assert_eq!(card["organizerChanged"], true);
    assert_eq!(card["autoApply"], "manual");

    let (status, refused) = account
        .call(
            tagmail_api_core::Method::Post,
            "/api/calendar/apply",
            &json!({ "opId": "t-1", "messageId": message.to_string() }),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["field"], "allowOrganizerChange");
    account
        .submit(
            "apply",
            json!({
                "opId": "t-2",
                "messageId": message.to_string(),
                "allowOrganizerChange": true
            }),
        )
        .await;
    assert_eq!(account.settled("t-2").await.status, OpStatusDto::Done);
    assert!(object(cal.user, "event.ics").await.unwrap().contains(thief));
    account.stop().await;
}

#[tokio::test]
async fn invitation_get_has_no_side_effects() {
    let test = "invitation_get_has_no_side_effects";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let (org, me) = (organizer(test), account.email());
    account.running.engine.stop_calendar().await;
    let message = account
        .deliver(
            &update(&uid, &org, &me, PASS_RESULTS).mail("Update"),
            "Update",
        )
        .await;
    for _ in 0..2 {
        let card = account.invitation(message).await;
        assert_eq!(card["autoApply"], "pending");
    }
    assert!(account.cal_ops("apply").await.is_empty());
    let queued = account
        .running
        .query("SELECT count(*) FROM cal_inbox", |r| r.get::<_, i64>(0))
        .await;
    assert_eq!(queued, [1]);
    assert!(
        !object(cal.user, "event.ics")
            .await
            .unwrap()
            .contains("Moved")
    );
    account.stop().await;
}

#[tokio::test]
async fn resync_does_not_requeue_apply() {
    let test = "resync_does_not_requeue_apply";
    let cal = CalFixture::for_test(test).await;
    let (account, uid) = answered(test, cal.user, 0).await;
    let (org, me) = (organizer(test), account.email());
    account
        .deliver(
            &update(&uid, &org, &me, PASS_RESULTS).mail("Update"),
            "Update",
        )
        .await;
    eventually("update applied", common::cal_flow::BUDGET, || async {
        object(cal.user, "event.ics")
            .await
            .is_some_and(|o| o.contains("Moved"))
    })
    .await;
    let applied = account.cal_ops("apply").await;
    assert_eq!(applied.len(), 1);
    let CalAccount {
        fixture, running, ..
    } = account;
    running.stop().await;

    fixture.wipe_cache();
    let running = fixture.start_sending(common::Sending::default()).await;
    running
        .engine
        .start_calendar_with(DiscoveryEnv::system())
        .unwrap();
    let account = CalAccount {
        api: common::cal_flow::api(&fixture, &running).await,
        fixture,
        running,
        user: cal.user,
    };
    account.cached(&uid).await;
    eventually(
        "invitation requeued and processed",
        common::cal_flow::BUDGET,
        || async {
            account
                .running
                .query("SELECT count(*) FROM message WHERE has_calendar = 1", |r| {
                    r.get::<_, i64>(0)
                })
                .await
                == [1]
        },
    )
    .await;
    account.inbox_drained().await;
    assert_eq!(account.cal_ops("apply").await, applied);
    account.stop().await;
}

#[tokio::test]
async fn publish_import_puts_each_vevent_and_sends_no_mail() {
    let test = "publish_import_puts_each_vevent_and_sends_no_mail";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let (a, b) = (
        common::cal_flow::unique_uid(&format!("{test}-a")),
        common::cal_flow::unique_uid(&format!("{test}-b")),
    );
    let vevents = [
        invite(&a, &org, &me).vevent(),
        invite(&b, &org, &me).vevent(),
    ]
    .concat();
    let mail = format!(
        "From: <{org}>\r\nTo: <{me}>\r\nSubject: Schedule\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/calendar; method=PUBLISH; charset=utf-8\r\n\r\n\
         BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nMETHOD:PUBLISH\r\n{vevents}\
         END:VCALENDAR\r\n"
    );
    let message = account.deliver(&mail, "Schedule").await;
    account
        .submit(
            "import",
            json!({ "opId": "p-1", "messageId": message.to_string() }),
        )
        .await;
    assert_eq!(account.settled("p-1").await.status, OpStatusDto::Done);
    for uid in [&a, &b] {
        let stored = object(cal.user, &format!("{uid}.ics"))
            .await
            .expect("imported");
        assert!(!stored.contains("METHOD:"), "{stored}");
    }
    assert!(account.outbox_ops().await.is_empty());
    tokio::time::sleep(common::cal_flow::QUIET).await;
    assert!(replies(&org, &a).await.is_empty() && replies(&org, &b).await.is_empty());
    account.stop().await;
}

#[tokio::test]
async fn import_updates_existing_uid_after_lookup() {
    let test = "import_updates_existing_uid_after_lookup";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    put_object(cal.user, "elsewhere.ics", &invite(&uid, &org, &me).stored()).await;
    let newer = update(&uid, &org, &me, "");
    let message = account.deliver(&newer.mail("Newer"), "Newer").await;
    account
        .submit(
            "import",
            json!({ "opId": "x-1", "messageId": message.to_string() }),
        )
        .await;
    assert_eq!(account.settled("x-1").await.status, OpStatusDto::Done);
    assert!(
        object(cal.user, "elsewhere.ics")
            .await
            .unwrap()
            .contains("Moved")
    );
    assert_eq!(object(cal.user, &format!("{uid}.ics")).await, None);
    account.stop().await;
}

#[tokio::test]
async fn fallback_mapping_targets_other_account_and_op_is_owned_by_source() {
    let test = "fallback_mapping_targets_other_account_and_op_is_owned_by_source";
    let source = CalFixture::for_test(test).await;
    let target = CalFixture::for_test(&format!("{test}/calendar")).await;
    let fixture = Fixture::existing(source.user, PASSWORD)
        .with_account(target.user)
        .await;
    let running = fixture.start_sending(common::Sending::default()).await;
    let calendars = AccountId(2);
    common::cal_flow::configure(&running, calendars, None).await;
    running
        .engine
        .start_calendar_with(DiscoveryEnv::system())
        .unwrap();
    common::cal_flow::discover(&running, calendars).await;
    common::cal_flow::synced(&running, calendars).await;
    common::cal_flow::set_default(&running, calendars, Some(default_href(target.user))).await;
    running
        .config
        .write(move |tx| {
            calendar::save_caldav(
                tx,
                CAL,
                &CaldavConfig {
                    fallback_account: Some(calendars),
                    ..CaldavConfig::default()
                },
            )
        })
        .await
        .unwrap();
    let account = CalAccount {
        api: common::cal_flow::api(&fixture, &running).await,
        fixture,
        running,
        user: source.user,
    };
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    let card = account.invitation(message).await;
    assert_eq!(card["target"]["accountId"], 2);
    rsvp(&account, "f-1", message, "tentative").await;
    let done = account.settled("f-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(done.account_id, CAL.0);
    assert_eq!(done.calendar.unwrap().target_account_id, 2);
    let stored = object(target.user, &format!("{uid}.ics"))
        .await
        .expect("saved");
    assert!(stored.contains("PARTSTAT=TENTATIVE"));
    let reply = exactly_replies(&org, &uid, 1).await.remove(0);
    assert!(reply.contains(&me), "the source account's identity replies");
    account.stop().await;
}

#[tokio::test]
async fn no_calendar_still_sends_reply() {
    let test = "no_calendar_still_sends_reply";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    common::cal_flow::set_default(&account.running, CAL, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let message = account
        .deliver(&invite(&uid, &org, &me).mail("Invite"), "Invite")
        .await;
    let card = account.invitation(message).await;
    assert_eq!(card["target"], serde_json::Value::Null);
    let op = account
        .submit(
            "rsvp",
            json!({ "opId": "n-1", "messageId": message.to_string(), "partstat": "accepted" }),
        )
        .await;
    assert_eq!(op.status, OpStatusDto::Done);
    exactly_replies(&org, &uid, 1).await;
    account.stop().await;
}

#[tokio::test]
async fn put_412_server_wins_and_keeps_rejected_ics() {
    let test = "put_412_server_wins_and_keeps_rejected_ics";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let (org, me) = (organizer(test), account.email());
    let uid = common::cal_flow::unique_uid(test);
    let server_copy = update(&uid, &org, &me, "").stored();
    put_object(cal.user, "edited.ics", &server_copy).await;
    let edit = invite(&uid, &org, &me).stored().replace("Review", "MyEdit");
    let href = format!("{}edited.ics", default_href(cal.user));
    account
        .running
        .engine
        .queue_calendar_put(
            "e-1",
            CAL,
            &default_href(cal.user),
            &href,
            &edit,
            Some("\"stale\""),
        )
        .await
        .unwrap();
    let failed = account.settled("e-1").await;
    assert_eq!(failed.status, OpStatusDto::Failed);
    assert_eq!(
        failed.calendar.as_ref().unwrap().conflict,
        Some(CalConflictDto::ServerWon)
    );
    assert!(
        object(cal.user, "edited.ics")
            .await
            .unwrap()
            .contains("Moved")
    );
    let kept = std::fs::read_to_string(
        account
            .fixture
            .outbox_dir()
            .join("cal")
            .join("e-1.conflict.ics"),
    )
    .unwrap();
    assert_eq!(kept, edit);

    WriteService::retry_op(&account.running.engine, "e-1".into())
        .await
        .unwrap();
    let done = account.settled("e-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert!(
        object(cal.user, "edited.ics")
            .await
            .unwrap()
            .contains("MyEdit")
    );
    account.stop().await;
}

fn event_fields(summary: &str, start: &str, end: &str, attendees: &[&str]) -> serde_json::Value {
    json!({
        "summary": summary,
        "location": "Room 4",
        "description": null,
        "start": start,
        "end": end,
        "allDay": false,
        "tzid": "Europe/Zurich",
        "attendees": attendees.iter().map(|a| json!({ "email": a })).collect::<Vec<_>>(),
        "alarms": [15],
    })
}

fn put_body(
    op_id: &str,
    user: &str,
    href: Option<&str>,
    etag: Option<&serde_json::Value>,
    event: serde_json::Value,
) -> serde_json::Value {
    let mut body = json!({
        "opId": op_id,
        "kind": "put",
        "accountId": CAL.0,
        "calendarHref": default_href(user),
        "event": event,
    });
    if let Some(href) = href {
        body["href"] = json!(href);
    }
    if let Some(etag) = etag {
        body["etag"] = etag.clone();
    }
    body
}

#[tokio::test]
async fn edit_roundtrips_to_radicale() {
    let test = "edit_roundtrips_to_radicale";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let uid = common::cal_flow::unique_uid(test);
    let original = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:20261101T120000Z\r\nSEQUENCE:2\r\n\
         DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:Draft\r\n\
         X-TAGMAIL-TEST;X-P=1:kept\r\nCATEGORIES:Work\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Draft\r\nTRIGGER:-PT10M\r\n\
         X-ALARM-NOTE:kept too\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    put_object(cal.user, "edit.ics", &original).await;
    account.cached(&uid).await;
    let href = format!("{}edit.ics", default_href(cal.user));
    let detail = account.detail(&href, None).await;
    let mut fields = detail["fields"].clone();
    assert_eq!(fields["start"], "2099-12-01T09:00:00");
    assert_eq!(fields["tzid"], "UTC");
    assert_eq!(fields["alarms"], json!([10]));
    fields["summary"] = json!("Final");
    account
        .edit(put_body(
            "e-1",
            cal.user,
            Some(&href),
            Some(&detail["etag"]),
            fields,
        ))
        .await;
    let stored = object(cal.user, "edit.ics").await.unwrap();
    for kept in [
        "X-TAGMAIL-TEST;X-P=1:kept",
        "X-ALARM-NOTE:kept too",
        "TRIGGER:-PT10M",
        "CATEGORIES:Work",
        "DTSTART:20991201T090000Z",
    ] {
        assert!(stored.contains(kept), "{kept} in {stored}");
    }
    assert!(
        stored.contains("SUMMARY:Final") && stored.contains("SEQUENCE:3"),
        "{stored}"
    );
    let reread = account.detail(&href, Some(&detail["etag"])).await;
    assert_eq!(reread["fields"]["summary"], "Final");

    let mut all_day = reread["fields"].clone();
    all_day["allDay"] = json!(true);
    all_day["start"] = json!("2099-12-02");
    all_day["end"] = json!("2099-12-03");
    all_day["tzid"] = json!(null);
    all_day["alarms"] = json!([0, 60]);
    account
        .edit(put_body(
            "e-2",
            cal.user,
            Some(&href),
            Some(&reread["etag"]),
            all_day,
        ))
        .await;
    let stored = object(cal.user, "edit.ics").await.unwrap();
    assert!(stored.contains("DTSTART;VALUE=DATE:20991202"), "{stored}");
    assert!(
        stored.contains("TRIGGER:-PT1H") && !stored.contains("TRIGGER:-PT10M"),
        "{stored}"
    );

    account
        .edit(json!({
            "opId": "e-3",
            "kind": "delete",
            "accountId": CAL.0,
            "calendarHref": default_href(cal.user),
            "href": href,
        }))
        .await;
    assert!(object(cal.user, "edit.ics").await.is_none());
    assert!(
        account.outbox_ops().await.is_empty(),
        "no attendees, no mail"
    );
    account.stop().await;
}

#[tokio::test]
async fn invite_client_mode_sends_request_from_account_identity() {
    let test = "invite_client_mode_sends_request_from_account_identity";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let guest = organizer(test);
    let other = "guest2@example.com";
    let event = event_fields(
        "Kickoff",
        "2099-12-01T09:00:00",
        "2099-12-01T10:00:00",
        &[&guest, other],
    );
    account
        .edit(put_body("i-1", cal.user, None, None, event))
        .await;
    let (_, uid) = account.written("i-1").await;
    let request = common::cal_flow::exactly(&guest, &uid, "REQUEST", 1)
        .await
        .remove(0);
    let me = account.email();
    assert!(request.contains(&format!("<{me}>")), "{request}");
    assert!(
        request
            .lines()
            .any(|l| l.starts_with("ORGANIZER") && l.trim_end().ends_with(&format!("mailto:{me}"))),
        "{request}"
    );
    assert!(
        request.contains(&format!("mailto:{guest}"))
            && request.contains(&format!("mailto:{other}"))
    );
    assert!(
        !request.contains("SCHEDULE-AGENT") && !request.contains("VALARM"),
        "{request}"
    );
    assert_eq!(account.outbox_ops().await, ["i-1#request"]);
    account.stop().await;
}

#[tokio::test]
async fn time_change_bumps_sequence_and_sends_updated_request() {
    let test = "time_change_bumps_sequence_and_sends_updated_request";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let guest = organizer(test);
    let event = event_fields(
        "Sync",
        "2099-12-01T09:00:00",
        "2099-12-01T10:00:00",
        &[&guest],
    );
    account
        .edit(put_body("t-1", cal.user, None, None, event))
        .await;
    let (href, uid) = account.written("t-1").await;
    common::cal_flow::exactly(&guest, &uid, "REQUEST", 1).await;
    let detail = account.detail(&href, None).await;
    assert_eq!(detail["sequence"], 0);
    let moved = event_fields(
        "Sync",
        "2099-12-01T11:00:00",
        "2099-12-01T12:00:00",
        &[&guest],
    );
    account
        .edit(put_body(
            "t-2",
            cal.user,
            Some(&href),
            Some(&detail["etag"]),
            moved,
        ))
        .await;
    let requests = common::cal_flow::exactly(&guest, &uid, "REQUEST", 2).await;
    let updated = requests
        .iter()
        .find(|r| r.contains("SEQUENCE:1"))
        .expect("an updated REQUEST");
    assert!(
        updated.contains("DTSTART;TZID=Europe/Zurich:20991201T110000"),
        "{updated}"
    );
    assert!(updated.contains("BEGIN:VTIMEZONE"), "{updated}");
    account.stop().await;
}

#[tokio::test]
async fn delete_sends_cancel() {
    let test = "delete_sends_cancel";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, None).await;
    let guest = organizer(test);
    let event = event_fields(
        "Gone",
        "2099-12-01T09:00:00",
        "2099-12-01T10:00:00",
        &[&guest],
    );
    account
        .edit(put_body("d-1", cal.user, None, None, event))
        .await;
    let (href, uid) = account.written("d-1").await;
    let detail = account.detail(&href, None).await;
    account
        .edit(json!({
            "opId": "d-2",
            "kind": "delete",
            "accountId": CAL.0,
            "calendarHref": default_href(cal.user),
            "href": href,
            "etag": detail["etag"],
        }))
        .await;
    let cancel = common::cal_flow::exactly(&guest, &uid, "CANCEL", 1)
        .await
        .remove(0);
    assert!(
        cancel.contains("STATUS:CANCELLED") && cancel.contains("SEQUENCE:1"),
        "{cancel}"
    );
    assert_eq!(account.outbox_ops().await, ["d-1#request", "d-2#cancel"]);
    account.stop().await;
}

#[tokio::test]
async fn invite_server_mode_sends_no_outbox() {
    let test = "invite_server_mode_sends_no_outbox";
    let cal = CalFixture::for_test(test).await;
    let account = CalAccount::start(cal.user, Some(Scheduling::Server)).await;
    let guest = organizer(test);
    let event = event_fields(
        "Served",
        "2099-12-01T09:00:00",
        "2099-12-01T10:00:00",
        &[&guest],
    );
    account
        .edit(put_body("s-1", cal.user, None, None, event))
        .await;
    let (href, uid) = account.written("s-1").await;
    let stored = object(cal.user, href.rsplit('/').next().unwrap())
        .await
        .unwrap();
    assert!(
        stored.contains(&format!("mailto:{guest}")) && !stored.contains("SCHEDULE-AGENT"),
        "{stored}"
    );
    tokio::time::sleep(common::cal_flow::QUIET).await;
    assert!(account.outbox_ops().await.is_empty());
    assert!(
        common::cal_flow::itip_mails(&guest, &uid, "REQUEST")
            .await
            .is_empty()
    );
    account.stop().await;
}

/// An event I organize with `guest` invited, cached; the account trusts
/// [`TRUSTED`].
async fn organized(test: &str, user: &'static str) -> (CalAccount, String, String) {
    let account = CalAccount::start(user, None).await;
    let guest = organizer(test);
    let uid = common::cal_flow::unique_uid(test);
    let me = account.email();
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//test//EN\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:20261101T120000Z\r\nSEQUENCE:0\r\n\
         DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:Review\r\n\
         ORGANIZER;CN=Me:mailto:{me}\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{guest}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    put_object(user, "mine.ics", &ics).await;
    common::cal_flow::set_trust(&account.running, CAL, Some(TRUSTED)).await;
    account.cached(&uid).await;
    (account, uid, guest)
}

fn reply_from<'a>(
    uid: &'a str,
    me: &'a str,
    guest: &'a str,
    from: &'a str,
    headers: &'a str,
) -> String {
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//other//EN\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:20261102T120000Z\r\nSEQUENCE:0\r\n\
         DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:Review\r\n\
         ORGANIZER;CN=Me:mailto:{me}\r\n\
         ATTENDEE;PARTSTAT=ACCEPTED:mailto:{guest}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    format!(
        "{headers}From: <{from}>\r\nTo: <{me}>\r\nSubject: Accepted\r\n\
         Message-ID: <{uid}-reply@tagmail.test>\r\nDate: Wed, 25 Nov 2026 10:00:00 +0000\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/calendar; method=REPLY; charset=utf-8\r\n\r\n{ics}"
    )
}

#[tokio::test]
async fn incoming_reply_updates_attendee_partstat() {
    let test = "incoming_reply_updates_attendee_partstat";
    let cal = CalFixture::for_test(test).await;
    let (account, uid, guest) = organized(test, cal.user).await;
    let me = account.email();
    let message = account
        .deliver(
            &reply_from(&uid, &me, &guest, &guest, PASS_RESULTS),
            "Accepted",
        )
        .await;
    eventually("reply applied", common::cal_flow::BUDGET, || async {
        object(cal.user, "mine.ics")
            .await
            .is_some_and(|o| o.contains(&format!("PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:{guest}")))
    })
    .await;
    assert_eq!(account.cal_ops("apply").await.len(), 1);
    account.cached(&uid).await;
    eventually("card applied", common::cal_flow::BUDGET, || async {
        account.invitation(message).await["autoApply"] == "applied"
    })
    .await;
    assert_eq!(account.invitation(message).await["senderAuth"], "verified");
    assert!(
        account.outbox_ops().await.is_empty(),
        "a reply is answered by nothing"
    );
    account.stop().await;
}

/// Delivers a REPLY that must not be applied without asking, checks the
/// card, then applies it by hand.
async fn manual_reply(test: &str, from: Option<&str>, headers: &str, sender_auth: &str) {
    let cal = CalFixture::for_test(test).await;
    let (account, uid, guest) = organized(test, cal.user).await;
    let me = account.email();
    let from = from.map_or(guest.clone(), str::to_owned);
    let message = account
        .deliver(&reply_from(&uid, &me, &guest, &from, headers), "Accepted")
        .await;
    account.inbox_drained().await;
    assert!(account.cal_ops("apply").await.is_empty());
    assert!(
        object(cal.user, "mine.ics")
            .await
            .unwrap()
            .contains("PARTSTAT=NEEDS-ACTION")
    );
    let card = account.invitation(message).await;
    assert_eq!(card["method"], "reply");
    assert_eq!(card["senderAuth"], sender_auth);
    assert_eq!(card["autoApply"], "manual");

    account
        .submit(
            "apply",
            json!({ "opId": "m-1", "messageId": message.to_string() }),
        )
        .await;
    let done = account.settled("m-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert!(
        object(cal.user, "mine.ics")
            .await
            .unwrap()
            .contains(&format!("PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:{guest}"))
    );
    account.stop().await;
}

#[tokio::test]
async fn spoofed_reply_sender_is_not_applied() {
    manual_reply(
        "spoofed_reply_sender_is_not_applied",
        Some("eve@example.com"),
        PASS_RESULTS,
        "address_mismatch",
    )
    .await;
}

#[tokio::test]
async fn forged_reply_without_authentication_results_is_manual() {
    manual_reply(
        "forged_reply_without_authentication_results_is_manual",
        None,
        "",
        "unauthenticated",
    )
    .await;
}
