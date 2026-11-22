//! Tagmail domain types and pure logic shared by the engine, store and API.

pub mod email_id;
pub mod error;
pub mod flags;
pub mod ids;
pub mod label;
pub mod mailbox_name;
pub mod query;

pub use email_id::{EmailId, EmailIdSource, HeaderIdentity};
pub use error::{Error, Result};
pub use flags::{Flags, Keyword, KeywordSet, split_imap_flags};
pub use ids::{
    AccountId, CacheEpoch, ChangeCursor, MailboxRef, MailboxRowId, MessageRowId, SpecialUse,
    ThreadId,
};
pub use label::{
    DEFAULT_KEYWORD_LIMIT, KeywordClass, LABEL_PREFIX, LabelKey, canonical_label_keyword,
    classify_keyword, decode_label, encode_label,
};
pub use mailbox_name::decode_mailbox_name;
