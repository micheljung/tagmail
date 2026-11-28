//! Transport-agnostic API: handlers over a [`Store`], the push channel over
//! an [`EventSource`], and the DTOs shared with the TypeScript client.
//! Used by `tagmail-server` (axum) and the desktop app (custom protocol).

pub mod api;
pub mod auth_dto;
pub mod caldav;
pub mod calendar_dto;
pub mod compose;
pub mod compose_dto;
pub mod desktop_dto;
pub mod dto;
pub mod events;
pub mod id_index;
pub mod identities;
mod invitation;
#[cfg(feature = "calendar")]
pub mod itip_auth;
pub mod messages;
pub mod mime;
pub mod ops_dto;
pub mod push_dto;
pub mod replica_dto;
pub mod routes;
pub mod signing;
#[cfg(feature = "sqlite")]
mod sqlite_calendar;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;
pub mod store;
pub mod ts;
pub mod writes;

pub use api::{API_VERSION, Api, Method, PARTIAL_HEADER, Request, Response, offline_error};
pub use caldav::{CalAction, CalRequest, CalendarError, CalendarService};
pub use events::{Arrival, ChangeFeed, Event, EventSession, EventSource, OpenError, ReminderPush};
pub use store::{
    AccountPatch, AccountRegistry, AccountSummary, BoxFuture, CaldavSettingsUpdate, CalendarPatch,
    ChangePage, FeatureDisabled, FetchRawError, ImageProxy, InvitationContext, MarkDeleting,
    MessageRecord, NewAccount, ProxiedImage, ProxyError, RuntimeStatus, Secret, Store, StoreError,
    StoreResult, StoredCopy, ThreadDetail, ThreadRows, View, ViewError, ViewIndex,
};
pub use writes::{OpMailbox, OpRequest, Submitted, WriteError, WriteResult, WriteService};
