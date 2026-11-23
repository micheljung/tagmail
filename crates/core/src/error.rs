use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    #[error("label name is empty")]
    EmptyLabel,
    #[error("encoded label keyword is {len} bytes, over the {limit}-byte limit")]
    LabelTooLong { len: usize, limit: usize },
    #[error("keyword {0:?} is not a $label_ keyword")]
    NotALabelKeyword(String),
    #[error("invalid =HH escape in label keyword {0:?}")]
    InvalidEscape(String),
    #[error("label keyword {0:?} does not decode to UTF-8")]
    InvalidUtf8(String),
    #[error("invalid IMAP keyword {0:?}")]
    InvalidKeyword(String),
    #[error("invalid OBJECTID {0:?}")]
    InvalidObjectId(String),
    #[error("invalid email id {0:?}")]
    InvalidEmailId(String),
    #[error("unknown special-use attribute {0:?}")]
    UnknownSpecialUse(String),
    #[error("invalid change cursor {0:?}")]
    InvalidCursor(String),
    #[error("unknown op kind {0:?}")]
    UnknownOpKind(String),
    #[error("unknown op state {0:?}")]
    UnknownOpState(String),
    #[error("unknown archive mode {0:?}")]
    UnknownArchiveMode(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
