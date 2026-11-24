//! HTTP client for the paired server: bearer device token, 5 s budget per
//! request, and the online state the composite store routes on (§7).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tagmail_api_core::dto::{ApiErrorDto, VersionDto};
use thiserror::Error;
use url::Url;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest silence on any read, streams included: three server SSE
/// heartbeats.
pub const STREAM_IDLE: Duration = Duration::from_secs(75);
/// The server reads a whole uncached message from IMAP before answering.
pub const RAW_IDLE: Duration = Duration::from_secs(5 * 60);
pub const RAW_DEADLINE: Duration = Duration::from_secs(15 * 60);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// A success this recent counts as online.
pub const ONLINE_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum ClientError {
    /// No answer: connection refused, DNS, TLS or timeout.
    #[error("server unreachable: {0}")]
    Offline(String),
    /// The device token is missing or revoked.
    #[error("device is not paired")]
    Unauthorized,
    /// The server's cache epoch changed; the replica must bootstrap again.
    #[error("server cache epoch changed")]
    Gone,
    #[error("server answered {status}: {}", .body.message)]
    Api { status: u16, body: ApiErrorDto },
    #[error("server answered {status}")]
    Status { status: u16 },
    #[error("unexpected server response: {0}")]
    Decode(String),
    #[error("the server address must use https: {0}")]
    Insecure(String),
}

/// Plain http only reaches this machine.
pub fn is_secure(server: &Url) -> bool {
    match (server.scheme(), server.host()) {
        ("https", _) => true,
        ("http", Some(url::Host::Domain(d))) => d.eq_ignore_ascii_case("localhost"),
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
    }
}

pub type ClientResult<T> = Result<T, ClientError>;

/// When the server last answered. Any HTTP answer counts, errors included:
/// the server was reachable.
#[derive(Debug, Default)]
pub struct Link {
    last_ok: Mutex<Option<Instant>>,
}

impl Link {
    pub fn mark_ok(&self) {
        *self.lock() = Some(Instant::now());
    }

    pub fn mark_down(&self) {
        *self.lock() = None;
    }

