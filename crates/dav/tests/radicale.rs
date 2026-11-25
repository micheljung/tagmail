//! CalDAV client against Radicale (`cargo xtask radicale up`):
//! `cargo test -p tagmail-dav --features caldav-tests --test radicale`.
//! `TAGMAIL_CALDAV_ADDR` overrides the published port. The suite owns the
//! Radicale user `cal28` (row `dav_radicale_suite` in the engine's
//! `CAL_USERS`); every test works in its own collection.

mod common;

use common::Server;

const DEFAULT_ADDR: &str = "127.0.0.1:10232";
const USER: &str = "cal28";
const PASSWORD: &str = "engine";

fn server() -> Option<Server> {
    if std::env::var("TAGMAIL_IMAP_SERVER").is_ok_and(|s| s == "stalwart") {
        eprintln!("skipped: TAGMAIL_IMAP_SERVER=stalwart");
        return None;
    }
    Some(Server {
        addr: std::env::var("TAGMAIL_CALDAV_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned()),
        user: USER,
        password: PASSWORD,
        home: format!("/{USER}/"),
    })
}

#[tokio::test]
async fn manual_url() {
    let Some(server) = server() else { return };
    common::manual_url(&server, "manual_url").await;
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
    common::sync_collection_incremental(&server, "sync_collection", common::UnknownToken::Rejected)
        .await;
}
