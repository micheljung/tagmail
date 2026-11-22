//! Transport-agnostic API: handlers over a [`Store`], the push channel over
//! an [`EventSource`], and the DTOs shared with the TypeScript client.
//! Used by `tagmail-server` (axum) and the desktop app (custom protocol).

pub mod api;
pub mod auth_dto;
pub mod dto;
pub mod events;
pub mod id_index;
pub mod store;
pub mod ts;

pub use api::{API_VERSION, Api, Method, Request, Response};
pub use events::{ChangeFeed, Event, EventSession, EventSource, OpenError};
pub use store::{
    AccountRegistry, AccountSummary, ChangePage, NewAccount, RuntimeStatus, Secret, Store,
    StoreError, StoreResult, ThreadRows, View, ViewError, ViewIndex,
};
