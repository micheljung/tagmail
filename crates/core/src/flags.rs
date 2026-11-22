use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{BitOr, BitOrAssign};

use crate::error::{Error, Result};

/// IMAP system flags of one location, stored as `location.flags INT`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Flags(u8);

impl Flags {
    pub const SEEN: Flags = Flags(1);
    pub const FLAGGED: Flags = Flags(1 << 1);
    pub const ANSWERED: Flags = Flags(1 << 2);
    pub const DRAFT: Flags = Flags(1 << 3);
    pub const DELETED: Flags = Flags(1 << 4);

    const NAMES: [(Flags, &'static str); 5] = [
        (Flags::SEEN, "\\Seen"),
        (Flags::FLAGGED, "\\Flagged"),
        (Flags::ANSWERED, "\\Answered"),
        (Flags::DRAFT, "\\Draft"),
        (Flags::DELETED, "\\Deleted"),
    ];
    const MASK: u8 = 0b1_1111;

    pub const fn empty() -> Flags {
        Flags(0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Unknown bits are dropped.
    pub const fn from_bits(bits: u8) -> Flags {
        Flags(bits & Self::MASK)
    }

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Flags) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Flags) {
        self.0 &= !other.0;
    }

    /// The system flag named by `flag` (case-insensitive), if it is one we store.
    pub fn from_imap(flag: &str) -> Option<Flags> {
        Self::NAMES
            .into_iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(flag))
            .map(|(f, _)| f)
    }

    pub fn to_imap(self) -> Vec<&'static str> {
        Self::NAMES
            .into_iter()
            .filter(|(f, _)| self.contains(*f))
            .map(|(_, name)| name)
            .collect()
    }
}

impl BitOr for Flags {
    type Output = Flags;

    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0;
    }
}

/// An IMAP keyword (`flag-keyword = atom`). Equality, ordering and hashing
/// are ASCII case-insensitive as in RFC 9051; the server's spelling is kept.
#[derive(Debug, Clone)]
pub struct Keyword(String);

const ATOM_SPECIALS: &[u8] = b"(){ %*\"\\]";

