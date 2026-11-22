//! Account-scoped message identity that survives moves, copies and
//! UIDVALIDITY resets (plan §3.4): RFC 8474 `EMAILID` when the server has
//! `OBJECTID`, Gmail `X-GM-MSGID` with `X-GM-EXT-1`, else a hash of
//! normalized header fields.

use std::fmt;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};

const OBJECT_ID_PREFIX: &str = "o:";
const GMAIL_PREFIX: &str = "g:";
const HASH_PREFIX: &str = "h:";
const HASH_DOMAIN: &[u8] = b"tagmail-email-id-v1";
const HASH_HEX_LEN: usize = 32;
const OBJECT_ID_MAX_LEN: usize = 255;

/// Stored as `message.email_id` and in durable `email_ids` sets.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EmailId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailIdSource {
    ObjectId,
    GmailMsgId,
    HeaderHash,
}

/// Header fields hashed when the server offers no stable id. Absent and
/// empty fields hash differently.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderIdentity<'a> {
    /// Raw `Message-ID` header value.
    pub message_id: Option<&'a str>,
    /// `Date` header as Unix seconds, `None` when missing or unparseable.
    pub date: Option<i64>,
    /// Address of the first `From` mailbox.
    pub from: Option<&'a str>,
    /// RFC 2047-decoded `Subject`.
    pub subject: Option<&'a str>,
    /// `RFC822.SIZE`; `None` drops size from the identity for servers whose
    /// copies of one message differ in size.
    pub size: Option<u64>,
}

impl EmailId {
    /// RFC 8474 `objectid = 1*255(ALPHA / DIGIT / "_" / "-")`, kept verbatim
    /// because object ids are case-sensitive.
    pub fn from_object_id(object_id: &str) -> Result<EmailId> {
        let valid = (1..=OBJECT_ID_MAX_LEN).contains(&object_id.len())
            && object_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid {
            return Err(Error::InvalidObjectId(object_id.to_owned()));
        }
        Ok(EmailId(format!("{OBJECT_ID_PREFIX}{object_id}")))
    }

    pub fn from_gmail_msgid(msgid: u64) -> EmailId {
        EmailId(format!("{GMAIL_PREFIX}{msgid}"))
    }

    pub fn from_headers(h: &HeaderIdentity<'_>) -> EmailId {
        let message_id = h.message_id.map(normalize_message_id);
        let from = h.from.map(normalize_address);
        let subject = h.subject.map(normalize_subject);
        let date = h.date.map(i64::to_be_bytes);
        let size = h.size.map(u64::to_be_bytes);

        let mut hasher = Sha256::new();
        hasher.update(HASH_DOMAIN);
        for field in [
            message_id.as_deref().map(str::as_bytes),
            date.as_ref().map(<[u8; 8]>::as_slice),
            from.as_deref().map(str::as_bytes),
            subject.as_deref().map(str::as_bytes),
            size.as_ref().map(<[u8; 8]>::as_slice),
        ] {
            match field {
                None => hasher.update([0u8]),
                Some(bytes) => {
                    hasher.update([1u8]);
                    hasher.update((bytes.len() as u64).to_be_bytes());
                    hasher.update(bytes);
                }
            }
        }
        let digest = hasher.finalize();
        EmailId(format!(
            "{HASH_PREFIX}{}",
            hex::encode(&digest[..HASH_HEX_LEN / 2])
        ))
    }

    /// Validates a value read back from storage or an API request.
    pub fn parse(stored: &str) -> Result<EmailId> {
        let invalid = || Error::InvalidEmailId(stored.to_owned());
        if let Some(object_id) = stored.strip_prefix(OBJECT_ID_PREFIX) {
            return EmailId::from_object_id(object_id).map_err(|_| invalid());
        }
        if let Some(msgid) = stored.strip_prefix(GMAIL_PREFIX) {
            let n: u64 = msgid.parse().map_err(|_| invalid())?;
            return (n.to_string() == msgid)
                .then(|| EmailId::from_gmail_msgid(n))
                .ok_or_else(invalid);
        }
        if let Some(hash) = stored.strip_prefix(HASH_PREFIX) {
            let valid = hash.len() == HASH_HEX_LEN
                && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            return valid
                .then(|| EmailId(stored.to_owned()))
                .ok_or_else(invalid);
        }
        Err(invalid())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn source(&self) -> EmailIdSource {
        if self.0.starts_with(OBJECT_ID_PREFIX) {
            EmailIdSource::ObjectId
        } else if self.0.starts_with(GMAIL_PREFIX) {
            EmailIdSource::GmailMsgId
        } else {
            EmailIdSource::HeaderHash
        }
    }
}

impl fmt::Display for EmailId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Drops folding whitespace and the enclosing angle brackets. The id itself
/// keeps its case: the local part is case-sensitive.
pub fn normalize_message_id(raw: &str) -> String {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .map_or_else(|| compact.clone(), str::to_owned)
}

fn normalize_address(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed)
        .trim()
        .to_ascii_lowercase()
}

