//! IMAP-level value types shared by the session API.
//!
//! These types never expose `imap-types`, so the engine does not depend on
//! the codec.

use std::fmt;

use tagmail_core::SpecialUse;

/// A message UID, unique within one mailbox and UIDVALIDITY.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uid(pub u32);

/// The UIDVALIDITY of a mailbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UidValidity(pub u32);

/// A CONDSTORE mod-sequence value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModSeq(pub u64);

/// A set of UIDs stored as sorted, merged, inclusive ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UidSet {
    ranges: Vec<(u32, u32)>,
}

impl UidSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `first..=last`; zero is ignored because it is not a valid UID.
    pub fn insert_range(&mut self, first: u32, last: u32) {
        let (mut lo, mut hi) = if first <= last {
            (first, last)
        } else {
            (last, first)
        };
        lo = lo.max(1);
        if hi == 0 {
            return;
        }
        let start = self
            .ranges
            .partition_point(|&(_, e)| e.saturating_add(1) < lo);
        let mut end = start;
        while end < self.ranges.len() && self.ranges[end].0 <= hi.saturating_add(1) {
            lo = lo.min(self.ranges[end].0);
            hi = hi.max(self.ranges[end].1);
            end += 1;
        }
        self.ranges.splice(start..end, [(lo, hi)]);
    }

    pub fn insert(&mut self, uid: Uid) {
        self.insert_range(uid.0, uid.0);
    }

    pub fn extend(&mut self, other: &UidSet) {
        for &(lo, hi) in &other.ranges {
            self.insert_range(lo, hi);
        }
    }

    pub fn contains(&self, uid: Uid) -> bool {
        let i = self.ranges.partition_point(|&(_, e)| e < uid.0);
        i < self.ranges.len() && self.ranges[i].0 <= uid.0
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.ranges
            .iter()
            .map(|&(lo, hi)| u64::from(hi - lo) + 1)
            .sum()
    }

    /// Inclusive ranges in ascending order.
    pub fn ranges(&self) -> &[(u32, u32)] {
        &self.ranges
    }

    pub fn min(&self) -> Option<Uid> {
        self.ranges.first().map(|&(lo, _)| Uid(lo))
    }

    pub fn max(&self) -> Option<Uid> {
        self.ranges.last().map(|&(_, hi)| Uid(hi))
    }

    /// UIDs in ascending order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Uid> + '_ {
        self.ranges.iter().flat_map(|&(lo, hi)| (lo..=hi).map(Uid))
    }

    /// The UIDs of `self` that are not in `other`.
    pub fn difference(&self, other: &UidSet) -> UidSet {
        let mut out = UidSet::new();
        for &(lo, hi) in &self.ranges {
            let mut cursor = u64::from(lo);
            for &(olo, ohi) in &other.ranges {
                if u64::from(ohi) < cursor || olo > hi {
                    continue;
                }
                if u64::from(olo) > cursor {
                    out.ranges.push((cursor as u32, olo - 1));
                }
                cursor = u64::from(ohi) + 1;
            }
            if cursor <= u64::from(hi) {
                out.ranges.push((cursor as u32, hi));
            }
        }
        out
    }
}

impl FromIterator<Uid> for UidSet {
    fn from_iter<I: IntoIterator<Item = Uid>>(iter: I) -> Self {
        let mut uids: Vec<u32> = iter.into_iter().map(|u| u.0).collect();
        uids.sort_unstable();
        let mut set = UidSet::new();
        for uid in uids {
            set.insert_range(uid, uid);
        }
        set
    }
}

/// IMAP sequence-set syntax, e.g. `1:3,5`.
impl fmt::Display for UidSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &(lo, hi)) in self.ranges.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            if lo == hi {
                write!(f, "{lo}")?;
            } else {
                write!(f, "{lo}:{hi}")?;
            }
        }
        Ok(())
    }
}

/// A message flag or keyword. `\Recent` is never reported.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
    /// A keyword such as `$label_Work`.
    Keyword(String),
    /// A non-standard system flag such as `\XFoo`, including the backslash.
    Extension(String),
}

impl Flag {
    /// Parses one flag as it appears on the wire; `None` for `\Recent` and `\*`.
    pub fn parse(s: &str) -> Option<Flag> {
        let Some(name) = s.strip_prefix('\\') else {
            return Some(Flag::Keyword(s.to_owned()));
        };
        Some(match name.to_ascii_lowercase().as_str() {
            "seen" => Flag::Seen,
            "answered" => Flag::Answered,
            "flagged" => Flag::Flagged,
            "deleted" => Flag::Deleted,
            "draft" => Flag::Draft,
            "recent" | "*" => return None,
            _ => Flag::Extension(s.to_owned()),
        })
    }

