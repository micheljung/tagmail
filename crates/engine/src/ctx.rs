//! What one account's sync needs from the outside: the cache, the blob
//! store and the change feed it publishes to after each commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tagmail_api_core::ChangeFeed;
use tagmail_core::AccountId;
use tagmail_store::rusqlite::Connection;
use tagmail_store::{BlobStore, CacheDb, changes};
use tokio::sync::Semaphore;

use crate::cache::W;
use crate::error::Result;
use crate::overlay::Overlays;

/// Blob writes in flight at once, over all accounts.
pub const MAX_BLOB_WRITES: usize = 1024;

/// A semaphore for [`Ctx::with_blob_writes`].
pub fn blob_writes() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_BLOB_WRITES))
}

/// Waits until no blob write counted in `writes` runs.
pub async fn drain_blob_writes(writes: &Semaphore) {
    let all = u32::try_from(MAX_BLOB_WRITES).unwrap_or(u32::MAX);
    drop(writes.acquire_many(all).await);
}

#[derive(Clone)]
pub struct Ctx {
    pub cache: Arc<CacheDb>,
    pub blobs: Arc<BlobStore>,
    pub feed: Arc<ChangeFeed>,
    pub account: AccountId,
    pub overlays: Arc<Overlays>,
    blob_writes: Arc<Semaphore>,
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
            overlays: Arc::default(),
            blob_writes: Arc::new(Semaphore::new(MAX_BLOB_WRITES)),
            clock: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Counts blob writes in `writes`, so a stop can wait for them.
    pub fn with_blob_writes(mut self, writes: Arc<Semaphore>) -> Ctx {
        self.blob_writes = writes;
        self
    }

    /// Runs a blocking blob write to the end even when the awaiting sync
    /// step is cancelled; [`drain_blob_writes`] waits for it.
    pub async fn write_blobs<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> tagmail_store::Result<T> + Send + 'static,
    {
        let permit = self
            .blob_writes
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| tagmail_store::Error::Closed)?;
        Ok(tokio::task::spawn_blocking(move || {
            let out = f();
            drop(permit);
            out
        })
        .await
        .map_err(|_| tagmail_store::Error::Closed)??)
    }

    /// Projects the pending ops in `overlays` over every write.
    pub fn with_overlays(mut self, overlays: Arc<Overlays>) -> Ctx {
        self.overlays = overlays;
        self
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
        let overlay = self.overlays.get(self.account);
        let (out, head) = self
            .cache
            .write(move |tx| {
                let mut w = W::new(tx, account, now).with_overlay(overlay);
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;
    use crate::testutil::open;

    #[tokio::test]
    async fn drain_waits_for_a_blob_write_whose_caller_was_cancelled() {
        let t = open();
        let writes = blob_writes();
        let ctx = t.ctx.clone().with_blob_writes(writes.clone());
        let done = Arc::new(AtomicBool::new(false));
        let (started, running) = std::sync::mpsc::channel();
        let flag = done.clone();
        let write = tokio::spawn(async move {
            ctx.write_blobs(move || {
                let _ = started.send(());
                std::thread::sleep(Duration::from_millis(200));
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
        });
        tokio::task::spawn_blocking(move || running.recv().unwrap())
            .await
            .unwrap();
        write.abort();
        drain_blob_writes(&writes).await;
        assert!(
            done.load(Ordering::SeqCst),
            "drained before the write ended"
        );
    }
}