fn normalize_subject(raw: &str) -> String {
    let nfc: String = raw.nfc().collect();
    nfc.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn base() -> HeaderIdentity<'static> {
        HeaderIdentity {
            message_id: Some("<abc@example.com>"),
            date: Some(1_700_000_000),
            from: Some("Alice@Example.com"),
            subject: Some("Hello world"),
            size: Some(1234),
        }
    }

    #[test]
    fn object_id_is_validated_and_case_sensitive() {
        let a = EmailId::from_object_id("M6d99ac3275bb4e").unwrap();
        assert_eq!(a.as_str(), "o:M6d99ac3275bb4e");
        assert_ne!(a, EmailId::from_object_id("m6d99ac3275bb4e").unwrap());
        assert_eq!(a.source(), EmailIdSource::ObjectId);
        for bad in ["", "a b", "a.b", "é"] {
            assert!(EmailId::from_object_id(bad).is_err(), "{bad:?}");
        }
        assert!(EmailId::from_object_id(&"a".repeat(255)).is_ok());
        assert!(EmailId::from_object_id(&"a".repeat(256)).is_err());
    }

    #[test]
    fn gmail_msgid_is_decimal() {
        let id = EmailId::from_gmail_msgid(1_278_455_344_230_334_865);
        assert_eq!(id.as_str(), "g:1278455344230334865");
        assert_eq!(id.source(), EmailIdSource::GmailMsgId);
    }

    #[test]
    fn header_hash_has_fixed_shape() {
        let id = EmailId::from_headers(&base());
        assert_eq!(id.source(), EmailIdSource::HeaderHash);
        assert_eq!(id.as_str().len(), 2 + HASH_HEX_LEN);
        assert_eq!(EmailId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn header_hash_is_pinned() {
        assert_eq!(
            EmailId::from_headers(&base()).as_str(),
            "h:b0b47e8111fb6be38472a8a7fa42f2b4"
        );
        let empty = EmailId::from_headers(&HeaderIdentity::default());
        assert_eq!(empty.as_str(), "h:198c5969016b0ee3817ce7fa736e94bd");
    }

    #[test]
    fn header_hash_normalizes_formatting_noise() {
        let noisy = HeaderIdentity {
            message_id: Some(" <abc@\r\n example.com>"),
            from: Some(" <ALICE@example.COM> "),
            subject: Some("  Hello \t\r\n world "),
            ..base()
        };
        assert_eq!(
            EmailId::from_headers(&noisy),
            EmailId::from_headers(&base())
        );
        let nfd = HeaderIdentity {
            subject: Some("Cafe\u{301}"),
            ..base()
        };
        let nfc = HeaderIdentity {
            subject: Some("Caf\u{e9}"),
            ..base()
        };
        assert_eq!(EmailId::from_headers(&nfd), EmailId::from_headers(&nfc));
    }

    #[test]
    fn header_hash_distinguishes_fields() {
        let id = EmailId::from_headers(&base());
        let variants = [
            HeaderIdentity {
                message_id: Some("<ABC@example.com>"),
                ..base()
            },
            HeaderIdentity {
                message_id: None,
                ..base()
            },
            HeaderIdentity {
                message_id: Some(""),
                ..base()
            },
            HeaderIdentity {
                date: Some(1_700_000_001),
                ..base()
            },
            HeaderIdentity {
                date: None,
                ..base()
            },
            HeaderIdentity {
                from: Some("bob@example.com"),
                ..base()
            },
            HeaderIdentity {
                subject: Some("Hello  world!"),
                ..base()
            },
            HeaderIdentity {
                size: Some(1235),
                ..base()
            },
            HeaderIdentity {
                size: None,
                ..base()
            },
        ];
        for v in variants {
            assert_ne!(EmailId::from_headers(&v), id, "{v:?}");
        }
    }

    #[test]
    fn header_fields_cannot_shift_into_each_other() {
        let a = HeaderIdentity {
            from: Some("ab"),
            subject: Some("c"),
            ..HeaderIdentity::default()
        };
        let b = HeaderIdentity {
            from: Some("a"),
            subject: Some("bc"),
            ..HeaderIdentity::default()
        };
        assert_ne!(EmailId::from_headers(&a), EmailId::from_headers(&b));
    }

    #[test]
    fn message_id_normalization() {
        assert_eq!(normalize_message_id("<a@b>"), "a@b");
        assert_eq!(normalize_message_id("a@b"), "a@b");
        assert_eq!(normalize_message_id(" <A@\r\n B> "), "A@B");
    }

    #[test]
    fn parse_rejects_foreign_values() {
        for bad in [
            "", "x:1", "h:ABC", "h:", "g:01", "g:-1", "g:x", "o:", "o:a.b",
        ] {
            assert!(EmailId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(EmailId::parse(&format!("h:{}", "A".repeat(32))).is_err());
    }

    proptest! {
        #[test]
        fn every_constructor_round_trips_through_parse(
            object_id in "[A-Za-z0-9_-]{1,255}",
            msgid: u64,
            mid in proptest::option::of(".*"),
            subject in proptest::option::of(".*"),
            date: Option<i64>,
            size: Option<u64>,
        ) {
            for id in [
                EmailId::from_object_id(&object_id).unwrap(),
                EmailId::from_gmail_msgid(msgid),
                EmailId::from_headers(&HeaderIdentity {
                    message_id: mid.as_deref(),
                    date,
                    from: None,
                    subject: subject.as_deref(),
                    size,
                }),
            ] {
                prop_assert_eq!(EmailId::parse(id.as_str()).unwrap(), id);
            }
        }
    }
}