    pub fn as_imap(&self) -> &str {
        match self {
            Flag::Seen => "\\Seen",
            Flag::Answered => "\\Answered",
            Flag::Flagged => "\\Flagged",
            Flag::Deleted => "\\Deleted",
            Flag::Draft => "\\Draft",
            Flag::Keyword(k) | Flag::Extension(k) => k,
        }
    }
}

/// One LIST result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInfo {
    /// The mailbox name as sent on the wire (modified UTF-7 unless UTF8=ACCEPT).
    pub name: String,
    pub delimiter: Option<char>,
    /// Attributes such as `\HasNoChildren` or `\Sent`.
    pub attributes: Vec<String>,
    pub special_use: Option<SpecialUse>,
    /// False for `\Noselect` and `\NonExistent` mailboxes.
    pub selectable: bool,
}

/// QRESYNC parameters for SELECT (RFC 7162 §3.2.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QresyncParams {
    pub uid_validity: UidValidity,
    pub modseq: ModSeq,
    pub known_uids: Option<UidSet>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectOptions {
    /// EXAMINE instead of SELECT.
    pub read_only: bool,
    /// Ignored unless QRESYNC was enabled on this connection.
    pub qresync: Option<QresyncParams>,
}

/// The state reported by SELECT or EXAMINE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedMailbox {
    pub name: String,
    pub read_only: bool,
    pub uid_validity: UidValidity,
    pub uid_next: Option<Uid>,
    pub exists: u32,
    /// `None` without CONDSTORE or when the mailbox reports `NOMODSEQ`.
    pub highest_modseq: Option<ModSeq>,
    pub nomodseq: bool,
    pub flags: Vec<Flag>,
    pub permanent_flags: Vec<Flag>,
    /// `PERMANENTFLAGS` contains `\*`, so new keywords can be stored.
    pub keyword_capable: bool,
    /// `VANISHED (EARLIER)` from a QRESYNC select.
    pub vanished: UidSet,
    /// Messages changed since the QRESYNC modseq.
    pub changed: Vec<MessageMeta>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailboxStatus {
    pub messages: Option<u32>,
    pub uid_next: Option<Uid>,
    pub uid_validity: Option<UidValidity>,
    pub unseen: Option<u32>,
    pub highest_modseq: Option<ModSeq>,
}

/// An RFC 3501 envelope address. Names may contain RFC 2047 encoded words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Address {
    pub name: Option<Vec<u8>>,
    pub mailbox: Option<Vec<u8>>,
    pub host: Option<Vec<u8>>,
}

/// An RFC 3501 envelope with undecoded header values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Envelope {
    pub date: Option<Vec<u8>>,
    pub subject: Option<Vec<u8>>,
    pub from: Vec<Address>,
    pub sender: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub in_reply_to: Option<Vec<u8>>,
    pub message_id: Option<Vec<u8>>,
}

/// A leaf MIME part from BODYSTRUCTURE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartInfo {
    /// The IMAP section number, e.g. `1` or `2.1`.
    pub section: String,
    /// Lowercase `type/subtype`.
    pub mime_type: String,
    pub charset: Option<String>,
    /// Lowercase Content-Transfer-Encoding.
    pub encoding: String,
    pub size: u32,
    /// Lowercase disposition, e.g. `attachment`.
    pub disposition: Option<String>,
    pub filename: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodySummary {
    pub parts: Vec<PartInfo>,
}

impl BodySummary {
    pub fn has_attachment(&self) -> bool {
        self.parts.iter().any(|p| {
            p.disposition.as_deref() == Some("attachment")
                || (p.filename.is_some() && !p.mime_type.starts_with("text/"))
        })
    }

    /// The first `text/plain` part, else the first `text/html` part.
    pub fn text_part(&self) -> Option<&PartInfo> {
        let inline = |p: &&PartInfo| p.disposition.as_deref() != Some("attachment");
        self.parts
            .iter()
            .filter(inline)
            .find(|p| p.mime_type == "text/plain")
            .or_else(|| {
                self.parts
                    .iter()
                    .filter(inline)
                    .find(|p| p.mime_type == "text/html")
            })
    }
}

/// The attributes of one message returned by FETCH. Fields not requested
/// (or not supported by the server) are `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageMeta {
    pub uid: Option<Uid>,
    pub seq: u32,
    pub flags: Option<Vec<Flag>>,
    pub modseq: Option<ModSeq>,
    /// RFC 8474 EMAILID.
    pub email_id: Option<String>,
    /// RFC 8474 THREADID.
    pub thread_id: Option<String>,
    pub gmail_msgid: Option<u64>,
    pub gmail_thrid: Option<u64>,
    /// INTERNALDATE as Unix seconds.
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
    pub envelope: Option<Envelope>,
    pub body: Option<BodySummary>,
    /// `BODY[HEADER.FIELDS (...)]` as raw header lines.
    pub header_fields: Option<Vec<u8>>,
    /// The start of part 1 (`BODY[1]<0>`), still transfer-encoded.
    pub snippet: Option<Vec<u8>>,
    /// The codec could not parse this FETCH line; only UID, FLAGS, MODSEQ,
    /// sizes, object ids and `BODY[...]` literals were recovered.
    pub degraded: bool,
}

