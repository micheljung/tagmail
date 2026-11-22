use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Core(#[from] tagmail_core::Error),
    #[error("database connection is closed")]
    Closed,
    #[error("a reader panicked")]
    ReaderPanicked,
    #[error("database schema version {found} is newer than the supported {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("blob exceeds the {cap}-byte cap")]
    BlobTooLarge { cap: u64 },
    #[error("corrupt store data: {0}")]
    Corrupt(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
