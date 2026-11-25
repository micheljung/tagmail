use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, percent_encode};
use url::Url;

use crate::error::{DavError, Result};

/// Everything outside RFC 3986 `pchar` is encoded, so `@`, `:` and the
/// sub-delims stay literal and `%40` / `@` compare equal.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@');

/// Everything but RFC 3986 unreserved characters is encoded.
const STRICT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Resolves a server href against the URL it came from and puts its path
/// into one canonical percent-encoding.
pub fn normalize_href(base: &Url, raw: &str) -> Result<Url> {
    let bad = || DavError::BadHref {
        href: raw.to_owned(),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(bad());
    }
    let mut url = base.join(raw).map_err(|_| bad())?;
    url.set_fragment(None);
    let path = canonical_path(url.path());
    url.set_path(&path);
    Ok(url)
}

fn canonical_path(path: &str) -> String {
    encode_segments(path, SEGMENT)
}

fn encode_segments(path: &str, set: &'static AsciiSet) -> String {
    path.split('/')
        .map(|segment| {
            let decoded = percent_decode_str(segment).collect::<Vec<u8>>();
            percent_encode(&decoded, set).to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` with every segment encoded as strictly as RFC 3986 allows, the
/// spelling every server decodes back to the same name.
pub(crate) fn strict_path(path: &str) -> String {
    encode_segments(path, STRICT)
}

/// Same resource, ignoring a trailing slash on collections.
pub fn same_resource(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
        && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
        && a.query() == b.query()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://webmail.your-server.de/rpc/calendars/").unwrap()
    }

    #[test]
    fn at_sign_variants_compare_equal() {
        let plain = normalize_href(&base(), "/rpc/principals/petition@micheljung.ch/").unwrap();
        let encoded = normalize_href(&base(), "/rpc/principals/petition%40micheljung.ch/").unwrap();
        assert_eq!(plain, encoded);
        assert_eq!(plain.path(), "/rpc/principals/petition@micheljung.ch/");
    }

    #[test]
    fn relative_and_absolute() {
        let rel = normalize_href(&base(), "a/b.ics").unwrap();
        let abs = normalize_href(
            &base(),
            "https://WEBMAIL.your-server.de/rpc/calendars/a/b.ics",
        )
        .unwrap();
        assert_eq!(rel, abs);
        assert_eq!(
            rel.as_str(),
            "https://webmail.your-server.de/rpc/calendars/a/b.ics"
        );
    }

    #[test]
    fn encoding_is_canonical() {
        let lower = normalize_href(&base(), "/cal/my%20event%2fx.ics").unwrap();
        let upper = normalize_href(&base(), "/cal/my%20event%2Fx.ics").unwrap();
        let unicode = normalize_href(&base(), "/cal/caf\u{e9}%7E.ics").unwrap();
        let unicode_enc = normalize_href(&base(), "/cal/caf%C3%A9~.ics").unwrap();
        assert_eq!(lower, upper);
        assert_eq!(lower.path(), "/cal/my%20event%2Fx.ics");
        assert_eq!(unicode, unicode_enc);
        assert_eq!(unicode.path(), "/cal/caf%C3%A9~.ics");
    }

    #[test]
    fn trailing_slash_is_same_collection() {
        let a = normalize_href(&base(), "/rpc/calendars/x").unwrap();
        let b = normalize_href(&base(), "/rpc/calendars/x/").unwrap();
        assert!(same_resource(&a, &b));
    }

    #[test]
    fn invalid_utf8_stays_byte_exact() {
        let latin1 = normalize_href(&base(), "/cal/caf%E9.ics").unwrap();
        assert_eq!(latin1.path(), "/cal/caf%E9.ics");
        let lower = normalize_href(&base(), "/cal/caf%e9.ics").unwrap();
        assert_eq!(latin1, lower);
    }

    #[test]
    fn strict_path_encodes_reserved_but_not_unreserved() {
        assert_eq!(
            strict_path("/rpc/calendars/petition@micheljung.ch/calendar~X_y-z.1/a%2Fb c.ics"),
            "/rpc/calendars/petition%40micheljung.ch/calendar~X_y-z.1/a%2Fb%20c.ics"
        );
    }

    #[test]
    fn empty_href_is_rejected() {
        assert!(normalize_href(&base(), "  ").is_err());
    }
}
