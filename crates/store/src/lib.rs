//! Tagmail storage: durable `config.db`, disposable `cache.db` (both SQLite,
//! WAL, one writer actor plus a small reader pool each), the change log,
//! full-text search, the message blob store and credential encryption.

pub mod blobs;
pub mod changes;
pub mod credentials;
mod db;
pub mod error;
pub mod fts;
mod migrations;
mod readers;
pub mod search;
mod writer;

pub use blobs::{BlobKey, BlobStore, DEFAULT_RAW_CAP, RawWriter, TEXT_CAP};
pub use changes::{Change, ChangeOp, Changes, Entity, NewChange};
pub use credentials::{CredentialError, MasterKey, Sealed, Vault};
pub use db::{CacheDb, ConfigDb, Database};
pub use error::{Error, Result};
pub use fts::FtsDocument;
pub use migrations::{CACHE_SCHEMA, CONFIG_SCHEMA};
pub use rusqlite;
