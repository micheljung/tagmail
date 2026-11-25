//! In-process HTTP server replaying recorded CalDAV responses.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, Response, StatusCode};

use url::Url;

use crate::env::DiscoveryEnv;
use crate::model::ClientOptions;

#[derive(Clone)]
pub struct Rule {
    method: &'static str,
    host: &'static str,
    path: String,
    body_contains: Option<&'static str>,
    auth: bool,
    challenge: String,
    challenge_headers: Vec<(&'static str, String)>,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Rule {
    pub fn new(method: &'static str, host: &'static str, path: &str) -> Self {
        Rule {
            method,
            host,
            path: path.to_owned(),
            body_contains: None,
            auth: true,
            challenge: String::new(),
            challenge_headers: Vec::new(),
            status: 207,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = body.to_owned();
        self
    }

    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    pub fn when_body(mut self, needle: &'static str) -> Self {
        self.body_contains = Some(needle);
        self
    }

    /// The body sent with the 401 to unauthenticated requests.
    pub fn challenge(mut self, body: &str) -> Self {
        self.challenge = body.to_owned();
        self
    }

    pub fn challenge_header(mut self, name: &'static str, value: &str) -> Self {
        self.challenge_headers.push((name, value.to_owned()));
        self
    }

    pub fn public(mut self) -> Self {
        self.auth = false;
        self
    }

    pub fn redirect(method: &'static str, host: &'static str, path: &str, to: &str) -> Self {
        Rule::new(method, host, path)
            .public()
            .status(301)
            .header("location", to)
    }
}

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub host: String,
    pub path: String,
    pub authorized: bool,
    pub headers: HeaderMap,
    pub body: String,
}

pub struct Mock {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    pub async fn start(rules: Vec<Rule>) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let rules = Arc::new(rules);
        let log = seen.clone();
        let app = Router::new().fallback(move |req: Request<Body>| {
            let rules = rules.clone();
            let log = log.clone();
            async move { answer(&rules, &log, req).await }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Mock { addr, seen }
    }

    pub fn env(&self, hosts: &[&str]) -> DiscoveryEnv {
        hosts.iter().fold(DiscoveryEnv::system(), |env, h| {
            env.with_resolver(h, self.addr)
        })
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn answer(rules: &[Rule], log: &Mutex<Vec<Seen>>, req: Request<Body>) -> Response<Body> {
    let method = req.method().as_str().to_owned();
    let headers = req.headers().clone();
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_owned();
    let path = req.uri().path().to_owned();
    let body = to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&body).into_owned();
    let authorized = headers.contains_key("authorization");
    log.lock().unwrap().push(Seen {
        method: method.clone(),
        host: host.clone(),
        path: path.clone(),
        authorized,
        headers,
        body: body.clone(),
    });
    let rule = rules.iter().find(|r| {
        r.method == method
            && r.host == host
            && r.path == path
            && r.body_contains.is_none_or(|n| body.contains(n))
    });
    let Some(rule) = rule else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };
    if rule.auth && !authorized {
        let mut response = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=\"dav\"");
        for (name, value) in &rule.challenge_headers {
            response = response.header(*name, value);
        }
        return response.body(Body::from(rule.challenge.clone())).unwrap();
    }
    let mut response = Response::builder().status(rule.status);
    if rule.status == StatusCode::MULTI_STATUS.as_u16() {
        response = response.header("content-type", "application/xml; charset=utf-8");
    }
    for (name, value) in &rule.headers {
        response = response.header(*name, value);
    }
    response.body(Body::from(rule.body.clone())).unwrap()
}

pub fn options() -> ClientOptions {
    ClientOptions {
        username: "petition@micheljung.ch".to_owned(),
        password: "secret".to_owned().into(),
        tls: rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth(),
        allow_insecure: true,
    }
}

pub const DAV_HOST: &str = "dav.tagmail.test";

pub fn url(host: &str, path: &str) -> Url {
    Url::parse(&format!("http://{host}{path}")).unwrap()
}

/// Serves the well-known lookups for `domains` over plain http.
pub fn well_known(env: DiscoveryEnv, domains: &[&str]) -> DiscoveryEnv {
    domains.iter().fold(env, |env, d| {
        env.with_base_url(d, Url::parse(&format!("http://{d}/")).unwrap())
    })
}

pub const TLS_HOST: &str = "secure.tagmail.test";

/// An https listener for [`TLS_HOST`] answering every request with a 301
/// to `location`.
pub async fn tls_redirect(location: &'static str) -> SocketAddr {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let cert = CertificateDer::from_pem_slice(fixture!("tls/cert.pem").as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(fixture!("tls/key.pem").as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && tls.read(&mut byte).await.unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let response = format!(
                    "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

/// Trusts only the [`TLS_HOST`] test certificate; http is not allowed.
pub fn tls_options() -> ClientOptions {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(fixture!("tls/cert.pem").as_bytes()).unwrap())
        .unwrap();
    ClientOptions {
        tls: rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
        allow_insecure: false,
        ..options()
    }
}
