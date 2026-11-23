//! Message reading over [`Api`]: render document and its CSP, signed part
//! and proxy URLs, fetch on open, the "always show images" list and the
//! thread detail.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_core::dto::{AddressDto, LabelDto, MailboxDto, ThreadRowDto};
use tagmail_api_core::messages::{PART_CSP, RENDER_CSP};
use tagmail_api_core::mime::BLOCKED_IMAGE;
use tagmail_api_core::signing::{self, Capability, TOKEN_TTL_SECS, UrlKeys};
use tagmail_api_core::{
    AccountRegistry, AccountSummary, Api, BoxFuture, ChangePage, FetchRawError, ImageProxy,
    MessageRecord, Method, NewAccount, ProxiedImage, ProxyError, Request, Response, RuntimeStatus,
    Store, StoreResult, ThreadDetail, ThreadRows, View, ViewIndex,
};
use tagmail_core::{AccountId, CacheEpoch, ChangeCursor, MessageRowId, ThreadId};
use zeroize::Zeroizing;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUg==";
/// Raw too large for the engine's cap.
const HUGE: i64 = 99;

fn raw_html() -> String {
    format!(
        concat!(
            "From: Ann <ann@example.com>\r\n",
            "To: me@example.com\r\n",
            "Reply-To: list@example.com\r\n",
            "Message-ID: <m1@example.com>\r\n",
            "Subject: News\r\n",
            "Content-Type: multipart/mixed; boundary=\"m\"\r\n",
            "\r\n",
            "--m\r\n",
            "Content-Type: multipart/related; boundary=\"r\"\r\n",
            "\r\n",
            "--r\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><head><meta http-equiv=refresh content=\"0;url=https://evil.example\">",
            "<base href=\"https://evil.example/\"></head><body>",
            "<script>alert(1)</script><p onclick=\"x()\">Hello</p>",
            "<img src=\"cid:logo@x\"><img src=\"https://cdn.example/pixel.gif\">",
            "<a href=\"javascript:alert(1)\">bad</a><a href=\"https://ok.example\">ok</a>",
            "</body></html>\r\n",
            "--r\r\n",
            "Content-Type: image/png\r\n",
            "Content-ID: <logo@x>\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "{png}\r\n",
            "--r--\r\n",
            "--m\r\n",
            "Content-Type: text/html; name=\"evil.html\"\r\n",
            "Content-Disposition: attachment; filename=\"evil.html\"\r\n",
            "\r\n",
            "<script>alert(1)</script>\r\n",
            "--m--\r\n",
        ),
        png = PNG_B64
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn record(id: i64, from: &str) -> MessageRecord {
    MessageRecord {
        id: MessageRowId(id),
        account_id: AccountId(1),
        thread_id: ThreadId(7),
        date: 1_790_000_000 + id,
        subject: Some(format!("subject {id}")),
        snippet: format!("snippet {id}"),
        from: vec![AddressDto {
            name: Some("Ann".to_owned()),
            email: from.to_owned(),
        }],
        to: vec![],
        cc: vec![],
        unread: id == 1,
        starred: false,
        has_attachment: true,
    }
}

struct Mail {
    records: Vec<MessageRecord>,
    cached: Arc<Mutex<HashMap<i64, Vec<u8>>>>,
    /// What the IMAP server has.
    server: HashMap<i64, Vec<u8>>,
    senders: Mutex<Vec<String>>,
    keys: UrlKeys,
}

impl Mail {
    fn new() -> Mail {
        Mail {
            records: vec![record(1, "ann@example.com"), record(2, "bob@example.com")],
            cached: Arc::default(),
            server: HashMap::from([
                (1, raw_html().into_bytes()),
                (
                    2,
                    b"Subject: plain\r\n\r\nsee https://ok.example\r\n".to_vec(),
                ),
            ]),
            senders: Mutex::default(),
            keys: UrlKeys {
                current: Zeroizing::new([7; signing::KEY_LEN]),
                previous: None,
            },
        }
    }
}

const EPOCH: ChangeCursor = ChangeCursor {
    epoch: CacheEpoch(1),
    seq: 0,
};

impl Store for Mail {
    async fn head(&self) -> StoreResult<ChangeCursor> {
        Ok(EPOCH)
    }

    async fn view_index(&self, _: View) -> StoreResult<ViewIndex> {
        Ok(ViewIndex {
            cursor: EPOCH,
            ids: vec![],
        })
    }

    async fn mailboxes(&self) -> StoreResult<Vec<MailboxDto>> {
        Ok(vec![])
    }

    async fn labels(&self) -> StoreResult<Vec<LabelDto>> {
        Ok(vec![])
    }

    async fn threads(&self, _: Vec<ThreadId>) -> StoreResult<ThreadRows> {
        Ok(ThreadRows {
            cursor: EPOCH,
            rows: vec![],
        })
    }

    async fn changes_since(&self, _: ChangeCursor, _: usize) -> StoreResult<ChangePage> {
        Ok(ChangePage::Gone)
    }

    async fn accounts(&self) -> StoreResult<Vec<AccountSummary>> {
        Ok(vec![])
    }

    async fn create_account(&self, _: NewAccount) -> StoreResult<AccountId> {
        Err("read-only".into())
    }

    async fn message(&self, id: MessageRowId) -> StoreResult<Option<MessageRecord>> {
        if id.0 == HUGE {
            return Ok(Some(record(HUGE, "x@example.com")));
        }
        Ok(self.records.iter().find(|r| r.id == id).cloned())
    }

    async fn thread_detail(&self, id: ThreadId) -> StoreResult<Option<ThreadDetail>> {
        Ok(
            (id == ThreadId(7) || id == ThreadId(3)).then(|| ThreadDetail {
                row: ThreadRowDto {
                    id: 7,
                    account_id: 1,
                    subject: "News".to_owned(),
                    snippet: String::new(),
                    last_date: 1_790_000_002,
                    message_count: 2,
                    unread_count: 1,
                    starred: false,
                    has_attachment: true,
                    senders: vec![],
                    labels: vec![],
                    mailbox_ids: vec![],
                },
                messages: self.records.clone(),
            }),
        )
    }

    async fn raw_message(&self, id: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.cached.lock().unwrap().get(&id.0).cloned())
    }

    async fn url_keys(&self) -> StoreResult<UrlKeys> {
        Ok(self.keys.clone())
    }

    async fn image_senders(&self) -> StoreResult<Vec<String>> {
        Ok(self.senders.lock().unwrap().clone())
    }

    async fn set_image_sender(&self, address: String, allow: bool) -> StoreResult<()> {
        let mut senders = self.senders.lock().unwrap();
        senders.retain(|s| *s != address);
        if allow {
            senders.push(address);
        }
        Ok(())
    }
}

struct Engine {
    cached: Arc<Mutex<HashMap<i64, Vec<u8>>>>,
    server: HashMap<i64, Vec<u8>>,
    fetches: AtomicUsize,
}

impl AccountRegistry for Engine {
    fn account_added(&self, _: AccountId) {}

    fn runtime_status(&self, _: AccountId) -> Option<RuntimeStatus> {
        None
    }

    fn fetch_raw(&self, _: AccountId, id: MessageRowId) -> BoxFuture<Result<(), FetchRawError>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let result = match self.server.get(&id.0) {
            _ if id.0 == HUGE => Err(FetchRawError::TooLarge),
            Some(raw) => {
                self.cached.lock().unwrap().insert(id.0, raw.clone());
                Ok(())
            }
            None => Err(FetchRawError::NotFound),
        };
        Box::pin(async move { result })
    }
}

