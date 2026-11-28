use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Account key in `config.db`. IMAP-stable: safe in durable tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub i64);

/// `cache.db` surrogate id of a thread. Never stored in durable tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub i64);

/// `cache.db` surrogate id of a message row. Never stored in durable tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageRowId(pub i64);

/// `cache.db` surrogate id of a mailbox row. Never stored in durable tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailboxRowId(pub i64);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// RFC 6154 special-use attributes (plus RFC 8457 `\Important`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecialUse {
    All,
    Archive,
    Drafts,
    Flagged,
    Important,
    Junk,
    Sent,
    Trash,
}

impl SpecialUse {
    pub const ALL: [SpecialUse; 8] = [
        SpecialUse::All,
        SpecialUse::Archive,
        SpecialUse::Drafts,
        SpecialUse::Flagged,
        SpecialUse::Important,
        SpecialUse::Junk,
        SpecialUse::Sent,
        SpecialUse::Trash,
    ];

    /// The IMAP attribute, e.g. `\Sent`.
    pub fn as_imap(self) -> &'static str {
        match self {
            SpecialUse::All => "\\All",
            SpecialUse::Archive => "\\Archive",
            SpecialUse::Drafts => "\\Drafts",
            SpecialUse::Flagged => "\\Flagged",
            SpecialUse::Important => "\\Important",
            SpecialUse::Junk => "\\Junk",
            SpecialUse::Sent => "\\Sent",
            SpecialUse::Trash => "\\Trash",
        }
    }
}

impl fmt::Display for SpecialUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_imap())
    }
}

impl FromStr for SpecialUse {
    type Err = Error;

    /// Accepts the attribute case-insensitively, with or without the backslash.
    fn from_str(s: &str) -> Result<Self> {
        let bare = s.strip_prefix('\\').unwrap_or(s);
        SpecialUse::ALL
            .into_iter()
            .find(|u| u.as_imap()[1..].eq_ignore_ascii_case(bare))
            .ok_or_else(|| Error::UnknownSpecialUse(s.to_owned()))
    }
}

/// Durable reference to a mailbox: what `config.db` stores instead of a
/// `cache.db` mailbox id. Resolved at execution time by special-use, then
/// name, then a unique uidvalidity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MailboxRef {
    pub name: String,
    pub special_use: Option<SpecialUse>,
    pub uidvalidity: u32,
}

/// Random 64-bit id of one `cache.db` incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheEpoch(pub u64);

impl fmt::Display for CacheEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for CacheEpoch {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let valid = s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit());
        valid
            .then(|| u64::from_str_radix(s, 16).ok())
            .flatten()
            .map(CacheEpoch)
            .ok_or_else(|| Error::InvalidCursor(s.to_owned()))
    }
}

/// State string `<cache_epoch>:<seq>`: what SSE event ids and `/changes`
/// cursors carry. `seq` is the last change-log entry the client has seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChangeCursor {
    pub epoch: CacheEpoch,
    pub seq: i64,
}

impl fmt::Display for ChangeCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.seq)
    }
}

impl FromStr for ChangeCursor {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = || Error::InvalidCursor(s.to_owned());
        let (epoch, seq) = s.split_once(':').ok_or_else(invalid)?;
        let canonical = seq.bytes().all(|b| b.is_ascii_digit())
            && !seq.is_empty()
            && (seq == "0" || !seq.starts_with('0'));
        if !canonical {
            return Err(invalid());
        }
        Ok(ChangeCursor {
            epoch: epoch.parse().map_err(|_| invalid())?,
            seq: seq.parse().map_err(|_| invalid())?,
        })
    }
}

/// Which change-log entities a reader sees: clients that never asked for
/// calendar data never receive its entities (appointments plan Decision e).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChangeFilter {
    #[default]
    Mail,
    WithCalendar,
}

impl ChangeFilter {
    /// The `include` query value that selects [`ChangeFilter::WithCalendar`].
    pub const INCLUDE_CALENDAR: &'static str = "calendar";

    pub fn calendar(self) -> bool {
        self == ChangeFilter::WithCalendar
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn special_use_parses_case_insensitively() {
        assert_eq!("\\sent".parse::<SpecialUse>(), Ok(SpecialUse::Sent));
        assert_eq!("ARCHIVE".parse::<SpecialUse>(), Ok(SpecialUse::Archive));
        assert!("\\Inbox".parse::<SpecialUse>().is_err());
        for u in SpecialUse::ALL {
            assert_eq!(u.as_imap().parse::<SpecialUse>(), Ok(u));
        }
    }

    #[test]
    fn cursor_formats_epoch_as_fixed_hex() {
        let c = ChangeCursor {
            epoch: CacheEpoch(0xab),
            seq: 42,
        };
        assert_eq!(c.to_string(), "00000000000000ab:42");
    }

    #[test]
    fn cursor_rejects_malformed() {
        for s in [
            "",
            "42",
            "00000000000000ab:",
            "00000000000000ab:-1",
            "00000000000000ab:+1",
            "00000000000000ab:01",
            "ab:1",
            "00000000000000zz:1",
        ] {
            assert!(s.parse::<ChangeCursor>().is_err(), "{s}");
        }
    }

    proptest! {
        #[test]
        fn cursor_round_trips(epoch: u64, seq in 0i64..) {
            let c = ChangeCursor { epoch: CacheEpoch(epoch), seq };
            prop_assert_eq!(c.to_string().parse::<ChangeCursor>(), Ok(c));
        }
    }
}
