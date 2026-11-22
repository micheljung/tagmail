use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, TransactionBehavior};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};

/// Queued writes before `write` callers wait for capacity.
const QUEUE_CAPACITY: usize = 64;

/// Plan §3.4: a writer transaction holds the lock for at most one sync
/// window; longer ones are logged so callers can split them.
const TXN_BUDGET: Duration = Duration::from_millis(50);

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// The single writer of one database. Jobs run one at a time, in order, on a
/// dedicated thread so blocking SQLite calls never stall the async runtime.
pub(crate) struct Writer {
    jobs: mpsc::Sender<Job>,
    thread: JoinHandle<()>,
}

impl Writer {
    pub(crate) fn spawn(name: &str, conn: Connection) -> Result<Writer> {
        let (jobs, mut queue) = mpsc::channel::<Job>(QUEUE_CAPACITY);
        let thread = thread::Builder::new()
            .name(format!("tagmail-{name}-writer"))
            .spawn(move || {
                let mut conn = conn;
                while let Some(job) = queue.blocking_recv() {
                    job(&mut conn);
                }
            })?;
        Ok(Writer { jobs, thread })
    }

    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = reply.send(f(conn));
        });
        self.jobs.send(job).await.map_err(|_| Error::Closed)?;
        response.await.map_err(|_| Error::Closed)?
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction, committed when `f`
    /// returns `Ok` and rolled back otherwise.
    pub(crate) async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        self.call(move |conn| {
            let started = Instant::now();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            let elapsed = started.elapsed();
            if elapsed > TXN_BUDGET {
                tracing::warn!(?elapsed, budget = ?TXN_BUDGET, "slow writer transaction");
            }
            Ok(out)
        })
        .await
    }

    /// Waits for queued jobs to finish and closes the connection.
    pub(crate) async fn close(self) -> Result<()> {
        let Writer { jobs, thread } = self;
        drop(jobs);
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|_| Error::Closed)?
            .map_err(|_| Error::Closed)
    }
}
