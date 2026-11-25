use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Response, StatusCode};
use tokio_util::io::StreamReader;
use url::Url;
use zeroize::Zeroizing;

use crate::env::DiscoveryEnv;
use crate::error::{DavError, Result, UidConflictKind};
use crate::host::{host_allowed, url_host};
use crate::href::{normalize_href, same_resource, strict_path};
use crate::model::{
    CalendarInfo, ClientOptions, MultigetItem, PutCondition, Resource, ResourceRef, SyncPage,
    UidMatch,
};
use crate::xml::{self, MsResponse, Ns, Props, is_success, xml_text};

/// Hrefs per `calendar-multiget` request.
pub const MULTIGET_BATCH: usize = 50;
pub(crate) const MAX_REDIRECTS: usize = 5;
const MAX_MULTISTATUS_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_SMALL_BODY_BYTES: usize = 64 * 1024;
const MAX_RESOURCE_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const USER_AGENT: &str = concat!("tagmail/", env!("CARGO_PKG_VERSION"));

const SYNC_COLLECTION: &str = "sync-collection";
const CONTENT_XML: &str = "application/xml; charset=utf-8";
const CONTENT_ICS: &str = "text/calendar; charset=utf-8";
const COMPONENT_EVENT: &str = "VEVENT";
const XMLNS: &str = r#"xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/""#;

fn propfind_method() -> Method {
    Method::from_bytes(b"PROPFIND").expect("valid method")
}

fn report_method() -> Method {
    Method::from_bytes(b"REPORT").expect("valid method")
}

fn move_method() -> Method {
    Method::from_bytes(b"MOVE").expect("valid method")
}

#[derive(Clone, Copy)]
pub(crate) enum Depth {
    Zero,
    One,
}

impl Depth {
    fn value(self) -> &'static str {
        match self {
            Depth::Zero => "0",
            Depth::One => "1",
        }
    }
}

pub(crate) struct Req {
    method: Method,
    url: Url,
    depth: Option<Depth>,
    body: Option<(String, &'static str)>,
    headers: HeaderMap,
}

impl Req {
    pub(crate) fn new(method: Method, url: &Url) -> Self {
        Req {
            method,
            url: url.clone(),
            depth: None,
            body: None,
            headers: HeaderMap::new(),
        }
    }

    pub(crate) fn propfind(url: &Url, depth: Depth, props: &str) -> Self {
        Req::new(propfind_method(), url)
            .depth(depth)
            .xml(format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind {XMLNS}><d:prop>{props}</d:prop></d:propfind>"#))
    }

    fn depth(mut self, depth: Depth) -> Self {
        self.depth = Some(depth);
        self
    }

    fn xml(mut self, body: String) -> Self {
        self.body = Some((body, CONTENT_XML));
        self
    }

    fn header(mut self, name: HeaderName, value: &str) -> Result<Self> {
        let value = HeaderValue::from_str(value)
            .map_err(|_| DavError::Config(format!("invalid {name} header value")))?;
        self.headers.insert(name, value);
        Ok(self)
    }
}

/// A CalDAV client bound to one account. Every request, and every redirect
/// hop, must stay on the confirmed host or its registrable domain.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    username: String,
    password: Zeroizing<String>,
    allow_insecure: bool,
    anchor: Option<String>,
    env: DiscoveryEnv,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("username", &self.username)
            .field("anchor", &self.anchor)
            .field("allow_insecure", &self.allow_insecure)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(options: &ClientOptions, confirmed_host: &str, env: &DiscoveryEnv) -> Result<Self> {
        Self::build(options, Some(confirmed_host), env)
    }

    pub(crate) fn build(
        options: &ClientOptions,
        anchor: Option<&str>,
        env: &DiscoveryEnv,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(options.tls.clone())
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(USER_AGENT);
        for (host, addr) in env.resolve_overrides() {
            builder = builder.resolve(host, *addr);
        }
        let http = builder.build().map_err(DavError::Network)?;
        Ok(Client {
            http,
            username: options.username.clone(),
            password: options.password.clone(),
            allow_insecure: options.allow_insecure,
            anchor: anchor.map(str::to_ascii_lowercase),
            env: env.clone(),
        })
    }

