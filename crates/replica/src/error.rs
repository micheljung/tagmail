use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] tagmail_store::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Client(#[from] crate::client::ClientError),
    #[error("durable schema version {found} is newer than the supported {supported}")]
    DurableTooNew { found: i64, supported: i64 },
    #[error("corrupt replica data: {0}")]
    Corrupt(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
