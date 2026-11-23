//! Finds a message by Message-ID in the selected mailbox. Stalwart answers
//! `UID SEARCH HEADER Message-ID` with nothing once a mailbox holds more
//! than one message, so an empty answer falls back to reading the header
//! of the newest UIDs, where appended and sent mail lands.

use std::collections::HashMap;

use tagmail_core::email_id::normalize_message_id;
use tagmail_imap::{FetchSpec, ImapSession, SearchCriteria, SelectedMailbox, UidSet};

use crate::error::Result;
use crate::message::header_value;
use crate::sync;

/// Newest UIDs whose Message-ID header the fallback reads.
pub(crate) const SCAN_UIDS: u32 = 500;

/// Message-ID lookups in one mailbox that read each of the newest headers
/// once: later lookups reuse what the fallback read and scan only UIDs that
/// arrived since.
#[derive(Debug, Default)]
pub(crate) struct Finder {
    /// UIDVALIDITY and UIDNEXT of the last scan.
    scanned: Option<(u32, u32)>,
    seen: HashMap<String, UidSet>,
}

impl Finder {
    /// UIDs in the selected mailbox `sel` whose Message-ID is `msgid`
    /// (normalized, without angle brackets).
    pub(crate) async fn find<S: ImapSession>(
        &mut self,
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
        let validity = sel.uid_validity.0;
        let from = match self.scanned {
            Some((v, done)) if v == validity => done,
            _ => {
                self.seen.clear();
                next.saturating_sub(SCAN_UIDS).max(1)
            }
        };
        if from < next {
            self.scan(s, from, next - 1).await?;
        }
        self.scanned = Some((validity, next.max(from)));
        Ok(self.seen.get(msgid).cloned().unwrap_or_default())
    }

    async fn scan<S: ImapSession>(&mut self, s: &mut S, first: u32, last: u32) -> Result<()> {
        let mut range = UidSet::new();
        range.insert_range(first, last);
        let spec = FetchSpec {
            header_fields: vec!["MESSAGE-ID".to_owned()],
            ..FetchSpec::default()
        };
        for meta in s.fetch(&range, &spec).await? {
            let header = meta.header_fields.as_deref().unwrap_or_default();
            if let (Some(v), Some(uid)) = (header_value(header, "Message-ID"), meta.uid) {
                self.seen
                    .entry(normalize_message_id(&v))
                    .or_default()
                    .insert(uid);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tagmail_imap::{ImapSession, SelectOptions, Uid};

    use super::*;
    use crate::fake::{FakeConnector, FakeMsg, Shared, lock, server};

    fn header_scans(srv: &Shared) -> Vec<Vec<u32>> {
        lock(srv)
            .header_fetches
            .iter()
            .map(|(_, uids)| uids.clone())
            .collect()
    }

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
        let found = Finder::default().find(&mut s, &sel, &newest).await;
        assert_eq!(found.unwrap().max(), Some(Uid(SCAN_UIDS + 2)));

        lock(&srv).header_search_empty = true;
        let mut finder = Finder::default();
        assert_eq!(
            finder.find(&mut s, &sel, &newest).await.unwrap().max(),
            Some(Uid(SCAN_UIDS + 2)),
            "found by the header scan"
        );
        let scanned = lock(&srv).fetches.last().unwrap().uids.clone();
        assert_eq!(scanned.len(), usize::try_from(SCAN_UIDS).unwrap());
        assert!(
            finder.find(&mut s, &sel, "m0@x").await.unwrap().is_empty(),
            "older mail is out of the scan window"
        );
        assert!(
            finder
                .find(&mut s, &sel, "nope@x")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(header_scans(&srv).len(), 1, "headers read once");
    }

    #[tokio::test]
    async fn later_lookups_scan_only_new_mail() {
        let srv = server(&["IMAP4rev1"]);
        lock(&srv).header_search_empty = true;
        for i in 0..3 {
            let id = format!("m{i}@x");
            lock(&srv).append("Sent", FakeMsg::new(&id, "s", "a@x", "b@x", 1));
        }
        let mut s = FakeConnector(srv.clone()).session();
        let mut finder = Finder::default();
        let sel = s.select("Sent", SelectOptions::default()).await.unwrap();
        assert!(
            finder
                .find(&mut s, &sel, "late@x")
                .await
                .unwrap()
                .is_empty()
        );

        lock(&srv).append("Sent", FakeMsg::new("late@x", "s", "a@x", "b@x", 1));
        let sel = s.select("Sent", SelectOptions::default()).await.unwrap();
        assert_eq!(
            finder.find(&mut s, &sel, "late@x").await.unwrap().max(),
            Some(Uid(4))
        );
        assert_eq!(
            finder.find(&mut s, &sel, "m1@x").await.unwrap().max(),
            Some(Uid(2))
        );
        assert_eq!(header_scans(&srv), [vec![1, 2, 3], vec![4]]);
    }
}
