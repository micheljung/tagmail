//! Tagmail sync engine: keeps `cache.db` in step with each account's IMAP
//! server (plan §3.4) and publishes every committed change to the API's
//! change feed.

#![allow(dead_code)]

mod cache;
mod ctx;
mod error;
mod message;
#[cfg(test)]
mod testutil;
mod threading;

pub use error::{Error, Result};
