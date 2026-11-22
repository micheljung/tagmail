use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Imap(#[from] tagmail_imap::Error),
    #[error(transparent)]
    Store(#[from] tagmail_store::Error),
    #[error("account {0} not found")]
    NoAccount(i64),
    #[error("invalid account settings: {0}")]
    InvalidAccount(String),
    #[error("stopped")]
    Stopped,
}

impl Error {
    /// The IMAP connection is unusable after this error.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Imap(e) if e.is_fatal())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
