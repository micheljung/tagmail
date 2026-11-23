//! Transport-agnostic API: handlers over a [`Store`], the push channel over
//! an [`EventSource`], and the DTOs shared with the TypeScript client.
//! Used by `tagmail-server` (axum) and the desktop app (custom protocol).

pub mod api;
pub mod auth_dto;
pub mod compose;
pub mod compose_dto;
pub mod dto;
pub mod events;
pub mod id_index;
pub mod identities;
pub mod messages;
pub mod mime;
pub mod ops_dto;
pub mod routes;
pub mod signing;
pub mod store;
pub mod ts;
pub mod writes;

pub use api::{API_VERSION, Api, Method, Request, Response};
pub use events::{ChangeFeed, Event, EventSession, EventSource, OpenError};
pub use store::{
    AccountPatch, AccountRegistry, AccountSummary, BoxFuture, ChangePage, FetchRawError,
    ImageProxy, MarkDeleting, MessageRecord, NewAccount, ProxiedImage, ProxyError, RuntimeStatus,
    Secret, Store, StoreError, StoreResult, ThreadDetail, ThreadRows, View, ViewError, ViewIndex,
};
pub use writes::{OpRequest, Submitted, WriteError, WriteResult, WriteService};
