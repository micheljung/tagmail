//! What one account's sync needs from the outside: the cache, the blob
//! store and the change feed it publishes to after each commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tagmail_api_core::ChangeFeed;
use tagmail_core::AccountId;
use tagmail_store::rusqlite::Connection;
use tagmail_store::{BlobStore, CacheDb, changes};

use crate::cache::W;
use crate::error::Result;

#[derive(Clone)]
pub struct Ctx {
    pub cache: Arc<CacheDb>,
    pub blobs: Arc<BlobStore>,
    pub feed: Arc<ChangeFeed>,
    pub account: AccountId,
    clock: Arc<AtomicI64>,
}

fn wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Ctx {
    pub fn new(
        cache: Arc<CacheDb>,
        blobs: Arc<BlobStore>,
        feed: Arc<ChangeFeed>,
        account: AccountId,
    ) -> Ctx {
        Ctx {
            cache,
            blobs,
            feed,
            account,
            clock: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Strictly increasing milliseconds, so every transaction of the
    /// account has its own timestamp (tombstones compare against polls).
    fn tick(&self) -> i64 {
        let now = wall_ms();
        let mut last = self.clock.load(Ordering::SeqCst);
        loop {
            let next = now.max(last + 1);
            match self
                .clock
                .compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return next,
                Err(seen) => last = seen,
            }
        }
    }

    /// Runs `f` in one cache transaction and publishes the new change-log
    /// head if it appended changes.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut W<'_>) -> tagmail_store::Result<T> + Send + 'static,
    {
        let account = self.account.0;
        let now = self.tick();
        let epoch = self.cache.epoch();
        let (out, head) = self
            .cache
            .write(move |tx| {
                let mut w = W::new(tx, account, now);
                let out = f(&mut w)?;
                let head = if w.finish()? {
                    Some(changes::head(tx, epoch)?)
                } else {
                    None
                };
                Ok((out, head))
            })
            .await?;
        if let Some(head) = head {
            self.feed.publish(head);
        }
        Ok(out)
    }

    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> tagmail_store::Result<T> + Send + 'static,
    {
        Ok(self.cache.read(f).await?)
    }
}
