//! Finds a message by Message-ID in the selected mailbox. Stalwart answers
//! `UID SEARCH HEADER Message-ID` with nothing once a mailbox holds more
//! than one message, so an empty answer falls back to reading the header
//! of the newest UIDs, where appended and sent mail lands.

use tagmail_core::email_id::normalize_message_id;
use tagmail_imap::{FetchSpec, ImapSession, SearchCriteria, SelectedMailbox, UidSet};

use crate::error::Result;
use crate::message::header_value;
use crate::sync;

/// Newest UIDs whose Message-ID header the fallback reads.
pub(crate) const SCAN_UIDS: u32 = 500;

/// UIDs in the selected mailbox `sel` whose Message-ID is `msgid`
/// (normalized, without angle brackets).
pub(crate) async fn find<S: ImapSession>(
    s: &mut S,
    sel: &SelectedMailbox,
    msgid: &str,
) -> Result<UidSet> {
    let found = s
        .uid_search(SearchCriteria::Header {
            name: "Message-ID".to_owned(),
            value: format!("<{msgid}>"),
        })
        .await?;
    if !found.is_empty() || sel.exists == 0 {
        return Ok(found);
    }
    let next = sync::uid_next(s, sel).await?;
    if next <= 1 {
        return Ok(found);
    }
    let mut tail = UidSet::new();
    tail.insert_range(next.saturating_sub(SCAN_UIDS).max(1), next - 1);
    let spec = FetchSpec {
        header_fields: vec!["MESSAGE-ID".to_owned()],
        ..FetchSpec::default()
    };
    let mut out = UidSet::new();
    for meta in s.fetch(&tail, &spec).await? {
        let header = meta.header_fields.as_deref().unwrap_or_default();
        let matches =
            header_value(header, "Message-ID").is_some_and(|v| normalize_message_id(&v) == msgid);
        if let (true, Some(uid)) = (matches, meta.uid) {
            out.insert(uid);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use tagmail_imap::{ImapSession, SelectOptions, Uid};

    use super::*;
    use crate::fake::{FakeConnector, FakeMsg, lock, server};

    #[tokio::test]
    async fn empty_header_search_falls_back_to_the_newest_uids() {
        let srv = server(&["IMAP4rev1"]);
        for i in 0..SCAN_UIDS + 2 {
            let id = format!("m{i}@x");
            lock(&srv).append("INBOX", FakeMsg::new(&id, "s", "a@x", "b@x", 1));
        }
        let mut s = FakeConnector(srv.clone()).session();
        let sel = s.select("INBOX", SelectOptions::default()).await.unwrap();
        let newest = format!("m{}@x", SCAN_UIDS + 1);
        assert_eq!(
            find(&mut s, &sel, &newest).await.unwrap().max(),
            Some(Uid(SCAN_UIDS + 2))
        );

        lock(&srv).header_search_empty = true;
        assert_eq!(
            find(&mut s, &sel, &newest).await.unwrap().max(),
            Some(Uid(SCAN_UIDS + 2)),
            "found by the header scan"
        );
        let scanned = lock(&srv).fetches.last().unwrap().uids.clone();
        assert_eq!(scanned.len(), usize::try_from(SCAN_UIDS).unwrap());
        assert!(
            find(&mut s, &sel, "m0@x").await.unwrap().is_empty(),
            "older mail is out of the scan window"
        );
        assert!(find(&mut s, &sel, "nope@x").await.unwrap().is_empty());
    }
}
