//! The remote image proxy behind `/api/proxy/img/{token}` (plan §3.9),
//! hardened against SSRF: it only ever fetches URLs the server signed;
//! only `http`/`https` on ports 80/443; each hop's host is resolved once by
//! our resolver, every address must be global unicast, and the connection
//! is pinned to the checked address (reqwest never resolves on its own);
//! redirects are followed by hand, at most three, each checked the same
//! way; 5 s for the whole fetch; at most 5 MB, streamed; the answer must
//! say `image/*` and start with a raster image signature; no cookies, no
//! Referer, no system proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{StatusCode, Url, header};
use tagmail_api_core::mime::sniff_image;
use tagmail_api_core::{BoxFuture, ImageProxy, ProxiedImage, ProxyError};
use tokio::time::Instant;

use crate::outbound::{self, PinError};
pub use crate::outbound::{Resolver, SystemResolver, is_global_unicast};

pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 3;
pub const PORTS: &[u16] = &[80, 443];
const USER_AGENT: &str = "Tagmail-ImageProxy";

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
        Ok(ImageFetcher::new(outbound::webpki_tls()?))
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
        outbound::pin(url, &*self.resolver, self.allowed_ip, |port| {
            self.ports.contains(&port)
        })
        .await
        .map_err(|e| match e {
            PinError::Blocked(why) => ProxyError::Blocked(why),
            PinError::NoSuchHost(why) | PinError::Unresolved(why) => ProxyError::Upstream(why),
        })
    }

    fn client(
        &self,
        url: &Url,
        addr: SocketAddr,
        left: Duration,
    ) -> Result<reqwest::Client, ProxyError> {
        outbound::pinned_client(self.tls.clone(), url, addr, left, USER_AGENT)
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
