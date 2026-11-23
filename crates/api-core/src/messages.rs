//! Reading messages (plan §3.9): message and thread details, the
//! sanitized render document, signed part downloads, the image proxy
//! route and the per-sender "always show images" list.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tagmail_core::{MessageRowId, ThreadId};

use crate::api::{Api, Response, bad_request, query_param};
use crate::dto::{
    ImageSenderDto, ImageSendersDto, MessageDto, PartDto, ThreadDetailDto, ThreadMessageDto,
};
use crate::mime::{self, PartInfo};
use crate::signing::{self, Capability, UrlKeys};
use crate::store::{FetchRawError, ImageProxy, MessageRecord, ProxyError, Store, StoreError};

/// CSP of the render document: no script, no fetches, images only from the
/// API (signed parts and proxy) or inline `data:`.
pub const RENDER_CSP: &str = "default-src 'none'; img-src 'self' data:; \
                              style-src 'unsafe-inline'; script-src 'none'; form-action 'none'; \
                              base-uri 'none'; frame-ancestors 'self'";
/// CSP of part and proxied image responses, which are never documents.
pub const PART_CSP: &str = "default-src 'none'; sandbox; frame-ancestors 'self'";
/// How long the API waits for the engine to fetch a raw message on open.
pub const RAW_FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the API rereads the URL keys (the store rotates them).
pub const KEYS_REFRESH: Duration = Duration::from_secs(60 * 60);
/// Signed URLs are valid for 10 minutes, so responses may be cached as long.
const SIGNED_CACHE_CONTROL: &str = "private, max-age=600";
const MAX_ADDRESS_LEN: usize = 320;
const HTML: &str = "text/html; charset=utf-8";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn not_found(what: &str) -> Response {
    Response::error(404, "not_found", format!("no such {what}"))
}

fn evicted() -> Response {
    let error = FetchRawError::Unavailable("evicted right after the fetch".to_owned());
    tracing::warn!(error = %error, "fetching raw message on open");
    Response::error(503, "body_unavailable", error.to_string())
}

fn forbidden() -> Response {
    Response::error(403, "forbidden", "invalid or expired link")
}

fn parse_id(s: &str) -> Option<i64> {
    s.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| s.parse().ok())
        .flatten()
}

fn signed_response(
    status: u16,
    content_type: &str,
    disposition: String,
    body: Vec<u8>,
) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type", content_type.to_owned()),
            ("content-disposition", disposition),
            ("cache-control", SIGNED_CACHE_CONTROL.to_owned()),
            ("x-content-type-options", "nosniff".to_owned()),
            ("content-security-policy", PART_CSP.to_owned()),
            ("referrer-policy", "no-referrer".to_owned()),
        ],
        body,
    }
}

/// `attachment` with an RFC 6266 / RFC 8187 file name.
fn attachment_disposition(name: Option<&str>) -> String {
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return "attachment".to_owned();
    };
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename*=UTF-8''{encoded}")
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Box::new(e) as StoreError)
}

/// What the message endpoints derive from a raw message.
struct Rendered {
    body: mime::Body,
    parts: Vec<PartInfo>,
    reply_to: Vec<crate::dto::AddressDto>,
    message_id: Option<String>,
}

fn render(raw: &[u8], id: MessageRowId, keys: UrlKeys, at: u64, images: bool) -> Option<Rendered> {
    let message = mime::parse(raw)?;
    let part_keys = keys.clone();
    let part_url = move |part| signing::part_url(&part_keys, id, part, at);
    let remote: Option<mime::sanitize::RemoteImages> =
        images.then(|| Box::new(move |url: &str| signing::proxy_url(&keys, url, at)) as _);
    let body = mime::body(&message, &part_url, remote);
    let parts = mime::parts(&message, &body.cids);
    Some(Rendered {
        reply_to: mime::addresses(message.reply_to()),
        message_id: message.message_id().map(str::to_owned),
        body,
        parts,
    })
}

impl<S: Store> Api<S> {
    /// Serves `/api/proxy/img/{token}` through `proxy`; without one that
    /// route answers `503`.
    pub fn with_image_proxy(mut self, proxy: Arc<dyn ImageProxy>) -> Api<S> {
        self.proxy = Some(proxy);
        self
    }

    async fn url_keys(&self) -> Result<UrlKeys, StoreError> {
        let mut cached = self.keys.lock().await;
        if let Some((at, keys)) = cached.as_ref()
            && at.elapsed() < KEYS_REFRESH
        {
            return Ok(keys.clone());
        }
        let keys = self.store.url_keys().await?;
        *cached = Some((Instant::now(), keys.clone()));
        Ok(keys)
    }

    pub(crate) async fn record(
        &self,
        id: &str,
    ) -> Result<Result<MessageRecord, Response>, StoreError> {
        let Some(id) = parse_id(id) else {
            return Ok(Err(not_found("message")));
        };
        Ok(self
            .store
            .message(MessageRowId(id))
            .await?
            .ok_or_else(|| not_found("message")))
    }

