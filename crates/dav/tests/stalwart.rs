//! CalDAV client against Stalwart (`cargo xtask stalwart up`):
//! `TAGMAIL_IMAP_SERVER=stalwart cargo test -p tagmail-dav --features
//! caldav-tests --test stalwart`. `TAGMAIL_CALDAV_ADDR` overrides the
//! published HTTP port. The suite uses the calendars of `user1`, which no
//! other CalDAV test touches; every test works in its own collection.

mod common;

use common::{Server, event};
use tagmail_dav::{
    DavError, Discovery, DiscoveryEnv, DiscoveryTarget, PutCondition, UidConflictKind,
};
use url::Url;

const DEFAULT_ADDR: &str = "127.0.0.1:10080";
const USER: &str = "user1";
const PASSWORD: &str = "pass1";
const MAIL_DOMAIN: &str = "tagmail.test";

fn server() -> Option<Server> {
    if !std::env::var("TAGMAIL_IMAP_SERVER").is_ok_and(|s| s == "stalwart") {
        eprintln!("skipped: set TAGMAIL_IMAP_SERVER=stalwart");
        return None;
    }
    Some(Server {
        addr: std::env::var("TAGMAIL_CALDAV_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned()),
        user: USER,
        password: PASSWORD,
        home: format!("/dav/cal/{USER}/"),
    })
}

#[tokio::test]
async fn manual_url() {
    let Some(server) = server() else { return };
    common::manual_url(&server, "manual_url").await;
}

#[tokio::test]
async fn well_known_stalwart() {
    let Some(server) = server() else { return };
    let calendar = server.calendar("well_known").await;
    let env = DiscoveryEnv::system().with_base_url(MAIL_DOMAIN, server.base());
    let target = DiscoveryTarget {
        email: "user1@tagmail.test",
        ..server.target()
    };
    let found = tagmail_dav::discover(target, &server.options(), &env)
        .await
        .unwrap();
    let Discovery::Found(account) = found else {
        panic!("expected Found, got {found:?}");
    };
    assert_eq!(account.home.path(), server.home);
    assert!(
        account.scheduling.inbox.is_some(),
        "{:?}",
        account.scheduling
    );
    assert!(
        account.scheduling.outbox.is_some(),
        "{:?}",
        account.scheduling
    );
    common::assert_discovered(&server, &account.calendars, &calendar).await;
}

#[tokio::test]
async fn put_get_delete_with_if_match() {
    let Some(server) = server() else { return };
    common::put_get_delete_with_if_match(&server, "put_get_delete").await;
}

#[tokio::test]
async fn stale_if_match_is_412() {
    let Some(server) = server() else { return };
    common::stale_if_match_is_412(&server, "stale_if_match").await;
}

#[tokio::test]
async fn etag_listing_and_multiget() {
    let Some(server) = server() else { return };
    common::etag_listing_and_multiget(&server, "etag_listing").await;
}

#[tokio::test]
async fn sync_collection_incremental() {
    let Some(server) = server() else { return };
    common::sync_collection_incremental(
        &server,
        "sync_collection",
        common::UnknownToken::FullListing,
    )
    .await;
}

#[tokio::test]
async fn no_uid_conflict() {
    let Some(server) = server() else { return };
    let calendar = server.calendar("no_uid_conflict").await;
    let client = server.client();
    let uid = "no-uid-conflict@tagmail.test";
    let original: Url = calendar.join("original.ics").unwrap();
    client
        .put(&original, &event(uid, "Original"), PutCondition::Create)
        .await
        .unwrap();
    let err = client
        .put(
            &calendar.join("copy.ics").unwrap(),
            &event(uid, "Copy"),
            PutCondition::Create,
        )
        .await
        .unwrap_err();
    let DavError::UidConflict { kind, existing } = err else {
        panic!("expected UidConflict, got {err:?}");
    };
    assert_eq!(kind, UidConflictKind::NoUidConflict);
    if let Some(existing) = existing {
        assert!(
            tagmail_dav::same_resource(&existing, &original),
            "{existing}"
        );
    }
    let found = client
        .find_by_uid(std::slice::from_ref(&calendar), uid)
        .await
        .unwrap()
        .unwrap();
    assert!(tagmail_dav::same_resource(&found.resource.href, &original));
}
