//! Message reading over [`Api`]: render document and its CSP, signed part
//! and proxy URLs, fetch on open, the "always show images" list and the
//! thread detail.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tagmail_api_core::dto::{AccountDto, AddressDto, LabelDto, MailboxDto, ThreadRowDto};
use tagmail_api_core::messages::{PART_CSP, RENDER_CSP};
use tagmail_api_core::mime::BLOCKED_IMAGE;
use tagmail_api_core::mime::reduce::{CalendarParts, calendar_parts};
use tagmail_api_core::signing::{self, Capability, TOKEN_TTL_SECS, UrlKeys};
use tagmail_api_core::{
    AccountPatch, AccountRegistry, AccountSummary, Api, BoxFuture, ChangePage, FetchRawError,
    ImageProxy, MarkDeleting, MessageRecord, Method, NewAccount, ProxiedImage, ProxyError, Request,
    Response, RuntimeStatus, Store, StoreResult, ThreadDetail, ThreadRows, View, ViewIndex,
};
use tagmail_core::{AccountId, CacheEpoch, ChangeCursor, ChangeFilter, MessageRowId, ThreadId};
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
        has_calendar: false,
    }
}

struct Mail {
    records: Vec<MessageRecord>,
    /// Messages outside the thread, by id.
    others: HashMap<i64, MessageRecord>,
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
            others: HashMap::new(),
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