impl MessageMeta {
    /// The server-provided identity: EMAILID, else X-GM-MSGID. `None` means
    /// the engine falls back to the header hash.
    pub fn server_email_id(&self) -> Option<tagmail_core::EmailId> {
        self.email_id
            .as_deref()
            .and_then(|id| tagmail_core::EmailId::from_object_id(id).ok())
            .or_else(|| {
                self.gmail_msgid
                    .map(tagmail_core::EmailId::from_gmail_msgid)
            })
    }
}

/// `VANISHED` (RFC 7162): UIDs expunged; `earlier` for those reported
/// after a QRESYNC select or `VANISHED` fetch modifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vanished {
    pub earlier: bool,
    pub uids: UidSet,
}

/// Changes reported by the server outside of a command's own result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailboxEvent {
    Exists(u32),
    /// A sequence-number EXPUNGE (connections without QRESYNC).
    Expunge(u32),
    Vanished(Vanished),
    /// Unsolicited FETCH, usually a flag change by another client.
    Fetch(Box<MessageMeta>),
}

/// Result of `UID FETCH ... (CHANGEDSINCE ...)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub messages: Vec<MessageMeta>,
    /// Only filled when QRESYNC is enabled (`VANISHED` modifier).
    pub vanished: UidSet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOp {
    Add,
    Remove,
    Replace,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreResult {
    /// UIDs not changed because of `UNCHANGEDSINCE` (the `MODIFIED` code).
    pub modified: UidSet,
    /// FETCH responses the server sent back (with MODSEQ under CONDSTORE).
    pub updates: Vec<MessageMeta>,
}

/// UIDPLUS `COPYUID`: `source[i]` became `destination[i]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyUid {
    pub uid_validity: UidValidity,
    pub source: Vec<Uid>,
    pub destination: Vec<Uid>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveResult {
    pub copy_uid: Option<CopyUid>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppendResult {
    /// The new UID from `APPENDUID`.
    pub uid: Option<Uid>,
    /// The mailbox UIDVALIDITY from STATUS after the append. Dovecot has
    /// been seen reporting a different value in `APPENDUID` for a fresh
    /// INBOX, so the reported value is never used.
    pub uid_validity: Option<UidValidity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyFetch {
    /// Bytes written to the sink.
    pub bytes: u64,
    /// The section was longer than the cap and was cut there.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleStop {
    /// The server reported a change; see [`IdleResult::events`].
    Changed,
    /// The renewal timer fired before anything happened.
    Renew,
    /// The caller's wake signal fired.
    Woken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleResult {
    pub stop: IdleStop,
    pub events: Vec<MailboxEvent>,
}

/// Criteria for `UID SEARCH`. ESEARCH is never used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchCriteria {
    All,
    /// `HEADER <name> <value>` (substring match).
    Header {
        name: String,
        value: String,
    },
    /// RFC 8474 `EMAILID`; requires OBJECTID.
    EmailId(String),
    /// Messages with `\Deleted`.
    Deleted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(r: &[(u32, u32)]) -> UidSet {
        let mut s = UidSet::new();
        for &(a, b) in r {
            s.insert_range(a, b);
        }
        s
    }

    #[test]
    fn uid_set_merges_adjacent_and_overlapping_ranges() {
        let s = set(&[(5, 7), (1, 2), (3, 3), (10, 12), (6, 11)]);
        assert_eq!(s.ranges(), &[(1, 3), (5, 12)]);
        assert_eq!(s.to_string(), "1:3,5:12");
        assert_eq!(s.len(), 11);
        assert!(s.contains(Uid(5)) && !s.contains(Uid(4)));
    }

    #[test]
    fn uid_set_difference() {
        let a = set(&[(1, 10), (20, 25)]);
        let b = set(&[(3, 4), (10, 21), (25, 30)]);
        assert_eq!(a.difference(&b).ranges(), &[(1, 2), (5, 9), (22, 24)]);
        assert_eq!(a.difference(&UidSet::new()), a);
        assert!(b.difference(&set(&[(1, 100)])).is_empty());
    }

    #[test]
    fn flag_parse_round_trip() {
        assert_eq!(Flag::parse("\\SEEN"), Some(Flag::Seen));
        assert_eq!(Flag::parse("\\Recent"), None);
        assert_eq!(Flag::parse("$label_Work").unwrap().as_imap(), "$label_Work");
        assert_eq!(
            Flag::parse("\\XFoo"),
            Some(Flag::Extension("\\XFoo".into()))
        );
    }
}
