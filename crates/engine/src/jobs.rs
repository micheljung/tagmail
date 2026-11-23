//! The account worker's job queue: IMAP work other parts of the engine hand
//! to an account's worker connection (on-open fetches, op batches, drafts,
//! Sent copies). The worker serves queued jobs between sync steps, highest
//! [`Priority`] first; submitting wakes it from its wait or inline IDLE and
//! pauses non-INBOX initial sync after the current window. While the
//! account reconnects or backs off, queued jobs are answered
//! [`crate::Error::Unavailable`] without running.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use crate::ctx::Ctx;
use crate::error::Result;

/// The future a job returns; it may borrow the session and the context.
pub type JobFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Serving order, highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Someone waits on screen (opening a message).
    Interactive = 0,
    /// The player's writes: ops, drafts, Sent copies.
    Write = 1,
    /// Housekeeping that may wait behind everything else.
    Background = 2,
}

const LEVELS: usize = 3;
/// Jobs queued per account; more are refused as unavailable.
pub const MAX_QUEUED: usize = 256;

/// A job with its result channel erased: called with the session, or with
/// the reason it cannot run.
pub(crate) type Erased<S> = Box<
    dyn for<'a> FnOnce(
            std::result::Result<(&'a mut S, &'a Ctx), String>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send,
>;

pub(crate) struct Jobs<S> {
    queue: Mutex<[VecDeque<Erased<S>>; LEVELS]>,
    /// Wakes the worker from its wait or inline IDLE.
    pub(crate) wake: Arc<Notify>,
    /// Jobs are queued; pauses non-INBOX initial sync between windows.
    pub(crate) pending: Arc<AtomicBool>,
    /// Poll every mailbox at the next opportunity.
    pub(crate) poll_now: AtomicBool,
}

impl<S> Jobs<S> {
    pub(crate) fn new() -> Jobs<S> {
        Jobs {
            queue: Mutex::new(std::array::from_fn(|_| VecDeque::new())),
            wake: Arc::new(Notify::new()),
            pending: Arc::new(AtomicBool::new(false)),
            poll_now: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, [VecDeque<Erased<S>>; LEVELS]> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `job` and wakes the worker; gives it back when the queue is
    /// full.
    pub(crate) fn submit(&self, priority: Priority, job: Erased<S>) -> Result<(), Erased<S>> {
        {
            let mut queue = self.lock();
            if queue.iter().map(VecDeque::len).sum::<usize>() >= MAX_QUEUED {
                return Err(job);
            }
            queue[priority as usize].push_back(job);
        }
        self.pending.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        Ok(())
    }

    /// The next job, highest priority first.
    pub(crate) fn pop(&self) -> Option<Erased<S>> {
        self.lock().iter_mut().find_map(VecDeque::pop_front)
    }

    /// Answers every queued job with `why` without running it.
    pub(crate) async fn refuse_all(&self, why: &str) {
        while let Some(job) = self.pop() {
            job(Err(why.to_owned())).await;
        }
    }
}

/// Wraps `job` so that its result, or the reason it could not run, goes to
/// the returned receiver.
pub(crate) fn erase<S, T, J>(job: J) -> (Erased<S>, tokio::sync::oneshot::Receiver<Result<T>>)
where
    S: Send + 'static,
    T: Send + 'static,
    J: for<'a> FnOnce(&'a mut S, &'a Ctx) -> JobFuture<'a, T> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let erased: Erased<S> = Box::new(move |arg| {
        Box::pin(async move {
            let result = match arg {
                Ok((session, ctx)) => job(session, ctx).await,
                Err(why) => Err(crate::Error::Unavailable(why)),
            };
            let _ = tx.send(result);
        })
    });
    (erased, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(log: &Arc<Mutex<Vec<u32>>>, n: u32) -> Erased<()> {
        let log = log.clone();
        Box::new(move |arg| {
            Box::pin(async move {
                if arg.is_ok() {
                    log.lock().unwrap().push(n);
                }
            })
        })
    }

    #[tokio::test]
    async fn serves_by_priority_then_fifo_and_caps_the_queue() {
        let jobs = Jobs::<()>::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::testutil::open().ctx;
        for (p, n) in [
            (Priority::Background, 1),
            (Priority::Write, 2),
            (Priority::Interactive, 3),
            (Priority::Write, 4),
        ] {
            assert!(jobs.submit(p, job(&log, n)).is_ok());
        }
        assert!(jobs.pending.load(Ordering::SeqCst));
        while let Some(j) = jobs.pop() {
            j(Ok((&mut (), &ctx))).await;
        }
        assert_eq!(*log.lock().unwrap(), [3, 2, 4, 1]);

        for _ in 0..MAX_QUEUED {
            assert!(jobs.submit(Priority::Background, job(&log, 0)).is_ok());
        }
        assert!(jobs.submit(Priority::Interactive, job(&log, 0)).is_err());
        jobs.refuse_all("stopped").await;
        assert!(jobs.pop().is_none());
    }

    #[tokio::test]
    async fn refused_jobs_report_the_reason() {
        let jobs = Jobs::<()>::new();
        let (erased, rx) = erase::<(), u8, _>(|_, _| Box::pin(async { Ok(7) }));
        assert!(jobs.submit(Priority::Write, erased).is_ok());
        jobs.refuse_all("reconnecting").await;
        match rx.await.unwrap() {
            Err(crate::Error::Unavailable(why)) => assert_eq!(why, "reconnecting"),
            other => panic!("{other:?}"),
        }
    }
}