struct Proxy;

impl ImageProxy for Proxy {
    fn fetch(&self, url: String) -> BoxFuture<Result<ProxiedImage, ProxyError>> {
        Box::pin(async move {
            if url == "https://cdn.example/pixel.gif" {
                Ok(ProxiedImage {
                    content_type: "image/png",
                    bytes: PNG.to_vec(),
                })
            } else {
                Err(ProxyError::Blocked(url))
            }
        })
    }
}

struct Fixture {
    api: Api<Mail>,
    mail: Arc<Mail>,
    engine: Arc<Engine>,
}

fn fixture() -> Fixture {
    let mail = Arc::new(Mail::new());
    let engine = Arc::new(Engine {
        cached: mail.cached.clone(),
        server: mail.server.clone(),
        fetches: AtomicUsize::new(0),
    });
    let api = Api::new(
        mail.clone(),
        Some(engine.clone() as Arc<dyn AccountRegistry>),
    )
    .with_image_proxy(Arc::new(Proxy));
    Fixture { api, mail, engine }
}

async fn call(api: &Api<Mail>, method: Method, path: &str, body: &[u8]) -> Response {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    api.handle(Request {
        method,
        path,
        query,
        body,
    })
    .await
}

async fn get(api: &Api<Mail>, path: &str) -> Response {
    call(api, Method::Get, path, b"").await
}

fn json(resp: &Response) -> Value {
    serde_json::from_slice(&resp.body).unwrap()
}

fn text(resp: &Response) -> String {
    String::from_utf8(resp.body.clone()).unwrap()
}

