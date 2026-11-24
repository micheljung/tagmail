//! Outbound HTTP to hosts chosen by others (image URLs in mail, push
//! endpoints from browsers), hardened against SSRF (plan §3.9): each host
//! is resolved once by our resolver, every address must be global unicast,
//! and the connection is pinned to the checked address (reqwest never
//! resolves on its own); no redirects, no cookies, no Referer, no system
//! proxy.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::redirect::Policy;
use tagmail_api_core::BoxFuture;

/// Push endpoint URLs longer than this are refused.
pub const MAX_ENDPOINT_LEN: usize = 2048;
const HTTPS_PORT: u16 = 443;

/// Resolves host names for outbound requests; swapped out in tests.
pub trait Resolver: Send + Sync + 'static {
    /// Fails with [`io::ErrorKind::NotFound`] when the name does not exist.
    fn resolve(&self, host: String) -> BoxFuture<io::Result<Vec<IpAddr>>>;
}

/// The operating system's resolver.
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: String) -> BoxFuture<io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| {
                    if no_such_name(&e) {
                        io::Error::new(io::ErrorKind::NotFound, e.to_string())
                    } else {
                        e
                    }
                })?
                .map(|a| a.ip())
                .collect())
        })
    }
}

/// `getaddrinfo` reports a name that does not exist (`EAI_NONAME`,
/// `EAI_NODATA`) only through its message on Unix, and as
/// `WSAHOST_NOT_FOUND`/`WSANO_DATA` on Windows; anything else (`EAI_AGAIN`
/// for SERVFAIL and timeouts, I/O errors) is a fault of this host's resolver.
fn no_such_name(e: &io::Error) -> bool {
    const WSAHOST_NOT_FOUND: i32 = 11001;
    const WSANO_DATA: i32 = 11004;
    const MESSAGES: &[&str] = &[
        "Name or service not known",
        "No address associated with hostname",
        "Name does not resolve",
        "Name has no usable address",
        "nodename nor servname provided, or not known",
    ];
    if cfg!(windows) {
        return matches!(e.raw_os_error(), Some(WSAHOST_NOT_FOUND | WSANO_DATA));
    }
    let message = e.to_string();
    MESSAGES.iter().any(|m| message.ends_with(m))
}

/// Whether `ip` is in a range that is never fetched.
fn blocked_v4(ip: Ipv4Addr) -> bool {
    const RANGES: &[([u8; 4], u8)] = &[
        ([0, 0, 0, 0], 8),       // "this network"
        ([10, 0, 0, 0], 8),      // private
        ([100, 64, 0, 0], 10),   // CGNAT
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local
        ([172, 16, 0, 0], 12),   // private
        ([192, 0, 0, 0], 24),    // IETF protocol assignments
        ([192, 0, 2, 0], 24),    // documentation
        ([192, 88, 99, 0], 24),  // 6to4 relay anycast
        ([192, 168, 0, 0], 16),  // private
        ([198, 18, 0, 0], 15),   // benchmarking
        ([198, 51, 100, 0], 24), // documentation
        ([203, 0, 113, 0], 24),  // documentation
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // reserved, incl. broadcast
    ];
    let bits = u32::from(ip);
    RANGES.iter().any(|(net, len)| {
        let mask = u32::MAX << (32 - u32::from(*len));
        bits & mask == u32::from_be_bytes(*net) & mask
    })
}

fn in_v6(ip: Ipv6Addr, net: Ipv6Addr, len: u32) -> bool {
    let mask = u128::MAX << (128 - len);
    u128::from(ip) & mask == u128::from(net) & mask
}

/// Global unicast only: IPv4 outside the special ranges, IPv6 inside
/// 2000::/3 minus the translation, tunnelling and documentation ranges.
/// IPv4-mapped and -compatible IPv6 addresses are refused, not unwrapped.
pub fn is_global_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !blocked_v4(v4),
        IpAddr::V6(v6) => {
            const BLOCKED: &[(Ipv6Addr, u32)] = &[
                (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96), // IPv4-mapped
                (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),      // IPv4-compatible, ::, ::1
                (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96), // NAT64
                (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48), // local NAT64
                (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23), // IETF, incl. Teredo
                (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32), // documentation
                (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16), // 6to4
                (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20), // documentation
            ];
            in_v6(v6, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3)
                && !BLOCKED.iter().any(|(net, len)| in_v6(v6, *net, *len))
        }
    }
}

/// Fails every lookup, so a host without a pinned address never connects.
struct NoDns;

