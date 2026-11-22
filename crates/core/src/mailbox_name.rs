//! IMAP modified UTF-7 mailbox names (RFC 3501 §5.1.3).

use base64::Engine as _;
use base64::alphabet::Alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

const ALPHABET: Alphabet =
    match Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,") {
        Ok(a) => a,
        Err(_) => panic!("valid modified base64 alphabet"),
    };

const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &ALPHABET,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        .with_decode_allow_trailing_bits(true),
);

/// The display form of a wire mailbox name. Names that are not valid
/// modified UTF-7 (servers with UTF8=ACCEPT send UTF-8) come back unchanged.
pub fn decode_mailbox_name(wire: &str) -> String {
    try_decode(wire).unwrap_or_else(|| wire.to_owned())
}

fn try_decode(wire: &str) -> Option<String> {
    let mut out = String::with_capacity(wire.len());
    let mut rest = wire;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('-')?;
        let encoded = &after[..end];
        if encoded.is_empty() {
            out.push('&');
        } else {
            let bytes = ENGINE.decode(encoded).ok()?;
            if bytes.len() % 2 != 0 {
                return None;
            }
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c));
            for c in char::decode_utf16(units) {
                out.push(c.ok()?);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rfc_examples() {
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(
            decode_mailbox_name("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
        assert_eq!(decode_mailbox_name("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_mailbox_name("Entw&APw-rfe"), "Entwürfe");
    }

    #[test]
    fn invalid_names_stay_raw() {
        for raw in ["a&b", "&Jjo!-", "Entwürfe", "&AP-"] {
            assert_eq!(decode_mailbox_name(raw), raw, "{raw}");
        }
    }
}
