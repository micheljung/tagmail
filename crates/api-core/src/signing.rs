//! Short-lived capability URLs for message parts and proxied images (plan
//! §3.9): the token is the base64url payload and its HMAC-SHA256, so the
//! render route can hand out `/api/parts/{token}` and
//! `/api/proxy/img/{token}` without a server-side table, and the proxy
//! fetches only URLs the server itself put into a rendered message.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tagmail_core::MessageRowId;
use zeroize::Zeroizing;

/// Lifetime of a signed URL.
pub const TOKEN_TTL_SECS: u64 = 10 * 60;
pub const KEY_LEN: usize = 32;
/// Remote URLs longer than this are not proxied (they stay blocked).
pub const MAX_PROXY_URL_LEN: usize = 2048;

pub const PARTS_PREFIX: &str = "/api/parts/";
pub const PROXY_PREFIX: &str = "/api/proxy/img/";

type HmacSha256 = Hmac<Sha256>;

pub type Key = Zeroizing<[u8; KEY_LEN]>;

/// The signing key and, after a rotation, the one before it, which still
/// verifies tokens signed shortly before the rotation.
#[derive(Clone)]
pub struct UrlKeys {
    pub current: Key,
    pub previous: Option<Key>,
}

impl std::fmt::Debug for UrlKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UrlKeys(<redacted>)")
    }
}

/// What a valid token grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// Part `part` (a mail-parser part id) of a message.
    Part { message: MessageRowId, part: u32 },
    /// One remote image URL.
    Image { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
}

fn mac(key: &[u8; KEY_LEN], payload: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(payload);
    mac
}

fn payload(cap: &Capability, exp: u64) -> String {
    match cap {
        Capability::Part { message, part } => format!("p:{exp}:{}:{part}", message.0),
        Capability::Image { url } => format!("i:{exp}:{url}"),
    }
}

/// A token for `cap` valid until `now + TOKEN_TTL_SECS`.
pub fn sign(keys: &UrlKeys, cap: &Capability, now: u64) -> String {
    let payload = payload(cap, now + TOKEN_TTL_SECS);
    let tag = mac(&keys.current, payload.as_bytes())
        .finalize()
        .into_bytes();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

pub fn part_url(keys: &UrlKeys, message: MessageRowId, part: u32, now: u64) -> String {
    format!(
        "{PARTS_PREFIX}{}",
        sign(keys, &Capability::Part { message, part }, now)
    )
}

/// `None` for URLs too long to proxy.
pub fn proxy_url(keys: &UrlKeys, url: &str, now: u64) -> Option<String> {
    (url.len() <= MAX_PROXY_URL_LEN).then(|| {
        let cap = Capability::Image {
            url: url.to_owned(),
        };
        format!("{PROXY_PREFIX}{}", sign(keys, &cap, now))
    })
}

/// Checks the signature (against the current, then the previous key) before
/// looking at the payload, then the expiry.
pub fn verify(keys: &UrlKeys, token: &str, now: u64) -> Result<Capability, TokenError> {
    let (payload, tag) = token.split_once('.').ok_or(TokenError::Malformed)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| TokenError::Malformed)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| TokenError::Malformed)?;
    let valid = std::iter::once(&keys.current)
        .chain(keys.previous.as_ref())
        .any(|key| mac(key, &payload).verify_slice(&tag).is_ok());
    if !valid {
        return Err(TokenError::BadSignature);
    }
    let payload = String::from_utf8(payload).map_err(|_| TokenError::Malformed)?;
    let mut fields = payload.splitn(3, ':');
    let kind = fields.next().ok_or(TokenError::Malformed)?;
    let exp: u64 = fields
        .next()
        .and_then(|e| e.parse().ok())
        .ok_or(TokenError::Malformed)?;
    let rest = fields.next().ok_or(TokenError::Malformed)?;
    let cap = match kind {
        "p" => {
            let (message, part) = rest.split_once(':').ok_or(TokenError::Malformed)?;
            Capability::Part {
                message: MessageRowId(message.parse().map_err(|_| TokenError::Malformed)?),
                part: part.parse().map_err(|_| TokenError::Malformed)?,
            }
        }
        "i" => Capability::Image {
            url: rest.to_owned(),
        },
        _ => return Err(TokenError::Malformed),
    };
    if now > exp {
        return Err(TokenError::Expired);
    }
    Ok(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(current: u8, previous: Option<u8>) -> UrlKeys {
        UrlKeys {
            current: Zeroizing::new([current; KEY_LEN]),
            previous: previous.map(|p| Zeroizing::new([p; KEY_LEN])),
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn round_trips_until_expiry() {
        let k = keys(1, None);
        let part = Capability::Part {
            message: MessageRowId(42),
            part: 3,
        };
        let token = sign(&k, &part, NOW);
        assert_eq!(verify(&k, &token, NOW), Ok(part.clone()));
        assert_eq!(verify(&k, &token, NOW + TOKEN_TTL_SECS), Ok(part));
        assert_eq!(
            verify(&k, &token, NOW + TOKEN_TTL_SECS + 1),
            Err(TokenError::Expired)
        );
        let image = Capability::Image {
            url: "https://example.com/a:b?c=d".to_owned(),
        };
        assert_eq!(verify(&k, &sign(&k, &image, NOW), NOW), Ok(image));
    }

    #[test]
    fn rejects_forgeries() {
        let k = keys(1, None);
        let token = sign(
            &k,
            &Capability::Part {
                message: MessageRowId(1),
                part: 1,
            },
            NOW,
        );
        assert_eq!(
            verify(&keys(2, None), &token, NOW),
            Err(TokenError::BadSignature)
        );
        let (_, tag) = token.split_once('.').unwrap();
        let forged = format!("{}.{tag}", URL_SAFE_NO_PAD.encode("p:9999999999:2:1"));
        assert_eq!(verify(&k, &forged, NOW), Err(TokenError::BadSignature));
        for bad in ["", "abc", "a.b", ".", "!!.!!"] {
            assert!(verify(&k, bad, NOW).is_err(), "{bad}");
        }
    }

    #[test]
    fn previous_key_still_verifies_after_rotation() {
        let old = keys(1, None);
        let token = sign(&old, &Capability::Image { url: "x".into() }, NOW);
        assert!(verify(&keys(2, Some(1)), &token, NOW).is_ok());
        assert_eq!(
            verify(&keys(3, Some(2)), &token, NOW),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn long_urls_are_not_proxied() {
        let k = keys(1, None);
        assert!(proxy_url(&k, &"a".repeat(MAX_PROXY_URL_LEN + 1), NOW).is_none());
        assert!(
            proxy_url(&k, "https://e.com/x.png", NOW)
                .unwrap()
                .starts_with(PROXY_PREFIX)
        );
    }
}