impl reqwest::dns::Resolve for NoDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            Err(io::Error::other(format!("unpinned lookup of {host} refused")).into())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// The URL or one of its addresses is not allowed.
    Blocked(String),
    /// The host name does not exist or has no addresses.
    NoSuchHost(String),
    /// This server's resolver failed.
    Unresolved(String),
}

/// The address to connect to for `url`: `http`/`https` without
/// credentials, an allowed port, and a host whose every address passes
/// `allowed_ip`.
pub async fn pin(
    url: &Url,
    resolver: &dyn Resolver,
    allowed_ip: fn(IpAddr) -> bool,
    allowed_port: impl Fn(u16) -> bool,
) -> Result<SocketAddr, PinError> {
    let blocked = |why: String| PinError::Blocked(why);
    if !matches!(url.scheme(), "http" | "https") {
        return Err(blocked(format!("scheme {}", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(blocked("credentials in URL".to_owned()));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| blocked("no port".to_owned()))?;
    if !allowed_port(port) {
        return Err(blocked(format!("port {port}")));
    }
    let ips = match url.host() {
        None => return Err(blocked("no host".to_owned())),
        Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(url::Host::Domain(host)) => resolver.resolve(host.to_owned()).await.map_err(|e| {
            let why = format!("resolving {host}: {e}");
            if e.kind() == io::ErrorKind::NotFound {
                PinError::NoSuchHost(why)
            } else {
                PinError::Unresolved(why)
            }
        })?,
    };
    let ip = *ips
        .first()
        .ok_or_else(|| PinError::NoSuchHost("no addresses".to_owned()))?;
    if let Some(bad) = ips.iter().find(|ip| !allowed_ip(**ip)) {
        return Err(blocked(format!("address {bad}")));
    }
    Ok(SocketAddr::new(ip, port))
}

/// Mozilla roots, ring provider.
pub fn webpki_tls() -> anyhow::Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// A one-shot client that can only reach `url`'s host at `addr`.
pub fn pinned_client(
    tls: rustls::ClientConfig,
    url: &Url,
    addr: SocketAddr,
    timeout: Duration,
    user_agent: &str,
) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .redirect(Policy::none())
        .referer(false)
        .dns_resolver(Arc::new(NoDns))
        .timeout(timeout)
        .user_agent(user_agent);
    if let Some(url::Host::Domain(host)) = url.host() {
        builder = builder.resolve(host, addr);
    }
    builder.build()
}

/// Which push endpoints are acceptable, at registration and on every send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::manual_non_exhaustive,
    reason = "the hidden variant is for tests, not future variants"
)]
pub enum EndpointPolicy {
    /// `https` to a DNS name on port 443 whose addresses are all global.
    #[default]
    Strict,
    /// Also `http`, IP literals, loopback and any port: a local mock push
    /// service. Never reachable from the command line.
    #[doc(hidden)]
    LoopbackForTests,
}

fn loopback_or_global(ip: IpAddr) -> bool {
    ip.is_loopback() || is_global_unicast(ip)
}