    fn check_url(&self, url: &Url, authenticated: bool) -> Result<()> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_insecure => {}
            _ => return Err(DavError::InsecureUrl { url: url.clone() }),
        }
        let host = url_host(url).ok_or_else(|| DavError::InsecureUrl { url: url.clone() })?;
        if authenticated {
            let anchor = self
                .anchor
                .as_deref()
                .ok_or_else(|| DavError::Config("no confirmed host".to_owned()))?;
            if !host_allowed(anchor, &host) {
                return Err(DavError::UnconfirmedHost { host });
            }
        }
        Ok(())
    }

    /// Sends `req`, following up to [`MAX_REDIRECTS`] redirects; returns the
    /// response and the URL that produced it.
    pub(crate) async fn send(&self, req: Req, authenticated: bool) -> Result<(Response, Url)> {
        let mut url = req.url.clone();
        for _ in 0..=MAX_REDIRECTS {
            self.check_url(&url, authenticated)?;
            let mut builder = self
                .http
                .request(req.method.clone(), url.clone())
                .headers(req.headers.clone());
            if let Some(depth) = req.depth {
                builder = builder.header("Depth", depth.value());
            }
            if let Some((body, content_type)) = &req.body {
                builder = builder
                    .header(header::CONTENT_TYPE, *content_type)
                    .body(body.clone());
            }
            if authenticated {
                builder = builder.basic_auth(&self.username, Some(self.password.as_str()));
            }
            let response = builder.send().await.map_err(DavError::Network)?;
            if !response.status().is_redirection() || response.status() == StatusCode::NOT_MODIFIED
            {
                return Ok((response, url));
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .ok_or(DavError::BadRedirect)?;
            let next = url.join(location).map_err(|_| DavError::BadRedirect)?;
            url = next;
        }
        Err(DavError::TooManyRedirects { max: MAX_REDIRECTS })
    }

    async fn send_ok(&self, req: Req) -> Result<(Response, Url)> {
        let (response, url) = self.send(req, true).await?;
        let response = check_status(response, &url).await?;
        Ok((response, url))
    }

    /// Streams a multistatus body into `on_response`, one `response` at a
    /// time; returns the top-level `sync-token`.
    async fn parse<F>(
        &self,
        response: Response,
        base: &Url,
        mut on_response: F,
    ) -> Result<Option<String>>
    where
        F: FnMut(&Url, MsResponse) -> Result<()>,
    {
        let total = AtomicUsize::new(0);
        let stream = response.bytes_stream().map(|chunk| {
            let chunk = chunk.map_err(io::Error::other)?;
            let seen = total.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
            if seen > MAX_MULTISTATUS_BYTES {
                return Err(io::Error::other("multistatus too large"));
            }
            Ok(chunk)
        });
        let result = xml::parse_multistatus(StreamReader::new(stream), |mut r| {
            self.apply_env(&mut r.props);
            on_response(base, r)
        })
        .await;
        if total.load(Ordering::Relaxed) > MAX_MULTISTATUS_BYTES {
            return Err(DavError::ResponseTooLarge {
                limit: MAX_MULTISTATUS_BYTES,
            });
        }
        result
    }

    fn apply_env(&self, props: &mut Props) {
        let env = &self.env;
        let slots: [(&str, &mut Option<String>); 5] = [
            ("getctag", &mut props.ctag),
            ("sync-token", &mut props.sync_token),
            ("displayname", &mut props.display_name),
            ("calendar-color", &mut props.color),
            ("getetag", &mut props.etag),
        ];
        for (name, slot) in slots {
            if env.property_disabled(name) {
                *slot = None;
            }
        }
        props.reports.retain(|r| !env.report_disabled(&r.local));
    }

    pub(crate) async fn propfind_all(&self, req: Req) -> Result<(Url, Vec<(Url, MsResponse)>)> {
        let (response, base) = self.send_ok(req).await?;
        let mut out = Vec::new();
        self.parse(response, &base, |base, r| {
            if let Some(href) = r.hrefs.first() {
                out.push((normalize_href(base, href)?, r));
            }
            Ok(())
        })
        .await?;
        Ok((base, out))
    }

    /// The `DAV` header tokens of an OPTIONS request.
    pub async fn capabilities(&self, url: &Url) -> Result<Vec<String>> {
        let (response, _) = self.send_ok(Req::new(Method::OPTIONS, url)).await?;
        Ok(response
            .headers()
            .get_all("DAV")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect())
    }

    /// The calendars in `home` that can hold events.
    pub async fn list_calendars(&self, home: &Url) -> Result<Vec<CalendarInfo>> {
        let (_, responses) = self
            .propfind_all(Req::propfind(home, Depth::One, CALENDAR_PROPS))
            .await?;
        Ok(responses
            .into_iter()
            .filter(|(href, r)| {
                !same_resource(href, home)
                    && r.status.is_none_or(is_success)
                    && r.props.has_type(Ns::CalDav, "calendar")
            })
            .map(|(href, r)| calendar_info(href, r.props))
            .filter(CalendarInfo::supports_events)
            .collect())
    }

    /// One calendar's own properties, including its CTag and sync token
    /// when the server has them.
    pub async fn calendar_state(&self, calendar: &Url) -> Result<CalendarInfo> {
        let (_, responses) = self
            .propfind_all(Req::propfind(calendar, Depth::Zero, CALENDAR_PROPS))
            .await?;
        responses
            .into_iter()
            .find(|(href, _)| same_resource(href, calendar))
            .map(|(href, r)| calendar_info(href, r.props))
            .ok_or_else(|| DavError::NotFound {
                url: calendar.clone(),
            })
    }

    /// RFC 6578 `sync-collection`. `token: None` is the initial sync.
    pub async fn sync_collection(&self, calendar: &Url, token: Option<&str>) -> Result<SyncPage> {
        if self.env.report_disabled(SYNC_COLLECTION) {
            return Err(DavError::ReportUnsupported(SYNC_COLLECTION));
        }
        let token = token.map(xml_text).unwrap_or_default();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:sync-collection {XMLNS}><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"#
        );
        let (response, base) = self
            .send(Req::new(report_method(), calendar).xml(body), true)
            .await?;
        let status = response.status();
        let mut page = SyncPage::default();
        let response = match status {
            StatusCode::INSUFFICIENT_STORAGE => {
                page.truncated = true;
                response
            }
            StatusCode::GONE => return Err(DavError::InvalidSyncToken),
            StatusCode::NOT_IMPLEMENTED => {
                return Err(DavError::ReportUnsupported(SYNC_COLLECTION));
            }
            _ => check_status(response, &base).await?,
        };
        let sync_token = self
            .parse(response, &base, |base, r| {
                let Some(href) = r.hrefs.first() else {
                    return Ok(());
                };
                let href = normalize_href(base, href)?;
                if same_resource(&href, calendar) {
                    if r.status == Some(StatusCode::INSUFFICIENT_STORAGE.as_u16()) {
                        page.truncated = true;
                    }
                    return Ok(());
                }
                match r.status {
                    Some(s) if s == StatusCode::NOT_FOUND.as_u16() => page.removed.push(href),
                    Some(s) if !is_success(s) => {}
                    _ => page.changed.push(ResourceRef {
                        href,
                        etag: r.props.etag,
                    }),
                }
                Ok(())
            })
            .await?;
        page.sync_token = sync_token;
        Ok(page)
    }

    /// Every object in `calendar` with its ETag, from one Depth-1 PROPFIND.
    pub async fn list_etags(&self, calendar: &Url) -> Result<Vec<ResourceRef>> {
        let (response, base) = self
            .send_ok(Req::propfind(
                calendar,
                Depth::One,
                "<d:getetag/><d:resourcetype/>",
            ))
            .await?;
        let mut out = Vec::new();
        self.parse(response, &base, |base, r| {
            let Some(href) = r.hrefs.first() else {
                return Ok(());
            };
            let href = normalize_href(base, href)?;
            if same_resource(&href, calendar)
                || r.props.has_type(Ns::Dav, "collection")
                || r.status.is_some_and(|s| !is_success(s))
            {
                return Ok(());
            }
            out.push(ResourceRef {
                href,
                etag: r.props.etag,
            });
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// `calendar-multiget` in batches of [`MULTIGET_BATCH`].
    pub async fn multiget(&self, calendar: &Url, hrefs: &[Url]) -> Result<Vec<MultigetItem>> {
        let mut out = Vec::with_capacity(hrefs.len());
        for batch in hrefs.chunks(MULTIGET_BATCH) {
            let refs: String = batch
                .iter()
                .map(|h| format!("<d:href>{}</d:href>", xml_text(&request_href(calendar, h))))
                .collect();
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-multiget {XMLNS}><d:prop><d:getetag/><c:calendar-data/></d:prop>{refs}</c:calendar-multiget>"#
            );
            let req = Req::new(report_method(), calendar)
                .depth(Depth::One)
                .xml(body);
            let (response, base) = self.send_ok(req).await?;
            let mut answered: Vec<Url> = Vec::new();
            self.parse(response, &base, |base, r| {
                for href in &r.hrefs {
                    answered.push(normalize_href(base, href)?);
                }
                if let Some(item) = multiget_item(base, r)? {
                    out.push(item);
                }
                Ok(())
            })
            .await?;
            for href in batch {
                if !answered.iter().any(|a| same_resource(a, href)) {
                    out.push(MultigetItem::Missing(href.clone()));
                }
            }
        }
        Ok(out)
    }

    pub async fn get(&self, href: &Url) -> Result<Resource> {
        let (response, url) = self.send_ok(Req::new(Method::GET, href)).await?;
        let etag = etag_header(&response);
        let data = read_limited(response, MAX_RESOURCE_BYTES).await?;
        Ok(Resource {
            href: url,
            etag,
            data: String::from_utf8_lossy(&data).into_owned(),
        })
    }

    /// Writes an object; returns the new ETag when the server sends one.
    pub async fn put(
        &self,
        href: &Url,
        ics: &str,
        condition: PutCondition,
    ) -> Result<Option<String>> {
        let mut req = Req::new(Method::PUT, href);
        req.body = Some((ics.to_owned(), CONTENT_ICS));
        let req = match &condition {
            PutCondition::Create => req.header(header::IF_NONE_MATCH, "*")?,
            PutCondition::IfMatch(etag) => req.header(header::IF_MATCH, etag)?,
            PutCondition::Overwrite => req,
        };
        let (response, _) = self.send_ok(req).await?;
        Ok(etag_header(&response))
    }

    pub async fn delete(&self, href: &Url, etag: Option<&str>) -> Result<()> {
        let mut req = Req::new(Method::DELETE, href);
        if let Some(etag) = etag {
            req = req.header(header::IF_MATCH, etag)?;
        }
        self.send_ok(req).await?;
        Ok(())
    }

    /// MOVE without overwriting; servers that cannot move between
    /// collections get GET + PUT (create) + DELETE (If-Match) instead.
    pub async fn move_resource(&self, from: &Url, to: &Url, etag: Option<&str>) -> Result<()> {
        self.check_url(to, true)?;
        let mut req = Req::new(move_method(), from)
            .header(HeaderName::from_static("destination"), to.as_str())?
            .header(HeaderName::from_static("overwrite"), "F")?;
        if let Some(etag) = etag {
            req = req.header(header::IF_MATCH, etag)?;
        }
        match self.send_ok(req).await {
            Err(DavError::Status { status, .. }) if move_unsupported(status) => {}
            other => return other.map(|_| ()),
        }
        let resource = self.get(from).await?;
        if let (Some(expected), Some(actual)) = (etag, resource.etag.as_deref())
            && expected != actual
        {
            return Err(DavError::PreconditionFailed);
        }
        self.put(to, &resource.data, PutCondition::Create).await?;
        self.delete(from, resource.etag.as_deref()).await
    }

    /// Finds the object holding `uid` in any of `calendars`.
    pub async fn find_by_uid(&self, calendars: &[Url], uid: &str) -> Result<Option<UidMatch>> {
        let uid = xml_text(uid);
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query {XMLNS}><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="{COMPONENT_EVENT}"><c:prop-filter name="UID"><c:text-match collation="i;octet">{uid}</c:text-match></c:prop-filter></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
        );
        for calendar in calendars {
            let req = Req::new(report_method(), calendar)
                .depth(Depth::One)
                .xml(body.clone());
            let (response, base) = self.send_ok(req).await?;
            let mut found: Option<(Url, Option<String>, Option<String>)> = None;
            self.parse(response, &base, |base, r| {
                if found.is_some() || r.status.is_some_and(|s| !is_success(s)) {
                    return Ok(());
                }
                let Some(href) = r.hrefs.first() else {
                    return Ok(());
                };
                let href = normalize_href(base, href)?;
                if !same_resource(&href, calendar) {
                    found = Some((href, r.props.etag, r.props.calendar_data));
                }
                Ok(())
            })
            .await?;
            let Some((href, etag, data)) = found else {
                continue;
            };
            let resource = match data {
                Some(data) => Resource { href, etag, data },
                None => self.get(&href).await?,
            };
            return Ok(Some(UidMatch {
                calendar: calendar.clone(),
                resource,
            }));
        }
        Ok(None)
    }
}

