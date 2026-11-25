use url::{Host, Url};

const RESERVED_TLDS: &[&str] = &["test", "localhost", "local", "internal"];
const RESERVED_DOMAIN_LABELS: usize = 2;

/// The registrable domain (eTLD+1) a host belongs to. An IP literal is its
/// own registrable domain; names under a reserved TLD missing from the PSL
/// use their last two labels.
pub fn registrable_domain(host: &str) -> String {
    let host = normalize_host(host);
    if is_ip(&host) {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.last().is_some_and(|tld| RESERVED_TLDS.contains(tld)) {
        let keep = labels.len().min(RESERVED_DOMAIN_LABELS);
        return labels[labels.len() - keep..].join(".");
    }
    psl::domain_str(&host).map_or_else(|| host.clone(), str::to_owned)
}

/// Whether credentials anchored to `confirmed` may be sent to `host`.
pub fn host_allowed(confirmed: &str, host: &str) -> bool {
    let host = normalize_host(host);
    let confirmed = normalize_host(confirmed);
    if host == confirmed {
        return true;
    }
    if is_ip(&host) || is_ip(&confirmed) {
        return false;
    }
    registrable_domain(&host) == registrable_domain(&confirmed)
}

/// The host of `url` as compared by [`host_allowed`].
pub fn url_host(url: &Url) -> Option<String> {
    url.host().map(|h| host_string(&h.to_owned()))
}

fn host_string(host: &Host) -> String {
    match host {
        Host::Domain(d) => d.trim_end_matches('.').to_owned(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => format!("[{ip}]"),
    }
}

/// The domain one label above `host`, unless that is a public suffix.
pub fn parent_domain(host: &str) -> Option<String> {
    let host = normalize_host(host);
    if is_ip(&host) {
        return None;
    }
    let (_, parent) = host.split_once('.')?;
    let registrable = registrable_domain(&host);
    (parent.len() >= registrable.len()).then(|| parent.to_owned())
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// One spelling per host: lowercase, IDNA punycode, no trailing dot, IPv6
/// in brackets.
fn normalize_host(host: &str) -> String {
    let host = host.trim_end_matches('.');
    let bracketed;
    let input = if host.contains(':') && !host.starts_with('[') {
        bracketed = format!("[{host}]");
        &bracketed
    } else {
        host
    };
    Host::parse(input).map_or_else(|_| host.to_ascii_lowercase(), |h| host_string(&h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrable_domains() {
        assert_eq!(
            registrable_domain("webmail.your-server.de"),
            "your-server.de"
        );
        assert_eq!(registrable_domain("p07-caldav.icloud.com"), "icloud.com");
        assert_eq!(registrable_domain("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("a.mail.test"), "mail.test");
        assert_eq!(registrable_domain("mail.test"), "mail.test");
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("dav.corp.internal"), "corp.internal");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain("Mail.Example.COM."), "example.com");
    }

    #[test]
    fn hosts_allowed() {
        assert!(host_allowed(
            "mail.your-server.de",
            "webmail.your-server.de"
        ));
        assert!(host_allowed("caldav.icloud.com", "p42-caldav.icloud.com"));
        assert!(!host_allowed("caldav.icloud.com", "evil.com"));
        assert!(!host_allowed("a.mail.test", "b.other.test"));
        assert!(host_allowed("a.mail.test", "b.mail.test"));
        assert!(host_allowed("127.0.0.1", "127.0.0.1"));
        assert!(!host_allowed("127.0.0.1", "127.0.0.2"));
        assert!(!host_allowed("example.com", "93.184.216.34"));
        assert!(!host_allowed("github.io", "evil.github.io"));
    }

    #[test]
    fn ipv6_and_idn_spellings_compare_equal() {
        assert!(host_allowed("::1", "[::1]"));
        assert!(host_allowed("[0:0::1]", "[::1]"));
        assert!(!host_allowed("[::1]", "[::2]"));
        assert!(host_allowed(
            "dav.b\u{fc}cher.example",
            "mail.xn--bcher-kva.example"
        ));
        assert_eq!(
            registrable_domain("Dav.B\u{dc}cher.example"),
            "xn--bcher-kva.example"
        );
    }

    #[test]
    fn parents() {
        assert_eq!(
            parent_domain("mail.your-server.de").as_deref(),
            Some("your-server.de")
        );
        assert_eq!(parent_domain("your-server.de"), None);
        assert_eq!(
            parent_domain("imap.mail.example.co.uk").as_deref(),
            Some("mail.example.co.uk")
        );
        assert_eq!(parent_domain("example.co.uk"), None);
        assert_eq!(parent_domain("10.0.0.1"), None);
    }
}