impl EndpointPolicy {
    /// Parses and checks an endpoint as posted by a browser.
    pub fn validate(self, endpoint: &str) -> Result<Url, &'static str> {
        if endpoint.len() > MAX_ENDPOINT_LEN {
            return Err("endpoint too long");
        }
        let url = Url::parse(endpoint).map_err(|_| "invalid URL")?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err("credentials in URL");
        }
        match self {
            EndpointPolicy::Strict => {
                if url.scheme() != "https" {
                    return Err("not https");
                }
                if url.port().is_some_and(|p| p != HTTPS_PORT) {
                    return Err("port other than 443");
                }
                if !matches!(url.host(), Some(url::Host::Domain(_))) {
                    return Err("host is not a DNS name");
                }
            }
            EndpointPolicy::LoopbackForTests => {
                if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                    return Err("not http(s)");
                }
            }
        }
        Ok(url)
    }

    /// [`EndpointPolicy::validate`] plus [`pin`] under this policy.
    pub async fn pin(self, url: &Url, resolver: &dyn Resolver) -> Result<SocketAddr, PinError> {
        self.validate(url.as_str())
            .map_err(|why| PinError::Blocked(why.to_owned()))?;
        match self {
            EndpointPolicy::Strict => {
                pin(url, resolver, is_global_unicast, |p| p == HTTPS_PORT).await
            }
            EndpointPolicy::LoopbackForTests => {
                pin(url, resolver, loopback_or_global, |_| true).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_range_table() {
        let blocked = [
            "0.0.0.0",
            "0.255.255.255",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "64:ff9b::a00:1",
            "64:ff9b::808:808",
            "64:ff9b:1::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:2::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "2002:808:808::1",
            "3fff::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "ff02::1",
            "100::1",
        ];
        for ip in blocked {
            assert!(
                !is_global_unicast(ip.parse().unwrap()),
                "{ip} must be blocked"
            );
        }
        let allowed = [
            "1.1.1.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.1",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2001:200::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
        ];
        for ip in allowed {
            assert!(
                is_global_unicast(ip.parse().unwrap()),
                "{ip} must be allowed"
            );
        }
    }

    #[test]
    fn endpoint_table() {
        let rejected = [
            "http://fcm.googleapis.com/fcm/send/x",
            "https://1.2.3.4/x",
            "https://[2606:4700::1]/x",
            "https://fcm.googleapis.com:8443/x",
            "https://user:pw@fcm.googleapis.com/x",
            "https://user@fcm.googleapis.com/x",
            "not a url",
            "data:text/plain,x",
        ];
        for url in rejected {
            assert!(
                EndpointPolicy::Strict.validate(url).is_err(),
                "{url} must be rejected"
            );
        }
        let long = format!("https://push.example/{}", "a".repeat(MAX_ENDPOINT_LEN));
        assert!(EndpointPolicy::Strict.validate(&long).is_err());
        for url in [
            "https://fcm.googleapis.com/fcm/send/x",
            "https://web.push.apple.com:443/QG",
        ] {
            assert!(
                EndpointPolicy::Strict.validate(url).is_ok(),
                "{url} must be accepted"
            );
        }
        assert!(
            EndpointPolicy::LoopbackForTests
                .validate("http://127.0.0.1:9/push")
                .is_ok()
        );
        assert!(
            EndpointPolicy::LoopbackForTests
                .validate("ftp://127.0.0.1/push")
                .is_err()
        );
    }

    struct Fixed(Vec<IpAddr>);

    impl Resolver for Fixed {
        fn resolve(&self, _: String) -> BoxFuture<io::Result<Vec<IpAddr>>> {
            let ips = self.0.clone();
            Box::pin(async move { Ok(ips) })
        }
    }

    struct Failing(io::ErrorKind);

    impl Resolver for Failing {
        fn resolve(&self, _: String) -> BoxFuture<io::Result<Vec<IpAddr>>> {
            let kind = self.0;
            Box::pin(async move { Err(io::Error::from(kind)) })
        }
    }

    #[tokio::test]
    async fn a_missing_name_is_told_apart_from_a_failing_resolver() {
        let url = Url::parse("https://push.example/x").unwrap();
        let pin = |kind| {
            let url = url.clone();
            async move { EndpointPolicy::Strict.pin(&url, &Failing(kind)).await }
        };
        assert!(matches!(
            pin(io::ErrorKind::NotFound).await,
            Err(PinError::NoSuchHost(_))
        ));
        assert!(matches!(
            pin(io::ErrorKind::TimedOut).await,
            Err(PinError::Unresolved(_))
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn getaddrinfo_messages_for_missing_names_are_recognized() {
        let gai = |detail: &str| {
            io::Error::other(format!("failed to lookup address information: {detail}"))
        };
        for missing in [
            "Name or service not known",
            "No address associated with hostname",
            "Name does not resolve",
            "nodename nor servname provided, or not known",
        ] {
            assert!(no_such_name(&gai(missing)), "{missing}");
        }
        for fault in [
            "Temporary failure in name resolution",
            "Non-recoverable failure in name resolution",
            "System error",
        ] {
            assert!(!no_such_name(&gai(fault)), "{fault}");
        }
    }

    #[tokio::test]
    async fn strict_pins_only_when_every_address_is_global() {
        let url = Url::parse("https://push.example/x").unwrap();
        let pin = |ips: &[&str]| {
            let dns = Fixed(ips.iter().map(|ip| ip.parse().unwrap()).collect());
            let url = url.clone();
            async move { EndpointPolicy::Strict.pin(&url, &dns).await }
        };
        assert_eq!(
            pin(&["8.8.8.8"]).await.unwrap(),
            "8.8.8.8:443".parse().unwrap()
        );
        assert!(matches!(
            pin(&["8.8.8.8", "10.0.0.1"]).await,
            Err(PinError::Blocked(_))
        ));
        assert!(matches!(pin(&[]).await, Err(PinError::NoSuchHost(_))));
        let loopback = Fixed(vec!["127.0.0.1".parse().unwrap()]);
        assert!(EndpointPolicy::Strict.pin(&url, &loopback).await.is_err());
        let local = Url::parse("http://127.0.0.1:4000/x").unwrap();
        assert_eq!(
            EndpointPolicy::LoopbackForTests
                .pin(&local, &Fixed(Vec::new()))
                .await
                .unwrap(),
            "127.0.0.1:4000".parse().unwrap()
        );
    }
}
