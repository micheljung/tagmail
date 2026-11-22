//! Per-account connection budget with authentication backoff.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::session::{ImapSession, ServerConfig, TokioSession};

/// Opens authenticated sessions; implemented by [`ServerConfig`].
pub trait Connector: Send + Sync + 'static {
    type Session: ImapSession + 'static;

    fn connect(&self) -> impl Future<Output = Result<Self::Session>> + Send;
}

impl Connector for ServerConfig {
    type Session = TokioSession;

    fn connect(&self) -> impl Future<Output = Result<TokioSession>> + Send {
        TokioSession::connect(self)
    }
}

pub const DEFAULT_MAX_CONNECTIONS: usize = 2;
pub const MAX_CONNECTIONS: usize = 4;

#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Clamped to `1..=MAX_CONNECTIONS`.
    pub max_connections: usize,
    /// Wait after the first authentication failure; doubles per failure.
    pub auth_backoff_base: Duration,
    pub auth_backoff_max: Duration,
    /// A pooled session idle for longer is checked with NOOP before reuse.
    pub check_after: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            auth_backoff_base: Duration::from_secs(30),
            auth_backoff_max: Duration::from_secs(60 * 60),
            check_after: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Debug, Default)]
struct AuthState {
    failures: u32,
    until: Option<Instant>,
    last_error: Option<String>,
}

struct Inner<C: Connector> {
    connector: C,
    config: PoolConfig,
    permits: Arc<Semaphore>,
    idle: Mutex<Vec<(C::Session, Instant)>>,
    auth: Mutex<AuthState>,
}

/// Hands out at most `max_connections` sessions for one account and stops
/// logging in after authentication failures, backing off exponentially so
/// a wrong password never locks the account at the provider.
pub struct Pool<C: Connector> {
    inner: Arc<Inner<C>>,
}

impl<C: Connector> Clone for Pool<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<C: Connector> Pool<C> {
    pub fn new(connector: C, mut config: PoolConfig) -> Self {
        config.max_connections = config.max_connections.clamp(1, MAX_CONNECTIONS);
        Self {
            inner: Arc::new(Inner {
                connector,
                permits: Arc::new(Semaphore::new(config.max_connections)),
                config,
                idle: Mutex::new(Vec::new()),
                auth: Mutex::new(AuthState::default()),
            }),
        }
    }

    pub fn connector(&self) -> &C {
        &self.inner.connector
    }

    pub fn max_connections(&self) -> usize {
        self.inner.config.max_connections
    }

    /// Remaining authentication backoff and the server's last message.
    pub fn auth_backoff(&self) -> Option<(Duration, Option<String>)> {
        let auth = self
            .inner
            .auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let remaining = auth.until?.checked_duration_since(Instant::now())?;
        Some((remaining, auth.last_error.clone()))
    }

    /// Clears the backoff, e.g. after the credentials were changed.
    pub fn reset_auth_backoff(&self) {
        *self
            .inner
            .auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = AuthState::default();
    }

    fn check_backoff(&self) -> Result<()> {
        match self.auth_backoff() {
            Some((retry_in, _)) => Err(Error::AuthBackoff { retry_in }),
            None => Ok(()),
        }
    }

    /// Waits for a free slot, then reuses an idle session or logs in.
    pub async fn acquire(&self) -> Result<Pooled<C>> {
        self.check_backoff()?;
        let permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        loop {
            let reused = self
                .inner
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            let Some((mut session, since)) = reused else {
                break;
            };
            if since.elapsed() >= self.inner.config.check_after && session.noop().await.is_err() {
                continue;
            }
            if session.is_usable() {
                return Ok(self.pooled(session, permit));
            }
        }
        self.check_backoff()?;
        match self.inner.connector.connect().await {
            Ok(session) => {
                self.reset_auth_backoff();
                Ok(self.pooled(session, permit))
            }
            Err(e) => {
                if let Error::Auth(text) = &e {
                    self.record_auth_failure(text);
                }
                Err(e)
            }
        }
    }

    fn record_auth_failure(&self, text: &str) {
        let config = &self.inner.config;
        let mut auth = self
            .inner
            .auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        auth.failures = auth.failures.saturating_add(1);
        let factor = 1u32 << (auth.failures - 1).min(16);
        let wait = config
            .auth_backoff_base
            .saturating_mul(factor)
            .min(config.auth_backoff_max);
        auth.until = Some(Instant::now() + wait);
        auth.last_error = Some(text.to_owned());
        tracing::warn!(
            failures = auth.failures,
            ?wait,
            "IMAP authentication failed, backing off"
        );
    }

    fn pooled(&self, session: C::Session, permit: OwnedSemaphorePermit) -> Pooled<C> {
        Pooled {
            session: Some(session),
            _permit: permit,
            pool: self.inner.clone(),
        }
    }
}

/// A session checked out of a [`Pool`]; returned on drop while usable.
pub struct Pooled<C: Connector> {
    session: Option<C::Session>,
    _permit: OwnedSemaphorePermit,
    pool: Arc<Inner<C>>,
}

impl<C: Connector> Pooled<C> {
    /// Closes the connection instead of returning it to the pool.
    pub fn discard(mut self) {
        self.session = None;
    }
}

impl<C: Connector> Deref for Pooled<C> {
    type Target = C::Session;

    fn deref(&self) -> &C::Session {
        self.session.as_ref().expect("present until drop")
    }
}

impl<C: Connector> DerefMut for Pooled<C> {
    fn deref_mut(&mut self) -> &mut C::Session {
        self.session.as_mut().expect("present until drop")
    }
}

impl<C: Connector> Drop for Pooled<C> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && session.is_usable()
        {
            self.pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((session, Instant::now()));
        }
    }
}
