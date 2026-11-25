use std::collections::HashMap;
use std::net::SocketAddr;

use url::Url;

/// Where discovery and the client look things up. Production code builds it
/// only through [`DiscoveryEnv::system`]; the overrides exist for in-process
/// tests and are compiled out of release builds.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryEnv {
    base_urls: HashMap<String, Url>,
    resolve: Vec<(String, SocketAddr)>,
    disabled_reports: Vec<String>,
    disabled_properties: Vec<String>,
}

impl DiscoveryEnv {
    /// The system resolver and `https://<domain>/.well-known/caldav`.
    pub fn system() -> Self {
        Self::default()
    }

    pub(crate) fn well_known_base(&self, domain: &str) -> Option<Url> {
        match self.base_urls.get(&domain.to_ascii_lowercase()) {
            Some(base) => Some(base.clone()),
            None => Url::parse(&format!("https://{domain}/")).ok(),
        }
    }

    pub(crate) fn resolve_overrides(&self) -> &[(String, SocketAddr)] {
        &self.resolve
    }

    pub(crate) fn report_disabled(&self, report: &str) -> bool {
        self.disabled_reports.iter().any(|r| r == report)
    }

    pub(crate) fn property_disabled(&self, property: &str) -> bool {
        self.disabled_properties.iter().any(|p| p == property)
    }
}

#[cfg(any(test, feature = "caldav-tests"))]
impl DiscoveryEnv {
    /// Serves the well-known lookup for `domain` from `base` instead of
    /// `https://<domain>/`.
    pub fn with_base_url(mut self, domain: &str, base: Url) -> Self {
        self.base_urls.insert(domain.to_ascii_lowercase(), base);
        self
    }

    /// Connects to `addr` whenever a URL names `host`.
    pub fn with_resolver(mut self, host: &str, addr: SocketAddr) -> Self {
        self.resolve.push((host.to_ascii_lowercase(), addr));
        self
    }

    /// Behaves as if no calendar's `supported-report-set` listed `reports`.
    pub fn with_disabled_reports(mut self, reports: &[&str]) -> Self {
        self.disabled_reports
            .extend(reports.iter().map(|r| (*r).to_owned()));
        self
    }

    /// Behaves as if the server answered 404 for `properties` (by local
    /// name, e.g. `getctag`, `sync-token`) on every collection.
    pub fn with_disabled_properties(mut self, properties: &[&str]) -> Self {
        self.disabled_properties
            .extend(properties.iter().map(|p| (*p).to_owned()));
        self
    }
}