/// The `/api/...` URLs in a rendered document with `prefix`.
fn urls(html: &str, prefix: &str) -> Vec<String> {
    html.match_indices(prefix)
        .map(|(i, _)| html[i..].split('"').next().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn render_serves_a_sanitized_document_under_a_strict_csp() {
    let f = fixture();
    let resp = get(&f.api, "/api/messages/1/render").await;
    assert_eq!(resp.status, 200, "{}", text(&resp));
    assert_eq!(
        resp.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(resp.header("content-security-policy"), Some(RENDER_CSP));
    for directive in [
        "default-src 'none'",
        "img-src 'self' data:",
        "style-src 'unsafe-inline'",
        "script-src 'none'",
        "frame-ancestors 'self'",
    ] {
        assert!(RENDER_CSP.contains(directive), "{directive}");
    }
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(resp.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(resp.header("cache-control"), Some("no-store"));
    let html = text(&resp);
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<p>Hello</p>"), "{html}");
    for bad in [
        "<script",
        "onclick",
        "javascript:",
        "evil.example",
        "http-equiv",
        "<base",
        "cdn.example",
    ] {
        assert!(!html.contains(bad), "{bad} in {html}");
    }
    assert!(
        html.contains(BLOCKED_IMAGE),
        "remote images blocked by default"
    );
    assert!(html.contains(
        "<a href=\"https://ok.example\" target=\"_blank\" rel=\"noopener noreferrer\">ok</a>"
    ));
    assert_eq!(
        f.engine.fetches.load(Ordering::SeqCst),
        1,
        "raw fetched on open"
    );
    get(&f.api, "/api/messages/1/render").await;
    assert_eq!(
        f.engine.fetches.load(Ordering::SeqCst),
        1,
        "then served from the cache"
    );
}

#[tokio::test]
async fn cid_images_load_through_signed_part_urls_only() {
    let f = fixture();
    let html = text(&get(&f.api, "/api/messages/1/render").await);
    let parts = urls(&html, "/api/parts/");
    assert_eq!(parts.len(), 1, "{html}");

    let resp = get(&f.api, &parts[0]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("image/png"));
    assert_eq!(resp.header("content-disposition"), Some("inline"));
    assert_eq!(resp.header("content-security-policy"), Some(PART_CSP));
    assert_eq!(resp.body, PNG);

    let token = parts[0].trim_start_matches("/api/parts/");
    let (payload, tag) = token.split_once('.').unwrap();
    let mut forged_tag = tag.to_owned();
    forged_tag.replace_range(..1, if tag.starts_with('A') { "B" } else { "A" });
    let other_key = UrlKeys {
        current: Zeroizing::new([8; signing::KEY_LEN]),
        previous: None,
    };
    let cap = Capability::Part {
        message: MessageRowId(1),
        part: signing::verify(&f.mail.keys, token, now())
            .map(|c| match c {
                Capability::Part { part, .. } => part,
                Capability::Image { .. } => unreachable!(),
            })
            .unwrap(),
    };
    let expired = signing::sign(&f.mail.keys, &cap, now() - TOKEN_TTL_SECS - 5);
    let wrong_key = signing::sign(&other_key, &cap, now());
    let image_token = signing::sign(
        &f.mail.keys,
        &Capability::Image {
            url: "https://cdn.example/pixel.gif".into(),
        },
        now(),
    );
    for bad in [
        format!("{payload}.{forged_tag}"),
        expired,
        wrong_key,
        image_token,
        "garbage".to_owned(),
    ] {
        let resp = get(&f.api, &format!("/api/parts/{bad}")).await;
        assert_eq!(resp.status, 403, "{bad}");
        assert_eq!(json(&resp)["error"], "forbidden");
    }
}

#[tokio::test]
async fn message_detail_lists_parts_with_signed_urls() {
    let f = fixture();
    let resp = get(&f.api, "/api/messages/1").await;
    assert_eq!(resp.status, 200, "{}", text(&resp));
    let body = json(&resp);
    assert_eq!(body["subject"], "subject 1");
    assert_eq!(body["body"], "html");
    assert_eq!(body["remoteImages"], 1);
    assert_eq!(body["imagesAllowed"], false);
    assert_eq!(body["messageId"], "m1@example.com");
    assert_eq!(
        body["replyTo"],
        json!([{ "name": null, "email": "list@example.com" }])
    );
    let parts = body["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "{parts:?}");
    let logo = parts
        .iter()
        .find(|p| p["contentType"] == "image/png")
        .unwrap();
    assert_eq!(logo["inline"], true);
    let evil = parts
        .iter()
        .find(|p| p["contentType"] == "text/html")
        .unwrap();
    assert_eq!(evil["inline"], false);
    assert_eq!(evil["name"], "evil.html");

    let resp = get(&f.api, evil["url"].as_str().unwrap()).await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(
        resp.header("content-disposition"),
        Some("attachment; filename*=UTF-8''evil.html")
    );
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(resp.header("content-security-policy"), Some(PART_CSP));

    let logo_url = format!("{}?download=1", logo["url"].as_str().unwrap());
    let resp = get(&f.api, &logo_url).await;
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(resp.header("content-disposition"), Some("attachment"));
}

#[tokio::test]
async fn show_images_proxies_through_signed_urls() {
    let f = fixture();
    let html = text(&get(&f.api, "/api/messages/1/render?images=1").await);
    assert!(!html.contains(BLOCKED_IMAGE));
    assert!(!html.contains("cdn.example"));
    let proxied = urls(&html, "/api/proxy/img/");
    assert_eq!(proxied.len(), 1, "{html}");
    let resp = get(&f.api, &proxied[0]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("image/png"));
    assert_eq!(resp.header("content-security-policy"), Some(PART_CSP));

    let forged = signing::sign(
        &UrlKeys {
            current: Zeroizing::new([9; signing::KEY_LEN]),
            previous: None,
        },
        &Capability::Image {
            url: "http://169.254.169.254/".into(),
        },
        now(),
    );
    assert_eq!(
        get(&f.api, &format!("/api/proxy/img/{forged}"))
            .await
            .status,
        403
    );
    assert_eq!(
        get(&f.api, "/api/messages/1/render?images=2").await.status,
        400
    );

    let no_proxy = Api::new(f.mail.clone(), None);
    assert_eq!(get(&no_proxy, &proxied[0]).await.status, 503);
}

#[tokio::test]
async fn senders_on_the_list_always_show_images() {
    let f = fixture();
    let set = |address: &str, allow: bool| {
        serde_json::to_vec(&json!({ "address": address, "allow": allow })).unwrap()
    };
    let resp = call(
        &f.api,
        Method::Post,
        "/api/image-senders",
        &set(" Ann@Example.COM ", true),
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(json(&resp)["addresses"], json!(["ann@example.com"]));

    let html = text(&get(&f.api, "/api/messages/1/render").await);
    assert_eq!(urls(&html, "/api/proxy/img/").len(), 1, "{html}");
    assert_eq!(
        json(&get(&f.api, "/api/messages/1").await)["imagesAllowed"],
        true
    );
    let html = text(&get(&f.api, "/api/messages/1/render?images=0").await);
    assert_eq!(
        urls(&html, "/api/proxy/img/").len(),
        1,
        "list wins over images=0"
    );

    for bad in ["", "no-at", "@x", "a@", "a b@x"] {
        let resp = call(&f.api, Method::Post, "/api/image-senders", &set(bad, true)).await;
        assert_eq!(resp.status, 422, "{bad}");
    }
    let resp = call(
        &f.api,
        Method::Post,
        "/api/image-senders",
        &set("ann@example.com", false),
    )
    .await;
    assert_eq!(json(&resp)["addresses"], json!([]));
    assert_eq!(
        json(&get(&f.api, "/api/image-senders").await)["addresses"],
        json!([])
    );
}

#[tokio::test]
async fn plain_text_and_fetch_failures() {
    let f = fixture();
    let html = text(&get(&f.api, "/api/messages/2/render").await);
    assert!(
        html.contains("<div style=\"white-space:pre-wrap\">see <a href=\"https://ok.example\""),
        "{html}"
    );
    assert_eq!(json(&get(&f.api, "/api/messages/2").await)["body"], "text");

    let resp = get(&f.api, &format!("/api/messages/{HUGE}/render")).await;
    assert_eq!(resp.status, 413);
    assert_eq!(get(&f.api, "/api/messages/5").await.status, 404);
    assert_eq!(get(&f.api, "/api/messages/abc/render").await.status, 404);
    assert_eq!(
        call(&f.api, Method::Post, "/api/messages/1/render", b"")
            .await
            .status,
        405
    );

    let offline = Api::new(Arc::new(Mail::new()), None);
    let resp = get(&offline, "/api/messages/1/render").await;
    assert_eq!(resp.status, 503);
    assert_eq!(json(&resp)["error"], "body_unavailable");
}

#[tokio::test]
async fn thread_detail_lists_messages_oldest_first() {
    let f = fixture();
    let body = json(&get(&f.api, "/api/threads/3").await);
    assert_eq!(body["thread"]["id"], 7, "merged id resolves to the target");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(
        messages[0]["from"],
        json!({ "name": "Ann", "email": "ann@example.com" })
    );
    assert_eq!(messages[0]["unread"], true);
    assert_eq!(
        messages[0]["hasRemoteImages"],
        Value::Null,
        "not cached yet"
    );

    get(&f.api, "/api/messages/1").await;
    get(&f.api, "/api/messages/2").await;
    let body = json(&get(&f.api, "/api/threads/7").await);
    assert_eq!(body["messages"][0]["hasRemoteImages"], true);
    assert_eq!(body["messages"][1]["hasRemoteImages"], false);
    assert_eq!(get(&f.api, "/api/threads/8").await.status, 404);
    assert_eq!(get(&f.api, "/api/threads/x").await.status, 404);
}
