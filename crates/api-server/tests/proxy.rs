//! The image proxy's SSRF guard against a local origin. Loopback stands in
//! for "the internet" via the test address policy; everything else in the
//! policy is the production `is_global_unicast`.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tagmail_api_core::{BoxFuture, ProxyError};
use tagmail_api_server::proxy::{ImageFetcher, MAX_REDIRECTS, Resolver, is_global_unicast};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const LIMIT: usize = 64 * 1024;
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Default)]
struct Seen {
    hits: AtomicUsize,
    headers: Mutex<Vec<HeaderMap>>,
}

#[derive(Clone)]
struct Origin {
    seen: Arc<Seen>,
    port: u16,
}

fn record(o: &Origin, h: &HeaderMap) {
    o.seen.hits.fetch_add(1, Ordering::SeqCst);
    o.seen.headers.lock().unwrap().push(h.clone());
}

fn redirect(to: String) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
}

fn png(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "image/png")], body).into_response()
}

async fn origin() -> Origin {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let o = Origin {
        seen: Arc::default(),
        port,
    };
    let app = Router::new()
        .route(
            "/png",
            get(|State(o): State<Origin>, h: HeaderMap| async move {
                record(&o, &h);
                png(PNG.to_vec())
            }),
        )
        .route(
            "/cookie",
            get(|State(o): State<Origin>, h: HeaderMap| async move {
                record(&o, &h);
                let mut r = redirect("/png".to_owned());
                r.headers_mut()
                    .insert(header::SET_COOKIE, "s=1; Path=/".parse().unwrap());
                r
            }),
        )
        .route(
            "/to-private",
            get(|| async { redirect("http://10.0.0.1/x.png".to_owned()) }),
        )
        .route(
            "/to-metadata",
            get(|| async { redirect("http://169.254.169.254/latest/meta-data".to_owned()) }),
        )
        .route(
            "/to-host",
            get(|State(o): State<Origin>| async move {
                redirect(format!("http://img.test:{}/png", o.port))
            }),
        )
        .route(
            "/loop",
            get(|State(o): State<Origin>, h: HeaderMap| async move {
                record(&o, &h);
                redirect("/loop".to_owned())
            }),
        )
        .route(
            "/big",
            get(|| async {
                let mut body = PNG.to_vec();
                body.resize(LIMIT + 1, 0);
                png(body)
            }),
        )
        .route(
            "/big-stream",
            get(|| async {
                let chunks = (0..(LIMIT / 1024 + 2)).map(|i| {
                    Ok::<_, io::Error>(Bytes::from(if i == 0 {
                        PNG.to_vec()
                    } else {
                        vec![0; 1024]
                    }))
                });
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    Body::from_stream(futures_util::stream::iter(chunks)),
                )
                    .into_response()
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                png(PNG.to_vec())
            }),
        )
        .route(
            "/html",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<script>").into_response() }),
        )
        .route(
            "/fake-png",
            get(|| async { png(b"<svg onload=alert(1)>".to_vec()) }),
        )
        .with_state(o.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    o
}

/// Answers each name with the next list in its script (the last one
/// repeats) and counts lookups.
#[derive(Default)]
struct ScriptedDns {
    answers: Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
    lookups: AtomicUsize,
}

impl ScriptedDns {
    fn with(name: &str, answers: &[&[&str]]) -> Arc<ScriptedDns> {
        let dns = ScriptedDns::default();
        dns.answers.lock().unwrap().insert(
            name.to_owned(),
            answers
                .iter()
                .map(|a| a.iter().map(|ip| ip.parse().unwrap()).collect())
                .collect(),
        );
        Arc::new(dns)
    }
}

impl Resolver for ScriptedDns {
    fn resolve(&self, host: String) -> BoxFuture<io::Result<Vec<IpAddr>>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let mut answers = self.answers.lock().unwrap();
        let out = match answers.get_mut(&host) {
            Some(list) if list.len() > 1 => Ok(list.remove(0)),
            Some(list) => Ok(list[0].clone()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, host)),
        };
        Box::pin(async move { out })
    }
}

fn loopback_is_public(ip: IpAddr) -> bool {
    ip == LOOPBACK || is_global_unicast(ip)
}

fn fetcher(o: &Origin, dns: Arc<ScriptedDns>, timeout: Duration) -> ImageFetcher {
    ImageFetcher::with_webpki_roots().unwrap().with_policy(
        dns,
        loopback_is_public,
        vec![80, 443, o.port],
        timeout,
        LIMIT,
    )
}

fn at(o: &Origin, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", o.port)
}

#[tokio::test]
async fn fetches_images_without_cookies_or_referer() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    let image = f.fetch_url(&at(&o, "/cookie")).await.unwrap();
    assert_eq!(image.content_type, "image/png");
    assert_eq!(image.bytes, PNG);
    let seen = o.seen.headers.lock().unwrap();
    assert_eq!(seen.len(), 2, "redirect followed once");
    for h in seen.iter() {
        assert!(h.get(header::COOKIE).is_none(), "{h:?}");
        assert!(h.get(header::REFERER).is_none(), "{h:?}");
        assert_eq!(h[header::USER_AGENT], "Tagmail-ImageProxy");
    }
}

