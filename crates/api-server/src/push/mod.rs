//! Web Push (plan §3.10): subscription routes, the dispatcher that turns
//! new mail into pushes, RFC 8291 payload encryption and RFC 8292 VAPID.

pub mod dispatch;
pub mod ece;
pub mod payload;
pub mod routes;
pub mod store;
pub mod vapid;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tagmail_store::{ConfigDb, Vault};
use tokio::sync::mpsc;
use url::Url;

use crate::outbound::{EndpointPolicy, Resolver, SystemResolver};
use crate::ratelimit::{Rate, RateLimiter};
use vapid::VapidStore;

/// Subscription writes per client: 10, then 10 a minute.
pub const PUSH_WRITES: Rate = Rate {
    burst: 10,
    refill: Duration::from_secs(6),
};
pub const PUSH_WRITES_GLOBAL: Rate = Rate {
    burst: 100,
    refill: Duration::from_millis(600),
};
/// Test notifications: one per subscription every 10 s.
pub const PUSH_TESTS: Rate = Rate {
    burst: 1,
    refill: Duration::from_secs(10),
};
pub const PUSH_TESTS_GLOBAL: Rate = Rate {
    burst: store::MAX_SUBSCRIPTIONS as u32,
    refill: Duration::from_millis(500),
};

#[derive(Clone)]
pub struct PushConfig {
    pub policy: EndpointPolicy,
    pub resolver: Arc<dyn Resolver>,
}

impl Default for PushConfig {
    fn default() -> PushConfig {
        PushConfig {
            policy: EndpointPolicy::Strict,
            resolver: Arc::new(SystemResolver),
        }
    }
}

impl fmt::Debug for PushConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushConfig")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// State shared by the routes and the dispatcher.
pub struct Push {
    db: Arc<ConfigDb>,
    vapid: VapidStore,
    policy: EndpointPolicy,
    resolver: Arc<dyn Resolver>,
    tls: rustls::ClientConfig,
    session_idle: Duration,
    tests: mpsc::Sender<i64>,
    writes: RateLimiter,
    test_limit: RateLimiter<i64>,
}

impl Push {
    /// Also returns the receiving end of test requests for
    /// [`dispatch::spawn`].
    pub fn new(
        config: &PushConfig,
        db: Arc<ConfigDb>,
        vault: Arc<Vault>,
        public_origin: Option<&Url>,
        tls: rustls::ClientConfig,
        session_idle: Duration,
    ) -> (Arc<Push>, mpsc::Receiver<i64>) {
        let (tests, test_rx) = mpsc::channel(store::MAX_SUBSCRIPTIONS as usize);
        let push = Push {
            vapid: VapidStore::new(db.clone(), vault, vapid::subject(public_origin)),
            db,
            policy: config.policy,
            resolver: config.resolver.clone(),
            tls,
            session_idle,
            tests,
            writes: RateLimiter::new(PUSH_WRITES, PUSH_WRITES_GLOBAL),
            test_limit: RateLimiter::new(PUSH_TESTS, PUSH_TESTS_GLOBAL),
        };
        (Arc::new(push), test_rx)
    }
}
