//! Calendar sync against a real CalDAV server (Radicale in CI, plan §5
//! AC 11 and 12): discovery from a manual URL with the IMAP login, then
//! `sync-collection`, the etag fallback and a cache rebuild.
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
