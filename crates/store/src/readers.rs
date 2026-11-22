use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{Error, Result};

/// A fixed pool of read-only connections; `read` waits for a free one.
pub(crate) struct Readers {
    idle: Arc<Mutex<Vec<Connection>>>,
    permits: Arc<Semaphore>,
}

/// Returns the connection to the pool when the read ends, also when the
/// awaiting future was dropped. A connection whose reader panicked is
/// discarded together with its permit, shrinking the pool.
struct Lease {
    idle: Arc<Mutex<Vec<Connection>>>,
    conn: Option<Connection>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if thread::panicking() {
            if let Some(permit) = self.permit.take() {
                permit.forget();
            }
        } else if let Some(conn) = self.conn.take() {
            self.idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(conn);
        }
    }
}

impl Readers {
    pub(crate) fn new(conns: Vec<Connection>) -> Readers {
        Readers {
            permits: Arc::new(Semaphore::new(conns.len())),
            idle: Arc::new(Mutex::new(conns)),
        }
    }

    pub(crate) async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Closed)?;
        let idle = self.idle.clone();
        tokio::task::spawn_blocking(move || {
            let conn = idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
                .ok_or(Error::Closed)?;
            let lease = Lease {
                idle,
                conn: Some(conn),
                permit: Some(permit),
            };
            f(lease.conn.as_ref().ok_or(Error::Closed)?)
        })
        .await
        .map_err(|_| Error::ReaderPanicked)?
    }

    /// Runs `f` on every idle connection.
    pub(crate) async fn for_each_idle<F>(&self, f: F) -> Result<()>
    where
        F: Fn(&Connection) -> Result<()> + Send + 'static,
    {
        let idle = self.idle.clone();
        tokio::task::spawn_blocking(move || {
            let conns = idle.lock().unwrap_or_else(PoisonError::into_inner);
            conns.iter().try_for_each(f)
        })
        .await
        .map_err(|_| Error::ReaderPanicked)?
    }

    pub(crate) fn close(&self) {
        self.permits.close();
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}
