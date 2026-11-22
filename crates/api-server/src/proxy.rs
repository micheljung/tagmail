//! The remote image proxy behind `/api/proxy/img/{token}` (plan §3.9),
//! hardened against SSRF: it only ever fetches URLs the server signed;
//! only `http`/`https` on ports 80/443; each hop's host is resolved once by
//! our resolver, every address must be global unicast, and the connection
//! is pinned to the checked address (reqwest never resolves on its own);
//! redirects are followed by hand, at most three, each checked the same
//! way; 5 s for the whole fetch; at most 5 MB, streamed; the answer must
//! say `image/*` and start with a raster image signature; no cookies, no
//! Referer, no system proxy.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url, header};
use tagmail_api_core::mime::sniff_image;
use tagmail_api_core::{BoxFuture, ImageProxy, ProxiedImage, ProxyError};
use tokio::time::Instant;

pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 3;
pub const PORTS: &[u16] = &[80, 443];
const USER_AGENT: &str = "Tagmail-ImageProxy";

/// Resolves host names for the proxy; swapped out in tests.
pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: String) -> BoxFuture<io::Result<Vec<IpAddr>>>;
}

/// The operating system's resolver.
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: String) -> BoxFuture<io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    }
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

/// The SSRF-guarded fetcher.
#[derive(Clone)]
pub struct ImageFetcher {
    resolver: Arc<dyn Resolver>,
    tls: rustls::ClientConfig,
    /// Address policy; [`is_global_unicast`] outside tests.
    allowed_ip: fn(IpAddr) -> bool,
    ports: Vec<u16>,
    timeout: Duration,
    max_bytes: usize,
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

fn blocked(why: impl Into<String>) -> ProxyError {
    ProxyError::Blocked(why.into())
}

impl ImageFetcher {
    pub fn new(tls: rustls::ClientConfig) -> ImageFetcher {
        ImageFetcher {
            resolver: Arc::new(SystemResolver),
            tls,
            allowed_ip: is_global_unicast,
            ports: PORTS.to_vec(),
            timeout: TIMEOUT,
            max_bytes: MAX_BYTES,
        }
    }

    /// Mozilla roots, ring provider, HTTP/1.1.
    pub fn with_webpki_roots() -> anyhow::Result<ImageFetcher> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(ImageFetcher::new(tls))
    }

    /// Test seams: resolver, address policy, ports and limits.
    pub fn with_policy(
        mut self,
        resolver: Arc<dyn Resolver>,
        allowed_ip: fn(IpAddr) -> bool,
        ports: Vec<u16>,
        timeout: Duration,
        max_bytes: usize,
    ) -> ImageFetcher {
        self.resolver = resolver;
        self.allowed_ip = allowed_ip;
        self.ports = ports;
        self.timeout = timeout;
        self.max_bytes = max_bytes;
        self
    }

    /// The address to connect to for `url`, after all checks.
    async fn pin(&self, url: &Url) -> Result<SocketAddr, ProxyError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(blocked(format!("scheme {}", url.scheme())));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(blocked("credentials in URL"));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| blocked("no port"))?;
        if !self.ports.contains(&port) {
            return Err(blocked(format!("port {port}")));
        }
        let ips = match url.host() {
            None => return Err(blocked("no host")),
            Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
            Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
            Some(url::Host::Domain(host)) => self
                .resolver
                .resolve(host.to_owned())
                .await
                .map_err(|e| ProxyError::Upstream(format!("resolving {host}: {e}")))?,
        };
        let ip = *ips
            .first()
            .ok_or_else(|| ProxyError::Upstream("no addresses".to_owned()))?;
        if let Some(bad) = ips.iter().find(|ip| !(self.allowed_ip)(**ip)) {
            return Err(blocked(format!("address {bad}")));
        }
        Ok(SocketAddr::new(ip, port))
    }

    fn client(
        &self,
        url: &Url,
        addr: SocketAddr,
        left: Duration,
    ) -> Result<reqwest::Client, ProxyError> {
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(self.tls.clone())
            .no_proxy()
            .redirect(Policy::none())
            .referer(false)
            .dns_resolver(Arc::new(NoDns))
            .timeout(left)
            .user_agent(USER_AGENT);
        if let Some(url::Host::Domain(host)) = url.host() {
            builder = builder.resolve(host, addr);
        }
        builder
            .build()
            .map_err(|e| ProxyError::Upstream(format!("client: {e}")))
    }

    async fn fetch_hops(
        &self,
        mut url: Url,
        deadline: Instant,
    ) -> Result<ProxiedImage, ProxyError> {
        for hop in 0..=MAX_REDIRECTS {
            let addr = self.pin(&url).await?;
            let left = deadline.saturating_duration_since(Instant::now());
            let client = self.client(&url, addr, left)?;
            let mut resp = client
                .get(url.clone())
                .header(header::ACCEPT, "image/*")
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        ProxyError::Timeout
                    } else {
                        ProxyError::Upstream(e.to_string())
                    }
                })?;
            if resp.status().is_redirection() && resp.status() != StatusCode::NOT_MODIFIED {
                if hop == MAX_REDIRECTS {
                    return Err(ProxyError::Upstream("too many redirects".to_owned()));
                }
                let location = resp
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .ok_or_else(|| ProxyError::Upstream("redirect without Location".to_owned()))?;
                url = url
                    .join(location)
                    .map_err(|_| ProxyError::Upstream("invalid redirect".to_owned()))?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(ProxyError::Upstream(format!("status {}", resp.status())));
            }
            let declared = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !declared
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("image/")
            {
                return Err(ProxyError::Upstream(format!("content type {declared:?}")));
            }
            if resp
                .content_length()
                .is_some_and(|n| n > self.max_bytes as u64)
            {
                return Err(ProxyError::TooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    ProxyError::Timeout
                } else {
                    ProxyError::Upstream(e.to_string())
                }
            })? {
                if bytes.len() + chunk.len() > self.max_bytes {
                    return Err(ProxyError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            let content_type = sniff_image(&bytes)
                .ok_or_else(|| ProxyError::Upstream("not a raster image".to_owned()))?;
            return Ok(ProxiedImage {
                content_type,
                bytes,
            });
        }
        Err(ProxyError::Upstream("too many redirects".to_owned()))
    }

    pub async fn fetch_url(&self, url: &str) -> Result<ProxiedImage, ProxyError> {
        let url = Url::parse(url).map_err(|_| blocked("invalid URL"))?;
        let deadline = Instant::now() + self.timeout;
        tokio::time::timeout_at(deadline, self.fetch_hops(url, deadline))
            .await
            .unwrap_or(Err(ProxyError::Timeout))
    }
}

impl ImageProxy for ImageFetcher {
    fn fetch(&self, url: String) -> BoxFuture<Result<ProxiedImage, ProxyError>> {
        let this = self.clone();
        Box::pin(async move { this.fetch_url(&url).await })
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
}
