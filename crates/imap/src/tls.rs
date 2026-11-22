//! rustls client configuration (ring provider, no OpenSSL).

use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls::{ClientConfig, RootCertStore};

use crate::error::{Error, Result};

/// How the connection is secured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// Implicit TLS (port 993).
    Tls,
    /// Plain connection upgraded with STARTTLS (port 143).
    StartTls,
    /// No encryption. For local test servers only.
    Plain,
}

/// A client config trusting the Mozilla roots plus `extra_roots_pem`
/// (for self-hosted servers with a private CA).
pub fn client_config(extra_roots_pem: Option<&[u8]>) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(pem) = extra_roots_pem {
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| Error::InvalidArgument(format!("CA PEM: {e}")))?;
            roots.add(cert)?;
        }
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(config))
}