impl Keyword {
    pub fn new(keyword: impl Into<String>) -> Result<Keyword> {
        let keyword = keyword.into();
        let valid = !keyword.is_empty()
            && keyword
                .bytes()
                .all(|b| (0x21..0x7f).contains(&b) && !ATOM_SPECIALS.contains(&b));
        if valid {
            Ok(Keyword(keyword))
        } else {
            Err(Error::InvalidKeyword(keyword))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq for Keyword {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for Keyword {}

impl Ord for Keyword {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.0.bytes().map(|b| b.to_ascii_lowercase());
        a.cmp(other.0.bytes().map(|b| b.to_ascii_lowercase()))
    }
}

impl PartialOrd for Keyword {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for Keyword {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for b in self.0.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
    }
}

/// Keywords of one location, canonical = sorted case-insensitively and
/// deduplicated; stored as `location.keywords TEXT`, space-separated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeywordSet(BTreeSet<Keyword>);

impl KeywordSet {
    pub fn new() -> KeywordSet {
        KeywordSet::default()
    }

    /// Keeps the first spelling seen of a case-insensitive duplicate.
    pub fn insert(&mut self, keyword: Keyword) -> bool {
        self.0.insert(keyword)
    }

    pub fn remove(&mut self, keyword: &Keyword) -> bool {
        self.0.remove(keyword)
    }

    pub fn contains(&self, keyword: &Keyword) -> bool {
        self.0.contains(keyword)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Keyword> {
        self.0.iter()
    }

    pub fn to_storage(&self) -> String {
        let mut out = String::new();
        for (i, k) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(k.as_str());
        }
        out
    }

    pub fn from_storage(stored: &str) -> Result<KeywordSet> {
        stored.split_ascii_whitespace().map(Keyword::new).collect()
    }
}

impl FromIterator<Keyword> for KeywordSet {
    fn from_iter<I: IntoIterator<Item = Keyword>>(iter: I) -> Self {
        let mut set = KeywordSet::new();
        for keyword in iter {
            set.insert(keyword);
        }
        set
    }
}

impl<'a> IntoIterator for &'a KeywordSet {
    type Item = &'a Keyword;
    type IntoIter = std::collections::btree_set::Iter<'a, Keyword>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Splits a FETCH `FLAGS` list into stored system flags and keywords.
/// `\Recent`, `\*` and unknown `\`-flags are dropped.
pub fn split_imap_flags<'a>(
    flags: impl IntoIterator<Item = &'a str>,
) -> Result<(Flags, KeywordSet)> {
    let mut system = Flags::empty();
    let mut keywords = KeywordSet::new();
    for flag in flags {
        if let Some(f) = Flags::from_imap(flag) {
            system |= f;
        } else if !flag.starts_with('\\') {
            keywords.insert(Keyword::new(flag)?);
        }
    }
    Ok((system, keywords))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn kw(s: &str) -> Keyword {
        Keyword::new(s).unwrap()
    }

    #[test]
    fn flag_bits_are_stable() {
        assert_eq!(Flags::SEEN.bits(), 1);
        assert_eq!(Flags::FLAGGED.bits(), 2);
        assert_eq!(Flags::ANSWERED.bits(), 4);
        assert_eq!(Flags::DRAFT.bits(), 8);
        assert_eq!(Flags::DELETED.bits(), 16);
        assert_eq!(Flags::from_bits(0xff).bits(), 0b1_1111);
    }

    #[test]
    fn flags_insert_remove_contains() {
        let mut f = Flags::SEEN | Flags::DRAFT;
        assert!(f.contains(Flags::SEEN));
        assert!(!f.contains(Flags::SEEN | Flags::FLAGGED));
        f.remove(Flags::SEEN);
        f.insert(Flags::FLAGGED);
        assert_eq!(f.to_imap(), vec!["\\Flagged", "\\Draft"]);
    }

    #[test]
    fn keyword_rejects_atom_specials_and_non_ascii() {
        for bad in [
            "", "a b", "a(b", "a]", "\\Seen", "a*", "a%", "a\"", "a{", "ä", "a\u{7f}",
        ] {
            assert!(Keyword::new(bad).is_err(), "{bad:?}");
        }
        assert!(Keyword::new("$label_Work=20Stuff").is_ok());
    }

    #[test]
    fn keyword_identity_is_ascii_case_insensitive() {
        assert_eq!(kw("$Junk"), kw("$junk"));
        let set: KeywordSet = [kw("Work"), kw("work"), kw("$Junk")].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert_eq!(set.to_storage(), "$Junk Work");
    }

    #[test]
    fn split_imap_flags_separates_system_flags() {
        let (f, k) =
            split_imap_flags(["\\seen", "\\Recent", "\\*", "\\Unknown", "$label_A", "Work"])
                .unwrap();
        assert_eq!(f, Flags::SEEN);
        assert_eq!(k.to_storage(), "$label_A Work");
    }

    fn keyword_strategy() -> impl Strategy<Value = Keyword> {
        "[!#$&'+-\\[^-z|}~]{1,12}".prop_map(|s| Keyword::new(s).unwrap())
    }

    proptest! {
        #[test]
        fn keyword_set_storage_round_trips(set in prop::collection::btree_set(keyword_strategy(), 0..8)) {
            let set: KeywordSet = set.into_iter().collect();
            let stored = set.to_storage();
            prop_assert_eq!(KeywordSet::from_storage(&stored).unwrap(), set.clone());
            prop_assert_eq!(KeywordSet::from_storage(&stored).unwrap().to_storage(), stored);
        }

        #[test]
        fn keyword_set_storage_is_order_independent(mut v in prop::collection::vec(keyword_strategy(), 0..8)) {
            let a: KeywordSet = v.iter().cloned().collect();
            v.reverse();
            let b: KeywordSet = v.into_iter().collect();
            prop_assert_eq!(a.to_storage().to_ascii_lowercase(), b.to_storage().to_ascii_lowercase());
        }
    }
}