    pub fn is_online(&self) -> bool {
        self.lock().is_some_and(|at| at.elapsed() < ONLINE_WINDOW)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.last_ok.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct ServerClient {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
    raw: reqwest::Client,
    link: Arc<Link>,
}

impl ServerClient {
    pub fn new(server: &Url, token: Option<String>) -> ClientResult<ServerClient> {
        ServerClient::with_idle(server, token, STREAM_IDLE)
    }

    /// A client that gives up on a connection silent for `idle`.
    pub fn with_idle(
        server: &Url,
        token: Option<String>,
        idle: Duration,
    ) -> ClientResult<ServerClient> {
        if !is_secure(server) {
            return Err(ClientError::Insecure(server.to_string()));
        }
        let mut base = server.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| ClientError::Decode(format!("tls: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let http = |idle: Duration| {
            reqwest::Client::builder()
                .tls_backend_preconfigured(tls.clone())
                .connect_timeout(REQUEST_TIMEOUT)
                .read_timeout(idle)
                .tcp_keepalive(TCP_KEEPALIVE)
                .user_agent(concat!("tagmail-desktop/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| ClientError::Decode(format!("client: {e}")))
        };
        Ok(ServerClient {
            base,
            token,
            http: http(idle)?,
            raw: http(RAW_IDLE.max(idle))?,
            link: Arc::default(),
        })
    }

    pub fn server(&self) -> &Url {
        &self.base
    }

    pub fn link(&self) -> &Arc<Link> {
        &self.link
    }

    pub fn is_online(&self) -> bool {
        self.link.is_online()
    }

    /// A request without the 5 s budget, for streamed bodies.
    pub fn request(&self, method: Method, path: &str) -> ClientResult<RequestBuilder> {
        self.request_on(&self.http, method, path)
    }

    /// A raw message download, which may be silent for [`RAW_IDLE`] and
    /// takes at most [`RAW_DEADLINE`].
    pub fn raw_request(&self, method: Method, path: &str) -> ClientResult<RequestBuilder> {
        Ok(self
            .request_on(&self.raw, method, path)?
            .timeout(RAW_DEADLINE))
    }

    fn request_on(
        &self,
        http: &reqwest::Client,
        method: Method,
        path: &str,
    ) -> ClientResult<RequestBuilder> {
        let url = self
            .base
            .join(path.trim_start_matches('/'))
            .map_err(|e| ClientError::Decode(format!("path {path:?}: {e}")))?;
        let builder = http.request(method, url);
        Ok(match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        })
    }

    /// Sends and maps non-success statuses to errors.
    pub async fn send(&self, builder: RequestBuilder) -> ClientResult<Response> {
        let resp = match builder.send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.link.mark_down();
                return Err(ClientError::Offline(e.to_string()));
            }
        };
        self.link.mark_ok();
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        Err(match status {
            StatusCode::UNAUTHORIZED => ClientError::Unauthorized,
            StatusCode::GONE => ClientError::Gone,
            _ => match resp.json::<ApiErrorDto>().await {
                Ok(body) => ClientError::Api {
                    status: status.as_u16(),
                    body,
                },
                Err(_) => ClientError::Status {
                    status: status.as_u16(),
                },
            },
        })
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> ClientResult<T> {
        let builder = self.request(Method::GET, path)?.timeout(REQUEST_TIMEOUT);
        self.json(builder).await
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<T> {
        let builder = self
            .request(Method::POST, path)?
            .timeout(REQUEST_TIMEOUT)
            .json(body);
        self.json(builder).await
    }

    async fn json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> ClientResult<T> {
        let resp = self.send(builder).await?;
        match resp.bytes().await {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
            }
            Err(e) => {
                self.link.mark_down();
                Err(ClientError::Offline(e.to_string()))
            }
        }
    }

    /// Sends and returns any answer as status, content type and body;
    /// only a missing answer is an error.
    pub async fn exchange(
        &self,
        builder: RequestBuilder,
    ) -> ClientResult<(u16, Option<String>, Vec<u8>)> {
        let resp = match builder.send().await {
            Ok(resp) => resp,
            Err(e) => {
                self.link.mark_down();
                return Err(ClientError::Offline(e.to_string()));
            }
        };
        self.link.mark_ok();
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match resp.bytes().await {
            Ok(body) => Ok((status, content_type, body.to_vec())),
            Err(e) => {
                self.link.mark_down();
                Err(ClientError::Offline(e.to_string()))
            }
        }
    }

    /// Asks the server whether it answers, briefly.
    pub async fn probe(&self) -> bool {
        match self.request(Method::GET, "api/version") {
            Ok(b) => self.exchange(b.timeout(PROBE_TIMEOUT)).await.is_ok(),
            Err(_) => false,
        }
    }

    pub async fn version(&self) -> ClientResult<VersionDto> {
        self.get_json("api/version").await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::*;

    /// Answers each connection with the next canned response and reports
    /// the request head.
    pub(crate) async fn canned(
        responses: Vec<(u16, &'static str)>,
    ) -> (Url, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (seen, heads) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                let _ = seen.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        (url, heads)
    }

    /// Answers with `head` and the start of a body, then stays silent
    /// with the connection open.
    pub(crate) async fn stalled(head: &'static str, body: &'static str) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(format!("HTTP/1.1 200 OK\r\n{head}\r\n{body}").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        url
    }

    async fn closed_port() -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        url
    }

    #[tokio::test]
    async fn sends_the_bearer_token_and_goes_online() {
        let (url, mut heads) = canned(vec![(
            200,
            r#"{"api":{"major":1,"minor":1},"minClient":{"major":1,"minor":0},"server":"0.1"}"#,
        )])
        .await;
        let client = ServerClient::new(&url, Some("tok".into())).unwrap();
        assert!(!client.is_online());

        let version = client.version().await.unwrap();

        assert_eq!(version.api.minor, 1);
        assert!(client.is_online());
        let head = heads.recv().await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /api/version "), "{head}");
        assert!(head.contains("authorization: bearer tok"), "{head}");
    }

    #[tokio::test]
    async fn statuses_map_to_errors_and_still_count_as_reachable() {
        let (url, _) = canned(vec![
            (401, ""),
            (410, r#"{"error":"gone","message":"epoch"}"#),
            (503, r#"{"error":"busy","message":"later"}"#),
            (500, "oops"),
        ])
        .await;
        let client = ServerClient::new(&url, None).unwrap();

        let unauthorized = client.version().await.unwrap_err();
        let gone = client.version().await.unwrap_err();
        let api = client.version().await.unwrap_err();
        let status = client.version().await.unwrap_err();

        assert!(matches!(unauthorized, ClientError::Unauthorized));
        assert!(matches!(gone, ClientError::Gone));
        assert!(
            matches!(&api, ClientError::Api { status: 503, body } if body.error == "busy"),
            "{api:?}"
        );
        assert!(matches!(status, ClientError::Status { status: 500 }));
        assert!(client.is_online());
    }

    #[tokio::test]
    async fn unreachable_server_flips_offline() {
        let (url, _) = canned(vec![(
            200,
            r#"{"api":{"major":1,"minor":1},"minClient":{"major":1,"minor":0},"server":"x"}"#,
        )])
        .await;
        let client = ServerClient::new(&url, None).unwrap();
        client.version().await.unwrap();
        assert!(client.is_online());

        let down = ServerClient {
            base: closed_port().await,
            ..client.clone()
        };
        assert!(matches!(down.version().await, Err(ClientError::Offline(_))));
        assert!(!client.is_online());
    }

    #[tokio::test]
    async fn a_silent_server_times_out_offline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let client = ServerClient::new(&url, None).unwrap();
        client.link().mark_ok();

        let started = Instant::now();
        let err = client.version().await.unwrap_err();

        assert!(matches!(err, ClientError::Offline(_)), "{err:?}");
        assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(2));
        assert!(!client.is_online());
    }

    #[test]
    fn keeps_a_path_prefix() {
        let client =
            ServerClient::new(&Url::parse("https://example.org/tagmail").unwrap(), None).unwrap();
        let req = client
            .request(Method::GET, "/api/version")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://example.org/tagmail/api/version"
        );
    }

    #[test]
    fn plain_http_only_to_this_machine() {
        let secure = |s: &str| is_secure(&Url::parse(s).unwrap());
        assert!(secure("https://mail.example"));
        assert!(secure("http://localhost:8080"));
        assert!(secure("http://127.0.0.2/"));
        assert!(secure("http://[::1]:9000/"));
        assert!(!secure("http://mail.example"));
        assert!(!secure("http://192.168.1.2"));
        assert!(!secure("http://localhost.evil.example"));
        assert!(!secure("ftp://localhost"));
        assert!(matches!(
            ServerClient::new(&Url::parse("http://mail.example").unwrap(), None),
            Err(ClientError::Insecure(_))
        ));
    }
}