#[tokio::test]
async fn connections_are_pinned_to_the_checked_address() {
    let o = origin().await;
    let dns = ScriptedDns::with("img.test", &[&["127.0.0.1"], &["10.0.0.5"]]);
    let f = fetcher(&o, dns.clone(), Duration::from_secs(5));
    let url = format!("http://img.test:{}/png", o.port);
    assert_eq!(f.fetch_url(&url).await.unwrap().bytes, PNG);
    assert_eq!(
        dns.lookups.load(Ordering::SeqCst),
        1,
        "resolved once; reqwest never re-resolves"
    );
}

#[tokio::test]
async fn dns_rebinding_on_a_redirect_is_blocked() {
    let o = origin().await;
    let dns = ScriptedDns::with("img.test", &[&["127.0.0.1"], &["10.0.0.5"]]);
    let f = fetcher(&o, dns.clone(), Duration::from_secs(5));
    let url = format!("http://img.test:{}/to-host", o.port);
    assert_eq!(
        f.fetch_url(&url).await,
        Err(ProxyError::Blocked("address 10.0.0.5".to_owned()))
    );
    assert_eq!(dns.lookups.load(Ordering::SeqCst), 2, "rechecked per hop");
}

#[tokio::test]
async fn redirects_to_internal_addresses_are_blocked() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    for (path, ip) in [
        ("/to-private", "10.0.0.1"),
        ("/to-metadata", "169.254.169.254"),
    ] {
        assert_eq!(
            f.fetch_url(&at(&o, path)).await,
            Err(ProxyError::Blocked(format!("address {ip}"))),
            "{path}"
        );
    }
}

#[tokio::test]
async fn any_blocked_address_in_an_answer_blocks_the_host() {
    let o = origin().await;
    let dns = ScriptedDns::with("mixed.test", &[&["127.0.0.1", "192.168.0.10"]]);
    let f = fetcher(&o, dns, Duration::from_secs(5));
    let url = format!("http://mixed.test:{}/png", o.port);
    assert_eq!(
        f.fetch_url(&url).await,
        Err(ProxyError::Blocked("address 192.168.0.10".to_owned()))
    );
    assert_eq!(o.seen.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redirects_are_capped() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    assert_eq!(
        f.fetch_url(&at(&o, "/loop")).await,
        Err(ProxyError::Upstream("too many redirects".to_owned()))
    );
    assert_eq!(o.seen.hits.load(Ordering::SeqCst), MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn size_is_capped_with_and_without_content_length() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    for path in ["/big", "/big-stream"] {
        assert_eq!(
            f.fetch_url(&at(&o, path)).await,
            Err(ProxyError::TooLarge),
            "{path}"
        );
    }
}

#[tokio::test]
async fn slow_origins_time_out() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_millis(300));
    let start = Instant::now();
    assert_eq!(
        f.fetch_url(&at(&o, "/slow")).await,
        Err(ProxyError::Timeout)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn only_raster_images_pass() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    assert!(matches!(
        f.fetch_url(&at(&o, "/html")).await,
        Err(ProxyError::Upstream(e)) if e.contains("content type")
    ));
    assert_eq!(
        f.fetch_url(&at(&o, "/fake-png")).await,
        Err(ProxyError::Upstream("not a raster image".to_owned()))
    );
}

#[tokio::test]
async fn production_policy_blocks_before_connecting() {
    let f = ImageFetcher::with_webpki_roots().unwrap();
    for (url, why) in [
        ("http://127.0.0.1/x.png", "address 127.0.0.1"),
        ("http://[::1]/x.png", "address ::1"),
        ("http://[::ffff:7f00:1]/x.png", "address ::ffff:127.0.0.1"),
        ("http://0x7f.1/x.png", "address 127.0.0.1"),
        ("http://2130706433/x.png", "address 127.0.0.1"),
        ("http://169.254.169.254/latest", "address 169.254.169.254"),
        ("http://example.com:8080/x.png", "port 8080"),
        ("https://example.com:22/x.png", "port 22"),
        ("ftp://example.com/x.png", "scheme ftp"),
        ("file:///etc/passwd", "scheme file"),
        ("http://user:pw@example.com/x.png", "credentials in URL"),
        ("not a url", "invalid URL"),
    ] {
        assert_eq!(
            f.fetch_url(url).await,
            Err(ProxyError::Blocked(why.to_owned())),
            "{url}"
        );
    }
    let local = f.fetch_url("http://localhost/x.png").await;
    assert!(
        matches!(&local, Err(ProxyError::Blocked(why)) if why.starts_with("address ")),
        "{local:?}"
    );
}

#[tokio::test]
async fn unpinned_hosts_never_resolve() {
    let o = origin().await;
    let f = fetcher(&o, Arc::default(), Duration::from_secs(5));
    let url = format!("http://unknown.test:{}/png", o.port);
    assert!(matches!(
        f.fetch_url(&url).await,
        Err(ProxyError::Upstream(e)) if e.contains("resolving unknown.test")
    ));
    assert_eq!(o.seen.hits.load(Ordering::SeqCst), 0);
}