const CALENDAR_PROPS: &str = "<d:resourcetype/><d:displayname/><ic:calendar-color/><cs:getctag/><d:sync-token/><d:supported-report-set/><c:supported-calendar-component-set/>";

fn calendar_info(href: Url, props: Props) -> CalendarInfo {
    CalendarInfo {
        href,
        supports_sync_collection: props.has_report(SYNC_COLLECTION),
        components: (!props.components.is_empty()).then_some(props.components),
        display_name: props.display_name,
        color: props.color,
        ctag: props.ctag,
        sync_token: props.sync_token,
    }
}

fn multiget_item(base: &Url, r: MsResponse) -> Result<Option<MultigetItem>> {
    let Some(href) = r.hrefs.first() else {
        return Ok(None);
    };
    let href = normalize_href(base, href)?;
    if r.status.is_some_and(|s| !is_success(s)) {
        return Ok(Some(MultigetItem::Missing(href)));
    }
    Ok(Some(match r.props.calendar_data {
        Some(data) => MultigetItem::Found(Resource {
            href,
            etag: r.props.etag,
            data,
        }),
        None => MultigetItem::Missing(href),
    }))
}

/// The href to put in a request body: a strictly encoded path when on the
/// same origin. Stalwart matches body hrefs byte-wise against its own
/// spelling, which encodes `@`.
fn request_href(base: &Url, href: &Url) -> String {
    let path = strict_path(href.path());
    let path = match href.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    if base.origin() == href.origin() {
        path
    } else {
        format!("{}{path}", href.origin().ascii_serialization())
    }
}

