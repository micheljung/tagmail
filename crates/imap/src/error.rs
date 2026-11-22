use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("invalid TLS server name {0:?}")]
    InvalidServerName(String),
    #[error("connection closed{}", bye.as_ref().map(|b| format!(" by server: {b}")).unwrap_or_default())]
    Closed { bye: Option<String> },
    #[error("timed out")]
    Timeout,
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Login was refused for a temporary reason (`[UNAVAILABLE]`, `[LIMIT]`
    /// or a BYE greeting); retry later without counting it as an auth failure.
    #[error("server unavailable: {0}")]
    Unavailable(String),
    /// A previous authentication failure blocks new logins for this long.
    #[error("login blocked after authentication failures, retry in {retry_in:?}")]
    AuthBackoff { retry_in: Duration },
    #[error("server said NO: {text}")]
    No { code: Option<String>, text: String },
    #[error("server said BAD: {0}")]
    Bad(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("response larger than {limit} bytes")]
    ResponseTooLong { limit: u32 },
    #[error("not supported by the server: {0}")]
    Unsupported(&'static str),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("message not found")]
    NotFound,
    #[error("no mailbox selected")]
    NoMailboxSelected,
}

impl Error {
    /// The connection is unusable after this error and must be dropped.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::Io(_)
                | Error::Tls(_)
                | Error::Closed { .. }
                | Error::Timeout
                | Error::Protocol(_)
        )
    }

    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Auth(_))
    }
}