    /// The raw message, fetched from the server first if it is not cached.
    pub(crate) async fn raw(
        &self,
        record: &MessageRecord,
    ) -> Result<Result<Vec<u8>, Response>, StoreError> {
        if let Some(raw) = self.store.raw_message(record.id).await? {
            return Ok(Ok(raw));
        }
        if let Err(resp) = self.fetch(record).await? {
            return Ok(Err(resp));
        }
        Ok(self.store.raw_message(record.id).await?.ok_or_else(evicted))
    }

    /// [`Store::render_source`], fetched from the server first if the raw
    /// message is not cached.
    async fn render_source(
        &self,
        record: &MessageRecord,
    ) -> Result<Result<Vec<u8>, Response>, StoreError> {
        if let Some(source) = self.store.render_source(record.id).await? {
            return Ok(Ok(source));
        }
        if let Err(resp) = self.fetch(record).await? {
            return Ok(Err(resp));
        }
        Ok(self
            .store
            .render_source(record.id)
            .await?
            .ok_or_else(evicted))
    }

    /// Fetches the raw message into the store's cache.
    async fn fetch(&self, record: &MessageRecord) -> Result<Result<(), Response>, StoreError> {
        let Some(registry) = &self.registry else {
            return Ok(Err(Response::error(
                503,
                "body_unavailable",
                "the message body is not cached and no sync engine is running",
            )));
        };
        let fetched = tokio::time::timeout(
            RAW_FETCH_TIMEOUT,
            registry.fetch_raw(record.account_id, record.id),
        )
        .await
        .unwrap_or_else(|_| Err(FetchRawError::Unavailable("timed out".to_owned())));
        let Err(error) = fetched else {
            return Ok(Ok(()));
        };
        tracing::warn!(message = record.id.0, error = %error, "fetching raw message on open");
        let (status, code) = match error {
            FetchRawError::NotFound => (404, "gone"),
            FetchRawError::TooLarge => (413, "too_large"),
            FetchRawError::Unavailable(_) => (503, "body_unavailable"),
        };
        Ok(Err(Response::error(status, code, error.to_string())))
    }

    async fn images_allowed(&self, record: &MessageRecord) -> Result<bool, StoreError> {
        let allowed = self.store.image_senders().await?;
        Ok(record
            .from
            .iter()
            .any(|a| allowed.contains(&a.email.to_ascii_lowercase())))
    }

    async fn rendered(
        &self,
        record: &MessageRecord,
        images: bool,
    ) -> Result<Result<Rendered, Response>, StoreError> {
        let source = match self.render_source(record).await? {
            Ok(source) => source,
            Err(resp) => return Ok(Err(resp)),
        };
        let keys = self.url_keys().await?;
        let id = record.id;
        let at = now();
        Ok(blocking(move || render(&source, id, keys, at, images))
            .await?
            .ok_or_else(|| Response::error(422, "unparsable", "the message cannot be parsed")))
    }

    pub(crate) async fn message(&self, id: &str) -> Result<Response, StoreError> {
        let record = match self.record(id).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let images_allowed = self.images_allowed(&record).await?;
        let rendered = match self.rendered(&record, false).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let keys = self.url_keys().await?;
        let at = now();
        let parts = rendered
            .parts
            .into_iter()
            .map(|p| PartDto {
                url: signing::part_url(&keys, record.id, p.id, at),
                id: p.id,
                content_type: p.content_type,
                name: p.name,
                size: p.size,
                content_id: p.content_id,
                inline: p.inline,
            })
            .collect();
        Ok(Response::json(
            200,
            &MessageDto {
                id: record.id.0,
                thread_id: record.thread_id.0,
                account_id: record.account_id.0,
                subject: record.subject,
                from: record.from,
                to: record.to,
                cc: record.cc,
                reply_to: rendered.reply_to,
                date: record.date,
                message_id: rendered.message_id,
                unread: record.unread,
                starred: record.starred,
                body: rendered.body.kind,
                remote_images: rendered.body.stats.remote_images,
                images_allowed,
                parts,
            },
        ))
    }

    pub(crate) async fn render_message(
        &self,
        id: &str,
        query: &str,
    ) -> Result<Response, StoreError> {
        let record = match self.record(id).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        let images = match query_param(query, "images").as_deref() {
            None | Some("0") => self.images_allowed(&record).await?,
            Some("1") => true,
            Some(_) => return Ok(bad_request("images must be 0 or 1")),
        };
        let rendered = match self.rendered(&record, images).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        Ok(Response {
            status: 200,
            headers: vec![
                ("content-type", HTML.to_owned()),
                ("cache-control", "no-store".to_owned()),
                ("x-content-type-options", "nosniff".to_owned()),
                ("content-security-policy", RENDER_CSP.to_owned()),
                ("referrer-policy", "no-referrer".to_owned()),
            ],
            body: mime::document(&rendered.body.html).into_bytes(),
        })
    }

