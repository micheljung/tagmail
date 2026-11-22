//! IMAP client for the Tagmail sync engine.
//!
//! The engine depends only on [`ImapSession`]. [`TokioSession`] implements
//! it on imap-next/imap-codec over tokio with rustls (ring). Notes:
//!
//! - FETCH attribute lists follow the server's capabilities
//!   ([`FetchPlan`]): no MODSEQ without CONDSTORE (or on `NOMODSEQ`
//!   mailboxes), EMAILID only with OBJECTID, X-GM-MSGID only on Gmail.
//! - imap-codec cannot encode or parse EMAILID/X-GM-MSGID, so those are
//!   fetched with a separate raw command whose responses are parsed by a
//!   small fallback parser (`rawfetch`), which also recovers UID, FLAGS and
//!   MODSEQ from FETCH lines the codec rejects (server quirks).
//! - ESEARCH is never used; `UID SEARCH ALL` is split into UID ranges.
//! - Bodies are fetched in 1 MiB `BODY.PEEK[...]<offset.size>` chunks and
//!   responses over [`MAX_RESPONSE_SIZE`] are refused, so memory stays
//!   bounded for any message size.

mod append;
mod caps;
mod chunked;
mod conn;
mod error;
mod fetch;
mod idle;
mod r#move;
mod pool;
mod qresync;
mod rawfetch;
mod session;
mod store;
mod tls;
mod types;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod testutil;

pub use caps::{
    Capabilities, CapabilityOverride, FetchAttr, FetchPlan, FetchSpec, IdentityAttr, SNIPPET_BYTES,
};
pub use error::{Error, Result};
pub use fetch::{WINDOW, windows_newest_first};
pub use pool::{Connector, DEFAULT_MAX_CONNECTIONS, MAX_CONNECTIONS, Pool, PoolConfig, Pooled};
pub use session::{
    BODY_CHUNK, DEFAULT_IDLE_RENEW, ImapSession, MAX_IDLE_RENEW, MAX_RESPONSE_SIZE, Password,
    SEARCH_CHUNK, ServerConfig, TokioSession,
};
pub use tagmail_core::SpecialUse;
pub use tls::{Security, client_config};
pub use types::*;
