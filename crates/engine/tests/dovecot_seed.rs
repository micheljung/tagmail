//! AC 2 against the seeded Dovecot (`cargo xtask seed` + `cargo xtask
//! dovecot load`, into `user1`): the newest INBOX message is in the cache
//! within 30 s of starting a fresh sync. `TAGMAIL_SEED_MANIFEST` must point
//! at the seed's `tagmail-seed.json`.
#![cfg(feature = "dovecot-tests")]

mod common;

use std::time::Duration;

use common::*;
use tagmail_imap::*;

const USER: &str = "user1";
const PASS: &str = "pass1";
const NEWEST_INBOX_BUDGET: Duration = Duration::from_secs(30);

fn inbox_count() -> u64 {
    let path = std::env::var("TAGMAIL_SEED_MANIFEST")
        .expect("TAGMAIL_SEED_MANIFEST must name the loaded seed's tagmail-seed.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read manifest")).expect("json");
    manifest["mailboxes"]["INBOX"]["messages"]
        .as_u64()
        .expect("INBOX count")
}

#[tokio::test]
async fn newest_inbox_message_is_visible_within_30s() {
    let expected = inbox_count();
    let mut s = second_as(USER, PASS).await;
    let sel = s.select("INBOX", SelectOptions::default()).await.unwrap();
    assert_eq!(u64::from(sel.exists), expected, "seed not loaded into {USER}");
    let all = s.uid_search(SearchCriteria::All).await.unwrap();
    let spec = FetchSpec {
        internal_date: true,
        ..FetchSpec::default()
    };
    let mut newest = (i64::MIN, 0u32);
    for window in windows_newest_first(&all, 5000) {
        for m in s.fetch(&window, &spec).await.unwrap() {
            let date = m.internal_date.unwrap_or(i64::MIN);
            let uid = m.uid.map_or(0, |u| u.0);
            if (date, uid) > newest {
                newest = (date, uid);
            }
        }
    }
    let newest_uid = newest.1;

    let fixture = Fixture::existing(USER, PASS);
    let running = fixture.start(no_override(), None).await;
    let elapsed = eventually("newest INBOX message", NEWEST_INBOX_BUDGET, || async {
        running
            .cache
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM location l JOIN mailbox mb \
                     ON mb.id = l.mailbox_id WHERE mb.name = 'INBOX' AND l.uid = ?1)",
                    [newest_uid],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap()
    })
    .await;
    let visible = running
        .query("SELECT count(*) FROM location", |r| r.get::<_, i64>(0))
        .await;
    println!(
        "newest of {expected} INBOX messages (UID {newest_uid}) visible after {elapsed:?}; \
         {} locations cached by then",
        visible[0]
    );
    running.stop().await;
}