    pub(crate) async fn part(&self, token: &str, query: &str) -> Result<Response, StoreError> {
        let keys = self.url_keys().await?;
        let Ok(Capability::Part { message, part }) = signing::verify(&keys, token, now()) else {
            return Ok(forbidden());
        };
        let Some(record) = self.store.message(message).await? else {
            return Ok(not_found("message"));
        };
        let raw = match self.raw(&record).await? {
            Ok(raw) => raw,
            Err(resp) => return Ok(resp),
        };
        let content = blocking(move || {
            let message = mime::parse(&raw)?;
            mime::part_content(&message, part)
        })
        .await?;
        let Some((bytes, info)) = content else {
            return Ok(not_found("part"));
        };
        let download = query_param(query, "download").is_some_and(|v| v == "1");
        Ok(match mime::sniff_image(&bytes).filter(|_| !download) {
            Some(image) => signed_response(200, image, "inline".to_owned(), bytes),
            None => signed_response(
                200,
                crate::api::OCTET_STREAM,
                attachment_disposition(info.name.as_deref()),
                bytes,
            ),
        })
    }

    pub(crate) async fn proxy_image(&self, token: &str) -> Result<Response, StoreError> {
        let keys = self.url_keys().await?;
        let Ok(Capability::Image { url }) = signing::verify(&keys, token, now()) else {
            return Ok(forbidden());
        };
        let Some(proxy) = &self.proxy else {
            return Ok(Response::error(
                503,
                "proxy_unavailable",
                "no image proxy is configured",
            ));
        };
        Ok(match proxy.fetch(url).await {
            Ok(image) => signed_response(200, image.content_type, "inline".to_owned(), image.bytes),
            Err(e) => {
                tracing::debug!(error = ?e, "image proxy");
                match e {
                    ProxyError::Blocked(why) => Response::error(403, "blocked", why),
                    ProxyError::Upstream(why) => Response::error(502, "upstream", why),
                    ProxyError::TooLarge => {
                        Response::error(502, "too_large", "the image is too large")
                    }
                    ProxyError::Timeout => {
                        Response::error(504, "timeout", "the image server did not answer")
                    }
                }
            }
        })
    }

    pub(crate) async fn thread(&self, id: &str) -> Result<Response, StoreError> {
        let Some(id) = parse_id(id) else {
            return Ok(not_found("thread"));
        };
        let Some(detail) = self.store.thread_detail(ThreadId(id)).await? else {
            return Ok(not_found("thread"));
        };
        let keys = self.url_keys().await?;
        let mut messages = Vec::with_capacity(detail.messages.len());
        for m in detail.messages {
            let has_remote_images = match self.store.raw_message(m.id).await? {
                None => None,
                Some(raw) => {
                    let (keys, id, at) = (keys.clone(), m.id, now());
                    blocking(move || render(&raw, id, keys, at, false))
                        .await?
                        .map(|r| r.body.stats.remote_images > 0)
                }
            };
            messages.push(ThreadMessageDto {
                id: m.id.0,
                from: m.from.into_iter().next(),
                to: m.to,
                cc: m.cc,
                date: m.date,
                subject: m.subject,
                snippet: m.snippet,
                unread: m.unread,
                starred: m.starred,
                has_attachment: m.has_attachment,
                has_remote_images,
            });
        }
        Ok(Response::json(
            200,
            &ThreadDetailDto {
                thread: detail.row,
                messages,
            },
        ))
    }

    pub(crate) async fn image_senders(&self) -> Result<Response, StoreError> {
        let addresses = self.store.image_senders().await?;
        Ok(Response::json(200, &ImageSendersDto { addresses }))
    }

    pub(crate) async fn set_image_sender(&self, body: &[u8]) -> Result<Response, StoreError> {
        let dto: ImageSenderDto = match serde_json::from_slice(body) {
            Ok(dto) => dto,
            Err(e) => return Ok(bad_request(format!("invalid JSON: {e}"))),
        };
        let address = dto.address.trim().to_ascii_lowercase();
        let valid = address.len() <= MAX_ADDRESS_LEN
            && address
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
            && !address.chars().any(|c| c.is_whitespace() || c.is_control());
        if !valid {
            return Ok(Response::json(
                422,
                &crate::dto::ApiErrorDto {
                    error: "invalid".to_owned(),
                    message: "not an email address".to_owned(),
                    field: Some("address".to_owned()),
                },
            ));
        }
        self.store.set_image_sender(address, dto.allow).await?;
        self.image_senders().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_are_encoded() {
        assert_eq!(attachment_disposition(None), "attachment");
        assert_eq!(attachment_disposition(Some(" ")), "attachment");
        assert_eq!(
            attachment_disposition(Some("Bericht März \"1\".pdf")),
            "attachment; filename*=UTF-8''Bericht%20M%C3%A4rz%20%221%22.pdf"
        );
    }

    #[test]
    fn ids_are_plain_digits() {
        assert_eq!(parse_id("42"), Some(42));
        for bad in ["", "-1", "+1", "1a", "99999999999999999999"] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }
}
