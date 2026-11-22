//! Tagmail storage: durable `config.db`, disposable `cache.db` (both SQLite,
//! WAL, one writer actor plus a small reader pool each), the change log and
//! full-text search.

pub mod changes;
mod db;
pub mod error;
pub mod fts;
mod migrations;
mod readers;
mod writer;

pub use changes::{Change, ChangeOp, Changes, Entity, NewChange};
pub use db::{CacheDb, ConfigDb, Database};
pub use error::{Error, Result};
pub use fts::FtsDocument;
pub use rusqlite;