fn move_unsupported(status: u16) -> bool {
    [
        StatusCode::METHOD_NOT_ALLOWED,
        StatusCode::NOT_IMPLEMENTED,
        StatusCode::BAD_GATEWAY,
    ]
    .iter()
    .any(|s| s.as_u16() == status)
}

fn etag_header(response: &Response) -> Option<String> {
    response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

pub(crate) async fn read_limited(response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DavError::Network)?;
        if out.len() + chunk.len() > limit {
            return Err(DavError::ResponseTooLarge { limit });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Maps a non-2xx answer to a typed error, reading `DAV:error` bodies for
/// the preconditions callers act on.
pub(crate) async fn check_status(response: Response, url: &Url) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let url = url.clone();
    match status {
        StatusCode::UNAUTHORIZED => Err(DavError::Unauthorized),
        StatusCode::NOT_FOUND | StatusCode::GONE => Err(DavError::NotFound { url }),
        StatusCode::FORBIDDEN | StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
            let body = read_limited(response, MAX_SMALL_BODY_BYTES)
                .await
                .unwrap_or_default();
            for condition in xml::parse_error_body(&body) {
                let kind = match (condition.name.ns, condition.name.local.as_str()) {
                    (Ns::Dav, "valid-sync-token") => return Err(DavError::InvalidSyncToken),
                    (Ns::CalDav, "no-uid-conflict") => UidConflictKind::NoUidConflict,
                    (Ns::CalDav, "unique-scheduling-object-resource") => {
                        UidConflictKind::UniqueSchedulingObjectResource
                    }
                    _ => continue,
                };
                let existing = condition
                    .href
                    .as_deref()
                    .and_then(|h| normalize_href(&url, h).ok());
                return Err(DavError::UidConflict { kind, existing });
            }
            if status == StatusCode::PRECONDITION_FAILED {
                Err(DavError::PreconditionFailed)
            } else if status == StatusCode::FORBIDDEN {
                Err(DavError::Forbidden { url })
            } else {
                Err(DavError::Status {
                    status: status.as_u16(),
                    url,
                })
            }
        }
        _ => Err(DavError::Status {
            status: status.as_u16(),
            url,
        }),
    }
}
