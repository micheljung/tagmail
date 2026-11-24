//! Label ↔ IMAP keyword codec (plan Decision d).
//!
//! A label `Work Stuff/2026` is the keyword `$label_Work=20Stuff=2F2026`:
//! bytes outside `[A-Za-z0-9_.-]` of the UTF-8 name become `=HH` with
//! uppercase hex. Decoding accepts either hex case and a case-insensitive
//! prefix, since IMAP keywords are case-insensitive. Label identity is the
//! Unicode casefold of the decoded name.

use std::fmt;

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};
use crate::flags::Keyword;

pub const LABEL_PREFIX: &str = "$label_";

/// Server-safe encoded keyword length unless a server overrides it.
pub const DEFAULT_KEYWORD_LIMIT: usize = 200;

/// Set and cleared again by the engine to learn whether a server keeps
/// custom keywords.
pub const PROBE_KEYWORD: &str = "$tagmail_probe";

/// Keywords of other clients and of RFC 5788 / IANA, and the probe keyword,
/// that are never shown as labels. Compared case-insensitively.
const HIDDEN_KEYWORDS: &[&str] = &[
    PROBE_KEYWORD,
    "$Forwarded",
    "$MDNSent",
    "$Junk",
    "$NotJunk",
    "Junk",
    "NonJunk",
    "NotJunk",
    "$Phishing",
    "$Important",
    "$Submitted",
    "$SubmitPending",
    "$HasAttachment",
    "$HasNoAttachment",
    "$IsMailingList",
    "$IsTrusted",
    "$Notify",
    "$Muted",
    "$Followed",
    "$Memo",
    "$HasMemo",
    "$Unsubscribed",
    "$CanUnsubscribe",
    "$AutoSent",
    "$Imported",
    "$MailFlagBit0",
    "$MailFlagBit1",
    "$MailFlagBit2",
    "$has_cal",
];

/// Casefolded label identity (`label.label_key`, `label_registry.label_key`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LabelKey(String);

impl LabelKey {
    /// Unicode canonical caseless form: `NFC(casefold(NFD(name)))`.
    pub fn of(name: &str) -> LabelKey {
        let decomposed: String = name.nfd().collect();
        LabelKey(caseless::default_case_fold_str(&decomposed).nfc().collect())
    }

    /// Wraps a key read back from storage, which was produced by [`LabelKey::of`].
    pub fn from_stored(key: String) -> LabelKey {
        LabelKey(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// Canonical keyword for a label name; over `limit` bytes is an error, never
/// truncated.
pub fn encode_label(name: &str, limit: usize) -> Result<Keyword> {
    if name.trim().is_empty() {
        return Err(Error::EmptyLabel);
    }
    let mut out = String::with_capacity(LABEL_PREFIX.len() + name.len());
    out.push_str(LABEL_PREFIX);
    for b in name.bytes() {
        if is_safe(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("={b:02X}"));
        }
    }
    if out.len() > limit {
        return Err(Error::LabelTooLong {
            len: out.len(),
            limit,
        });
    }
    Keyword::new(out)
}

/// Label name of a `$label_` keyword.
pub fn decode_label(keyword: &str) -> Result<String> {
    let has_prefix = keyword
        .get(..LABEL_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(LABEL_PREFIX));
    if !has_prefix {
        return Err(Error::NotALabelKeyword(keyword.to_owned()));
    }
    let encoded = &keyword.as_bytes()[LABEL_PREFIX.len()..];
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        if encoded[i] == b'=' {
            let hex = encoded
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| Error::InvalidEscape(keyword.to_owned()))?;
            bytes.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| Error::InvalidEscape(keyword.to_owned()))?,
            );
            i += 3;
        } else {
            bytes.push(encoded[i]);
            i += 1;
        }
    }
    let name = String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8(keyword.to_owned()))?;
    if name.trim().is_empty() {
        return Err(Error::EmptyLabel);
    }
    Ok(name)
}

