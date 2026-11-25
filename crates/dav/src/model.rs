use std::fmt;

use url::Url;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct ClientOptions {
    pub username: String,
    pub password: Zeroizing<String>,
    pub tls: rustls::ClientConfig,
    /// Permits `http` URLs and https→http redirects (`caldav.allow_insecure`).
    pub allow_insecure: bool,
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientOptions")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("allow_insecure", &self.allow_insecure)
            .finish_non_exhaustive()
    }
}

/// What discovery starts from.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryTarget<'a> {
    pub email: &'a str,
    pub imap_host: &'a str,
    pub manual_url: Option<&'a Url>,
    /// A host the owner confirmed earlier via `/confirm-host`.
    pub confirmed_host: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub enum Discovery {
    Found(Box<DiscoveredAccount>),
    /// The server lives under a registrable domain other than the IMAP
    /// host's; nothing authenticated was sent.
    NeedsConfirmation {
        host: String,
        url: Url,
    },
}

#[derive(Debug, Clone)]
pub struct DiscoveredAccount {
    /// The host credentials are anchored to (`caldav.confirmed_host`).
    pub host: String,
    pub context_url: Url,
    pub principal: Url,
    pub home: Url,
    pub scheduling: Scheduling,
    pub calendars: Vec<CalendarInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scheduling {
    /// `calendar-auto-schedule` in the home's `DAV` header.
    pub auto_schedule: bool,
    pub inbox: Option<Url>,
    pub outbox: Option<Url>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulingMode {
    Server,
    Client,
}

impl Scheduling {
    /// RFC 6638 server scheduling needs the header and both mailboxes.
    pub fn detected(&self) -> SchedulingMode {
        if self.auto_schedule && self.inbox.is_some() && self.outbox.is_some() {
            SchedulingMode::Server
        } else {
            SchedulingMode::Client
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInfo {
    pub href: Url,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub supports_sync_collection: bool,
    /// `supported-calendar-component-set`; `None` when the server omits it.
    pub components: Option<Vec<String>>,
}

impl CalendarInfo {
    pub fn supports_events(&self) -> bool {
        self.components
            .as_ref()
            .is_none_or(|c| c.iter().any(|c| c == "VEVENT"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRef {
    pub href: Url,
    pub etag: Option<String>,
}

/// One `sync-collection` answer. When `truncated` (507), call again with
/// `sync_token` to get the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPage {
    pub sync_token: Option<String>,
    pub changed: Vec<ResourceRef>,
    pub removed: Vec<Url>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub href: Url,
    pub etag: Option<String>,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultigetItem {
    Found(Resource),
    Missing(Url),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutCondition {
    /// `If-None-Match: *`.
    Create,
    /// `If-Match: <etag>`.
    IfMatch(String),
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidMatch {
    pub calendar: Url,
    pub resource: Resource,
}
