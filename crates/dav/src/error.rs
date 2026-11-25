use url::Url;

/// Which CalDAV precondition rejected a write because another resource
/// already holds the UID (RFC 4791 §5.3.2.1, RFC 6638 §3.2.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidConflictKind {
    NoUidConflict,
    UniqueSchedulingObjectResource,
}

#[derive(Debug, thiserror::Error)]
pub enum DavError {
    #[error("CalDAV request failed: {0}")]
    Network(#[source] reqwest::Error),
    #[error("CalDAV server rejected the credentials")]
    Unauthorized,
    #[error("CalDAV server denied access to {url}")]
    Forbidden { url: Url },
    #[error("CalDAV resource not found: {url}")]
    NotFound { url: Url },
    #[error("CalDAV resource changed on the server (precondition failed)")]
    PreconditionFailed,
    #[error("another CalDAV resource already holds this UID")]
    UidConflict {
        kind: UidConflictKind,
        existing: Option<Url>,
    },
    #[error("CalDAV sync token is no longer valid")]
    InvalidSyncToken,
    #[error("CalDAV server answered {status} for {url}")]
    Status { status: u16, url: Url },
    #[error("CalDAV URL is not https: {url}")]
    InsecureUrl { url: Url },
    #[error("CalDAV server redirected to an unconfirmed host: {host}")]
    UnconfirmedHost { host: String },
    #[error("CalDAV server redirected more than {max} times")]
    TooManyRedirects { max: usize },
    #[error("CalDAV redirect without a valid Location")]
    BadRedirect,
    #[error("invalid CalDAV href {href:?}")]
    BadHref { href: String },
    #[error("invalid CalDAV XML: {0}")]
    Xml(String),
    #[error("CalDAV response exceeds {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("the CalDAV server does not support the {0} report")]
    ReportUnsupported(&'static str),
    #[error("no CalDAV calendar home found")]
    NoCalendarHome,
    #[error("no CalDAV server found")]
    NotDiscovered,
    #[error("invalid CalDAV client configuration: {0}")]
    Config(String),
}

impl From<quick_xml::Error> for DavError {
    fn from(e: quick_xml::Error) -> Self {
        DavError::Xml(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DavError>;
