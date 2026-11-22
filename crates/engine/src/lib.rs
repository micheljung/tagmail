//! Tagmail sync engine: keeps `cache.db` in step with each account's IMAP
//! server (plan §3.4) and publishes every committed change to the API's
//! change feed.

mod cache;
mod ctx;
mod error;
#[cfg(test)]
mod fake;
mod jobs;
mod message;
mod raw;
mod supervisor;
mod sync;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod testutil;
mod threading;

pub use error::{Error, Result};
pub use jobs::{JobFuture, MAX_QUEUED, Priority};
pub use supervisor::{
    AccountSettings, ConnectorFactory, Engine, EngineConfig, ImapConnectorFactory, Stores,
    load_account,
};
