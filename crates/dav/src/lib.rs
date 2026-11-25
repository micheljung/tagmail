//! Thin WebDAV/CalDAV client: discovery, sync reports and conditional
//! writes over reqwest, with streaming multistatus parsing and manual,
//! host-checked redirects.

#[cfg(test)]
macro_rules! fixture {
    ($path:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $path
        ))
    };
}

mod client;
mod discover;
#[cfg(test)]
mod discovery_tests;
mod env;
mod error;
mod host;
mod href;
#[cfg(test)]
mod mock;
mod model;
#[cfg(test)]
mod quirks_tests;
mod xml;

pub use client::{Client, MULTIGET_BATCH};
pub use discover::discover;
pub use env::DiscoveryEnv;
pub use error::{DavError, Result, UidConflictKind};
pub use host::{host_allowed, registrable_domain};
pub use href::{normalize_href, same_resource};
pub use model::{
    CalendarInfo, ClientOptions, DiscoveredAccount, Discovery, DiscoveryTarget, MultigetItem,
    PutCondition, Resource, ResourceRef, Scheduling, SchedulingMode, SyncPage, UidMatch,
};