/// What a location keyword means to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeywordClass {
    /// A system keyword such as `$Junk`; never shown.
    Hidden,
    /// A label: our `$label_` keyword or any other client's keyword
    /// (`$label1`, `Work`), whose name is then the keyword itself.
    Label { name: String, key: LabelKey },
}

pub fn classify_keyword(keyword: &Keyword) -> KeywordClass {
    let kw = keyword.as_str();
    if HIDDEN_KEYWORDS.iter().any(|h| h.eq_ignore_ascii_case(kw)) {
        return KeywordClass::Hidden;
    }
    let name = decode_label(kw).unwrap_or_else(|_| kw.to_owned());
    KeywordClass::Label {
        key: LabelKey::of(&name),
        name,
    }
}

/// The canonical spelling of a `$label_` keyword (uppercase hex, minimal
/// escaping, exact prefix), or `None` for other keywords. The limit is not
/// applied: the server already holds the keyword.
pub fn canonical_label_keyword(keyword: &Keyword) -> Option<Keyword> {
    let name = decode_label(keyword.as_str()).ok()?;
    encode_label(&name, usize::MAX).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn kw(s: &str) -> Keyword {
        Keyword::new(s).unwrap()
    }

    #[test]
    fn encodes_plan_example() {
        let k = encode_label("Work Stuff/2026", DEFAULT_KEYWORD_LIMIT).unwrap();
        assert_eq!(k.as_str(), "$label_Work=20Stuff=2F2026");
    }

    #[test]
    fn encodes_utf8_and_escape_char_with_uppercase_hex() {
        let k = encode_label("Café=1", DEFAULT_KEYWORD_LIMIT).unwrap();
        assert_eq!(k.as_str(), "$label_Caf=C3=A9=3D1");
    }

    #[test]
    fn decoder_accepts_lowercase_hex_and_prefix_case() {
        assert_eq!(decode_label("$LABEL_work=2fx").unwrap(), "work/x");
        assert_eq!(decode_label("$label_Caf=c3=a9").unwrap(), "Café");
    }

    #[test]
    fn decoder_rejects_bad_input() {
        assert_eq!(
            decode_label("Work"),
            Err(Error::NotALabelKeyword("Work".into()))
        );
        assert!(matches!(
            decode_label("$label_a=2"),
            Err(Error::InvalidEscape(_))
        ));
        assert!(matches!(
            decode_label("$label_a=zz"),
            Err(Error::InvalidEscape(_))
        ));
        assert!(matches!(
            decode_label("$label_=FF"),
            Err(Error::InvalidUtf8(_))
        ));
        assert_eq!(decode_label("$label_"), Err(Error::EmptyLabel));
        assert_eq!(decode_label("$label_=20"), Err(Error::EmptyLabel));
    }

    #[test]
    fn rejects_empty_names() {
        assert_eq!(
            encode_label("", DEFAULT_KEYWORD_LIMIT),
            Err(Error::EmptyLabel)
        );
        assert_eq!(
            encode_label("  ", DEFAULT_KEYWORD_LIMIT),
            Err(Error::EmptyLabel)
        );
    }

    #[test]
    fn length_limit_is_a_hard_error() {
        let fits = "a".repeat(DEFAULT_KEYWORD_LIMIT - LABEL_PREFIX.len());
        assert_eq!(
            encode_label(&fits, DEFAULT_KEYWORD_LIMIT)
                .unwrap()
                .as_str()
                .len(),
            200
        );
        let over = format!("{fits}a");
        assert_eq!(
            encode_label(&over, DEFAULT_KEYWORD_LIMIT),
            Err(Error::LabelTooLong {
                len: 201,
                limit: 200
            })
        );
        assert_eq!(
            encode_label("é", 9),
            Err(Error::LabelTooLong { len: 13, limit: 9 })
        );
    }

    #[test]
    fn label_key_casefolds_beyond_ascii() {
        assert_eq!(LabelKey::of("Straße"), LabelKey::of("STRASSE"));
        assert_eq!(LabelKey::of("Work"), LabelKey::of("wORK"));
        assert_eq!(LabelKey::of("ΣΑΣ"), LabelKey::of("σας"));
        assert_ne!(LabelKey::of("Work"), LabelKey::of("Works"));
    }

    #[test]
    fn label_key_ignores_normalization_form() {
        assert_eq!(LabelKey::of("Caf\u{e9}"), LabelKey::of("Cafe\u{301}"));
    }

    #[test]
    fn classifies_hidden_and_foreign_keywords() {
        assert_eq!(classify_keyword(&kw("$junk")), KeywordClass::Hidden);
        assert_eq!(classify_keyword(&kw("$MDNSent")), KeywordClass::Hidden);
        assert_eq!(classify_keyword(&kw("NonJunk")), KeywordClass::Hidden);
        assert_eq!(classify_keyword(&kw("$has_cal")), KeywordClass::Hidden);
        assert_eq!(
            classify_keyword(&kw("$label1")),
            KeywordClass::Label {
                name: "$label1".into(),
                key: LabelKey::of("$label1")
            }
        );
        assert_eq!(
            classify_keyword(&kw("Work")),
            KeywordClass::Label {
                name: "Work".into(),
                key: LabelKey::of("work")
            }
        );
        assert_eq!(
            classify_keyword(&kw("$label_Work=20Stuff")),
            KeywordClass::Label {
                name: "Work Stuff".into(),
                key: LabelKey::of("work stuff")
            }
        );
        assert_eq!(
            classify_keyword(&kw("$label_=zz")),
            KeywordClass::Label {
                name: "$label_=zz".into(),
                key: LabelKey::of("$label_=zz")
            }
        );
    }

    #[test]
    fn canonicalizes_label_keywords_only() {
        assert_eq!(
            canonical_label_keyword(&kw("$Label_a=2fb+c"))
                .unwrap()
                .as_str(),
            "$label_a=2Fb=2Bc"
        );
        assert_eq!(canonical_label_keyword(&kw("Work")), None);
    }

    proptest! {
        #[test]
        fn round_trips_any_name(name in "\\PC*[^\\s]\\PC*") {
            let k = encode_label(&name, usize::MAX).unwrap();
            prop_assert_eq!(decode_label(k.as_str()).unwrap(), name);
        }

        #[test]
        fn encoding_uses_only_safe_ascii(name in ".*[^\\s].*") {
            let k = encode_label(&name, usize::MAX).unwrap();
            let body = &k.as_str()[LABEL_PREFIX.len()..];
            prop_assert!(body.bytes().all(|b| is_safe(b) || b == b'='));
            prop_assert!(Keyword::new(k.as_str()).is_ok());
        }

        #[test]
        fn canonicalization_is_idempotent_and_hex_case_blind(name in ".*[^\\s].*") {
            let canonical = encode_label(&name, usize::MAX).unwrap();
            let lower = Keyword::new(canonical.as_str().to_ascii_lowercase()).unwrap();
            let from_lower = canonical_label_keyword(&lower).unwrap();
            prop_assert_eq!(decode_label(lower.as_str()).unwrap(), name.to_ascii_lowercase());
            prop_assert_eq!(canonical_label_keyword(&canonical).unwrap().to_string(), canonical.to_string());
            let again = canonical_label_keyword(&from_lower).unwrap();
            prop_assert_eq!(again.as_str(), from_lower.as_str());
        }

        #[test]
        fn label_key_is_case_blind_for_ascii(name in "[A-Za-z0-9 ]{1,20}") {
            prop_assert_eq!(LabelKey::of(&name.to_ascii_uppercase()), LabelKey::of(&name.to_ascii_lowercase()));
        }

        #[test]
        fn decoder_never_panics(s in "\\$label_[!-~]{0,30}") {
            let _ = decode_label(&s);
        }
    }
}
