//! Desktop replica of a Tagmail server (plan §3.6): local copies of the
//! server's cache and config, a never-wiped store for offline writes, and
//! the client that keeps them in sync.

pub mod client;
pub mod db;
pub mod durable;
pub mod error;
pub mod version;

pub use client::{ClientError, Link, ServerClient};
pub use db::{Discard, Expected, Layout, LocalDbs, Opened, ReplicaMeta};
pub use durable::Durable;
pub use error::{Error, Result};
pub use version::{Compat, MIN_SERVER};