    /// Adds message `id` with `raw` on the server and `has_calendar` set.
    fn with_invite(mut self, id: i64, from: &str, raw: Vec<u8>) -> Mail {
        let mut record = record(id, from);
        record.has_calendar = true;
        self.others.insert(id, record);
        self.server.insert(id, raw);
        self
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

    async fn changes_since(
        &self,
        _: ChangeCursor,
        _: usize,
        _: ChangeFilter,
    ) -> StoreResult<ChangePage> {
        Ok(ChangePage::Gone)
    }

    async fn accounts(&self) -> StoreResult<Vec<AccountSummary>> {
        Ok(vec![])
    }

    async fn create_account(&self, _: NewAccount) -> StoreResult<AccountId> {
        Err("read-only".into())
    }

    async fn accounts_full(&self) -> StoreResult<Vec<AccountDto>> {
        Ok(vec![])
    }

    async fn account_settings(&self, _: AccountId) -> StoreResult<Option<NewAccount>> {
        Ok(None)
    }

    async fn update_account(
        &self,
        _: AccountId,
        _: AccountPatch,
    ) -> StoreResult<Option<AccountDto>> {
        Err("read-only".into())
    }

    async fn mark_deleting(&self, _: AccountId) -> StoreResult<MarkDeleting> {
        Err("read-only".into())
    }

    async fn message(&self, id: MessageRowId) -> StoreResult<Option<MessageRecord>> {
        if id.0 == HUGE {
            return Ok(Some(record(HUGE, "x@example.com")));
        }
        Ok(self
            .records
            .iter()
            .find(|r| r.id == id)
            .or_else(|| self.others.get(&id.0))
            .cloned())
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

    async fn calendar_source(&self, id: MessageRowId) -> StoreResult<Option<CalendarParts>> {
        let cached = self.cached.lock().unwrap().get(&id.0).cloned();
        Ok(cached
            .map(|raw| calendar_parts(raw.as_slice()))
            .transpose()?)
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
    fixture_with(Mail::new())
}

fn fixture_with(mail: Mail) -> Fixture {
    let mail = Arc::new(mail);
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
    assert_eq!(body["hasInvitation"], false);
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

fn calendar_fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../calendar/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn base64_body(data: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|l| std::str::from_utf8(l).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// A mail with `from_fields` as its sender header fields, `inline` as the
/// inline calendar and `attached` as an `.ics` attachment.
fn calendar_mail(from_fields: &str, inline: &[u8], attached: &[u8]) -> Vec<u8> {
    format!(
        concat!(
            "{from_fields}Subject: Invitation\r\n",
            "Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n",
            "--m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n",
            "--a\r\nContent-Type: text/plain\r\n\r\nYou are invited\r\n",
            "--a\r\nContent-Type: text/calendar; charset=UTF-8; method=REQUEST\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n{inline}\r\n",
            "--a--\r\n",
            "--m\r\nContent-Type: application/ics; name=\"invite.ics\"\r\n",
            "Content-Disposition: attachment; filename=\"invite.ics\"\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n{attached}\r\n",
            "--m--\r\n",
        ),
        from_fields = from_fields,
        inline = base64_body(inline),
        attached = base64_body(attached),
    )
    .into_bytes()
}

/// An invitation mail as Google and Outlook send it: a text body, the
/// calendar inline and the same calendar again as an `.ics` attachment.
fn invite_mail(from: &str, ics: &[u8]) -> Vec<u8> {
    let domain = from.split('@').nth(1).unwrap_or_default();
    calendar_mail(
        &format!(
            "Authentication-Results: mx.example.net; dmarc=pass header.from={domain}\r\n\
             From: <{from}>\r\n"
        ),
        ics,
        ics,
    )
}

#[cfg(not(feature = "calendar"))]
#[tokio::test]
async fn invitations_need_the_calendar_feature() {
    let f = fixture_with(Mail::new().with_invite(
        10,
        "alice@example.com",
        invite_mail("alice@example.com", &calendar_fixture("google_request.ics")),
    ));
    let resp = get(&f.api, "/api/messages/10/invitation").await;
    assert_eq!(resp.status, 404);
    assert_eq!(json(&resp)["error"], "feature_disabled");
    assert_eq!(f.engine.fetches.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "calendar")]
mod invitation {
    use super::*;

    /// Fixture, sender address.
    const GOLDEN: &[(&str, &str)] = &[
        ("google_request", "alice@example.com"),
        ("google_cancel", "alice@example.com"),
        ("spoofed_organizer", "payroll@bank-example.test"),
        ("apple_all_day", "grace@icloud.example"),
        ("apple_recurring", "hank@example.com"),
        ("outlook_teams", "dana.sample@contoso.example"),
        ("outlook_custom_tz", "frank.demo@fabrikam.example"),
        ("publish_multi", "calendar@example.org"),
        ("publish_partially_bad", "calendar@example.org"),
        ("spoofed_organizer_email_param", "x@evil.example"),
        ("spoofed_schemeless_email_param", "x@evil.example"),
    ];
    /// Set to rewrite the golden files from the current output.
    const BLESS: &str = "TAGMAIL_BLESS";

    fn golden_path(name: &str) -> String {
        format!(
            "{}/tests/golden/invitation/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[tokio::test]
    async fn invitation_cards_match_their_golden_files() {
        let mut mail = Mail::new();
        for (i, (name, from)) in (10..).zip(GOLDEN) {
            let ics = calendar_fixture(&format!("{name}.ics"));
            mail = mail.with_invite(i, from, invite_mail(from, &ics));
        }
        let f = fixture_with(mail);
        for (i, (name, _)) in (10..).zip(GOLDEN) {
            let resp = get(&f.api, &format!("/api/messages/{i}/invitation")).await;
            assert_eq!(resp.status, 200, "{name}: {}", text(&resp));
            let got = json(&resp);
            let path = golden_path(name);
            if std::env::var_os(BLESS).is_some() {
                let pretty = serde_json::to_string_pretty(&got).unwrap() + "\n";
                std::fs::write(&path, pretty).unwrap();
                continue;
            }
            let expected: Value = serde_json::from_str(
                &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")),
            )
            .unwrap();
            assert_eq!(got, expected, "{name}");
        }
    }

    #[tokio::test]
    async fn the_sender_check_reports_mismatch_without_auto_apply() {
        let ics = calendar_fixture("google_request.ics");
        let f = fixture_with(
            Mail::new()
                .with_invite(
                    10,
                    "alice@example.com",
                    invite_mail("alice@example.com", &ics),
                )
                .with_invite(
                    11,
                    "mallory@example.com",
                    invite_mail("mallory@example.com", &ics),
                ),
        );
        let matching = json(&get(&f.api, "/api/messages/10/invitation").await);
        assert_eq!(matching["senderAuth"], "untrusted_config");
        assert_eq!(matching["autoApply"], "none");
        assert_eq!(
            matching["events"].as_array().unwrap().len(),
            1,
            "deduplicated"
        );
        let forged = json(&get(&f.api, "/api/messages/11/invitation").await);
        assert_eq!(forged["senderAuth"], "address_mismatch");
        assert_eq!(forged["autoApply"], "none");
    }

    #[tokio::test]
    async fn the_raw_is_fetched_on_open_once() {
        let ics = calendar_fixture("google_request.ics");
        let f = fixture_with(Mail::new().with_invite(
            10,
            "alice@example.com",
            invite_mail("alice@example.com", &ics),
        ));
        assert_eq!(get(&f.api, "/api/messages/10/invitation").await.status, 200);
        assert_eq!(get(&f.api, "/api/messages/10/invitation").await.status, 200);
        assert_eq!(f.engine.fetches.load(Ordering::SeqCst), 1);
        let message = json(&get(&f.api, "/api/messages/10").await);
        assert_eq!(message["hasInvitation"], true);
    }

    #[tokio::test]
    async fn messages_without_a_usable_invitation_are_refused() {
        let f = fixture_with(
            Mail::new()
                .with_invite(
                    10,
                    "a@example.com",
                    invite_mail("a@example.com", &calendar_fixture("malformed_not_ical.ics")),
                )
                .with_invite(
                    11,
                    "a@example.com",
                    b"Subject: no parts\r\n\r\nhi\r\n".to_vec(),
                )
                .with_invite(
                    12,
                    "a@example.com",
                    invite_mail(
                        "a@example.com",
                        &calendar_fixture("malformed_truncated.ics"),
                    ),
                ),
        );
        for id in [10, 12] {
            let resp = get(&f.api, &format!("/api/messages/{id}/invitation")).await;
            assert_eq!(resp.status, 422, "{id}");
            assert_eq!(json(&resp)["error"], "unparsable");
        }
        let resp = get(&f.api, "/api/messages/11/invitation").await;
        assert_eq!(resp.status, 404);
        assert_eq!(json(&resp)["error"], "no_invitation");
        let resp = get(&f.api, "/api/messages/1/invitation").await;
        assert_eq!(resp.status, 404);
        assert_eq!(json(&resp)["error"], "no_invitation");
        assert_eq!(
            f.engine.fetches.load(Ordering::SeqCst),
            3,
            "no fetch without has_calendar"
        );
        assert_eq!(get(&f.api, "/api/messages/77/invitation").await.status, 404);
    }

    #[tokio::test]
    async fn an_oversized_invitation_is_reported_not_parsed() {
        let big = format!(
            "From: a@example.com\r\nContent-Type: text/calendar\r\n\r\n{}\r\n",
            "x".repeat(tagmail_api_core::mime::reduce::CALENDAR_PART_CAP + 1)
        );
        let f = fixture_with(Mail::new().with_invite(10, "a@example.com", big.into_bytes()));
        let resp = get(&f.api, "/api/messages/10/invitation").await;
        assert_eq!(resp.status, 200, "{}", text(&resp));
        let body = json(&resp);
        assert_eq!(body["tooLarge"], true);
        assert_eq!(body["events"], json!([]));
    }

    #[tokio::test]
    async fn only_a_single_from_mailbox_can_match() {
        let ics = calendar_fixture("google_request.ics");
        let f = fixture_with(
            Mail::new()
                .with_invite(
                    10,
                    "alice@example.com",
                    calendar_mail(
                        "From: alice@example.com\r\nFrom: mallory@example.com\r\n",
                        &ics,
                        &ics,
                    ),
                )
                .with_invite(
                    11,
                    "alice@example.com",
                    calendar_mail(
                        "From: alice@example.com, mallory@example.com\r\n",
                        &ics,
                        &ics,
                    ),
                )
                .with_invite(
                    12,
                    "alice@example.com",
                    calendar_mail("Sender: alice@example.com\r\n", &ics, &ics),
                ),
        );
        for id in [10, 11, 12] {
            let card = json(&get(&f.api, &format!("/api/messages/{id}/invitation")).await);
            assert_eq!(card["senderAuth"], "address_mismatch", "{id}");
        }
    }

    #[tokio::test]
    async fn the_newest_copy_of_an_event_wins_and_methods_stay_per_part() {
        let request = calendar_fixture("google_request.ics");
        let updated = calendar_fixture("google_request_updated.ics");
        let cancel = calendar_fixture("google_cancel.ics");
        let from = "From: alice@example.com\r\n";
        let f = fixture_with(
            Mail::new()
                .with_invite(
                    10,
                    "alice@example.com",
                    calendar_mail(from, &request, &updated),
                )
                .with_invite(
                    11,
                    "alice@example.com",
                    calendar_mail(from, &updated, &request),
                )
                .with_invite(
                    12,
                    "alice@example.com",
                    calendar_mail(from, &cancel, &request),
                ),
        );
        let first = json(&get(&f.api, "/api/messages/10/invitation").await);
        let second = json(&get(&f.api, "/api/messages/11/invitation").await);
        assert_eq!(first["method"], "request");
        assert_eq!(first["events"].as_array().unwrap().len(), 1);
        assert_eq!(first, second, "SEQUENCE 1 wins in either order");
        let mixed = json(&get(&f.api, "/api/messages/12/invitation").await);
        assert_eq!(mixed["method"], "other");
        assert_eq!(
            mixed["events"][0]["cancelled"], true,
            "CANCEL has SEQUENCE 2"
        );
    }
}
