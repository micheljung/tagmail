//! Test fixtures: a cache database in a temp dir and message builders.

use std::sync::Arc;

use tagmail_api_core::ChangeFeed;
use tagmail_api_core::dto::AddressDto;
use tagmail_core::{AccountId, EmailId, Flags, KeywordSet};
use tagmail_store::{BlobStore, CacheDb};
use tempfile::TempDir;

use crate::cache::{self, ListedMailbox};
use crate::ctx::Ctx;
use crate::message::{Fetched, LocationState, NewMessage, normalize_subject};

pub struct TestCache {
    pub ctx: Ctx,
    pub feed: Arc<ChangeFeed>,
    _dir: TempDir,
}

pub fn open() -> TestCache {
    let dir = TempDir::new().unwrap();
    let cache = Arc::new(CacheDb::open(&dir.path().join("cache.db")).unwrap());
    let blobs = Arc::new(BlobStore::open(&dir.path().join("blobs")).unwrap());
    let feed = Arc::new(ChangeFeed::new(tagmail_core::ChangeCursor {
        epoch: cache.epoch(),
        seq: 0,
    }));
    TestCache {
        ctx: Ctx::new(cache, blobs, feed.clone(), AccountId(1)),
        feed,
        _dir: dir,
    }
}

impl TestCache {
    /// Creates the mailboxes and returns their ids in order.
    pub async fn mailboxes(&self, names: &[&str]) -> Vec<i64> {
        let listed: Vec<ListedMailbox> = names
            .iter()
            .map(|n| ListedMailbox {
                name: (*n).to_owned(),
                delimiter: Some('/'),
                special_use: if *n == "Sent" {
                    Some(tagmail_core::SpecialUse::Sent)
                } else {
                    None
                },
            })
            .collect();
        self.ctx
            .write(move |w| cache::sync_mailbox_list(w, &listed))
            .await
            .unwrap();
        let rows = self.ctx.read(|c| cache::mailboxes(c, 1)).await.unwrap();
        names
            .iter()
            .map(|n| rows.iter().find(|r| r.name == *n).unwrap().id)
            .collect()
    }

    pub async fn add(&self, mailbox: i64, fetched: Vec<Fetched>) {
        self.ctx
            .write(move |w| cache::add_messages(w, mailbox, &fetched))
            .await
            .unwrap();
    }

    pub async fn query<T: Send + 'static>(
        &self,
        sql: &'static str,
        f: impl Fn(&tagmail_store::rusqlite::Row<'_>) -> tagmail_store::rusqlite::Result<T>
        + Send
        + 'static,
    ) -> Vec<T> {
        self.ctx
            .read(move |c| {
                let mut stmt = c.prepare(sql)?;
                let rows = stmt
                    .query_map([], |r| f(r))?
                    .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }
}

pub fn addr(email: &str) -> AddressDto {
    AddressDto {
        name: None,
        email: email.to_owned(),
    }
}

/// A test message; `id` doubles as the object id and Message-ID.
#[derive(Clone, Debug)]
pub struct Msg {
    pub id: &'static str,
    pub subject: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub day: i64,
    pub refs: Vec<&'static str>,
    pub msgid: bool,
}

pub fn msg(
    id: &'static str,
    subject: &'static str,
    from: &'static str,
    to: &'static str,
    day: i64,
) -> Msg {
    Msg {
        id,
        subject,
        from,
        to,
        day,
        refs: Vec::new(),
        msgid: true,
    }
}

impl Msg {
    pub fn refs(mut self, refs: &[&'static str]) -> Msg {
        self.refs = refs.to_vec();
        self
    }

    pub fn no_msgid(mut self) -> Msg {
        self.msgid = false;
        self
    }

    pub fn fetched(&self, uid: u32) -> Fetched {
        self.fetched_with(uid, Flags::empty(), KeywordSet::new())
    }

    pub fn fetched_with(&self, uid: u32, flags: Flags, keywords: KeywordSet) -> Fetched {
        Fetched {
            location: LocationState {
                uid,
                flags,
                keywords,
                modseq: None,
            },
            message: Some(NewMessage {
                email_id: EmailId::from_object_id(self.id).unwrap(),
                msgid: self.msgid.then(|| format!("{}@test", self.id)),
                in_reply_to: self.refs.last().map(|r| format!("{r}@test")),
                refs: self.refs.iter().map(|r| format!("{r}@test")).collect(),
                date: 1_700_000_000 + self.day * 86_400,
                received: 1_700_000_000 + self.day * 86_400,
                subject: Some(self.subject.to_owned()),
                subject_norm: normalize_subject(self.subject),
                from: vec![addr(self.from)],
                to: vec![addr(self.to)],
                cc: Vec::new(),
                snippet: None,
                size: 100,
                has_attachment: false,
                has_calendar: false,
            }),
        }
    }
}
