use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use tokio::io::AsyncBufRead;

use crate::error::{DavError, Result};

const NS_DAV: &str = "DAV:";
const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const NS_CALSERVER: &str = "http://calendarserver.org/ns/";
const NS_APPLE: &str = "http://apple.com/ns/ical/";
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

const DEPTH_RESPONSE: usize = 2;
const DEPTH_RESPONSE_CHILD: usize = 3;
const DEPTH_PROPSTAT_CHILD: usize = 4;
const DEPTH_PROPERTY: usize = 5;
const DEPTH_PROPERTY_CHILD: usize = 6;
const DEPTH_REPORT_NAME: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ns {
    Dav,
    CalDav,
    CalServer,
    Apple,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name {
    pub ns: Ns,
    pub local: String,
}

impl Name {
    fn is(&self, ns: Ns, local: &str) -> bool {
        self.ns == ns && self.local == local
    }
}

/// The properties of one multistatus `response`, from its 2xx propstats.
#[derive(Debug, Default, Clone)]
pub(crate) struct Props {
    pub current_user_principal: Option<String>,
    pub calendar_home_set: Vec<String>,
    pub schedule_inbox: Option<String>,
    pub schedule_outbox: Option<String>,
    pub resource_types: Vec<Name>,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub etag: Option<String>,
    pub calendar_data: Option<String>,
    pub reports: Vec<Name>,
    pub components: Vec<String>,
}

impl Props {
    fn merge(&mut self, other: Props) {
        fn take(into: &mut Option<String>, from: Option<String>) {
            if from.is_some() {
                *into = from;
            }
        }
        take(
            &mut self.current_user_principal,
            other.current_user_principal,
        );
        self.calendar_home_set.extend(other.calendar_home_set);
        take(&mut self.schedule_inbox, other.schedule_inbox);
        take(&mut self.schedule_outbox, other.schedule_outbox);
        self.resource_types.extend(other.resource_types);
        take(&mut self.display_name, other.display_name);
        take(&mut self.color, other.color);
        take(&mut self.ctag, other.ctag);
        take(&mut self.sync_token, other.sync_token);
        take(&mut self.etag, other.etag);
        take(&mut self.calendar_data, other.calendar_data);
        self.reports.extend(other.reports);
        self.components.extend(other.components);
    }

    pub fn has_type(&self, ns: Ns, local: &str) -> bool {
        self.resource_types.iter().any(|t| t.is(ns, local))
    }

    pub fn has_report(&self, local: &str) -> bool {
        self.reports
            .iter()
            .any(|r| matches!(r.ns, Ns::Dav | Ns::CalDav) && r.local == local)
    }
}

#[derive(Debug, Default)]
pub(crate) struct MsResponse {
    pub hrefs: Vec<String>,
    pub status: Option<u16>,
    pub props: Props,
    pub errors: Vec<Name>,
}

#[derive(Default)]
struct Propstat {
    status: Option<u16>,
    props: Props,
}

/// Parses a multistatus body one `response` at a time; returns the
/// top-level `sync-token`, if any.
pub(crate) async fn parse_multistatus<R, F>(input: R, mut on_response: F) -> Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
    F: FnMut(MsResponse) -> Result<()>,
{
    let mut reader = NsReader::from_reader(input);
    let mut buf = Vec::new();
    let mut stack: Vec<Name> = Vec::new();
    let mut text = String::new();
    let mut response: Option<MsResponse> = None;
    let mut propstat: Option<Propstat> = None;
    let mut sync_token = None;
    loop {
        buf.clear();
        let (ns, event) = reader.read_resolved_event_into_async(&mut buf).await?;
        let ns = classify(&ns);
        match event {
            Event::Start(e) => {
                let name = element_name(ns, &e);
                check_root(&stack, &name)?;
                stack.push(name);
                text.clear();
                on_start(&stack, &e, &mut response, &mut propstat);
            }
            Event::Empty(e) => {
                let name = element_name(ns, &e);
                check_root(&stack, &name)?;
                stack.push(name);
                text.clear();
                on_start(&stack, &e, &mut response, &mut propstat);
                on_end(
                    &stack,
                    &text,
                    &mut response,
                    &mut propstat,
                    &mut sync_token,
                    &mut on_response,
                )?;
                stack.pop();
            }
            Event::End(_) => {
                on_end(
                    &stack,
                    &text,
                    &mut response,
                    &mut propstat,
                    &mut sync_token,
                    &mut on_response,
                )?;
                stack.pop();
                text.clear();
            }
            Event::Text(t) => push_text(&mut text, &t.xml10_content())?,
            Event::CData(t) => push_text(&mut text, &t.xml10_content())?,
            Event::GeneralRef(r) => {
                let resolved = if r.is_char_ref() {
                    r.resolve_char_ref()?.map(String::from)
                } else {
                    resolve_predefined_entity(&r.xml10_content()).map(str::to_owned)
                };
                let resolved = resolved.ok_or_else(|| {
                    DavError::Xml(format!("unknown entity &{};", r.xml10_content()))
                })?;
                push_text(&mut text, &resolved)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(DavError::Xml("truncated multistatus".to_owned()));
    }
    Ok(sync_token)
}

fn classify(ns: &ResolveResult<'_>) -> Ns {
    match ns {
        ResolveResult::Bound(n) => match n.as_ref() {
            NS_DAV => Ns::Dav,
            NS_CALDAV => Ns::CalDav,
            NS_CALSERVER => Ns::CalServer,
            NS_APPLE => Ns::Apple,
            _ => Ns::Other,
        },
        _ => Ns::Other,
    }
}

fn element_name(ns: Ns, e: &BytesStart<'_>) -> Name {
    Name {
        ns,
        local: e.local_name().as_ref().to_owned(),
    }
}

fn check_root(stack: &[Name], name: &Name) -> Result<()> {
    if stack.is_empty() && !name.is(Ns::Dav, "multistatus") {
        return Err(DavError::Xml(format!(
            "expected multistatus, got {}",
            name.local
        )));
    }
    Ok(())
}

fn push_text(text: &mut String, chunk: &str) -> Result<()> {
    if text.len() + chunk.len() > MAX_TEXT_BYTES {
        return Err(DavError::ResponseTooLarge {
            limit: MAX_TEXT_BYTES,
        });
    }
    text.push_str(chunk);
    Ok(())
}

fn at(stack: &[Name], depth: usize, ns: Ns, local: &str) -> bool {
    stack.get(depth).is_some_and(|n| n.is(ns, local))
}

fn in_prop(stack: &[Name]) -> bool {
    at(stack, 1, Ns::Dav, "response")
        && at(stack, 2, Ns::Dav, "propstat")
        && at(stack, 3, Ns::Dav, "prop")
}

fn on_start(
    stack: &[Name],
    e: &BytesStart<'_>,
    response: &mut Option<MsResponse>,
    propstat: &mut Option<Propstat>,
) {
    let len = stack.len();
    let current = &stack[len - 1];
    if len == DEPTH_RESPONSE && current.is(Ns::Dav, "response") {
        *response = Some(MsResponse::default());
    } else if len == DEPTH_RESPONSE_CHILD && at(stack, 1, Ns::Dav, "response") {
        if current.is(Ns::Dav, "propstat") {
            *propstat = Some(Propstat::default());
        }
    } else if len == DEPTH_PROPSTAT_CHILD && at(stack, 2, Ns::Dav, "error") {
        if let Some(r) = response.as_mut() {
            r.errors.push(current.clone());
        }
    } else if len == DEPTH_PROPERTY_CHILD && in_prop(stack) {
        let Some(ps) = propstat.as_mut() else { return };
        if at(stack, 4, Ns::Dav, "resourcetype") {
            ps.props.resource_types.push(current.clone());
        } else if at(stack, 4, Ns::CalDav, "supported-calendar-component-set")
            && current.is(Ns::CalDav, "comp")
            && let Ok(Some(attr)) = e.try_get_attribute("name")
            && let Ok(value) = attr.normalized_value(XmlVersion::Implicit1_0)
        {
            ps.props.components.push(value.to_ascii_uppercase());
        }
    } else if len == DEPTH_REPORT_NAME
        && in_prop(stack)
        && at(stack, 4, Ns::Dav, "supported-report-set")
        && at(stack, 6, Ns::Dav, "report")
        && let Some(ps) = propstat.as_mut()
    {
        ps.props.reports.push(current.clone());
    }
}

fn on_end<F>(
    stack: &[Name],
    text: &str,
    response: &mut Option<MsResponse>,
    propstat: &mut Option<Propstat>,
    sync_token: &mut Option<String>,
    on_response: &mut F,
) -> Result<()>
where
    F: FnMut(MsResponse) -> Result<()>,
{
    let len = stack.len();
    let current = &stack[len - 1];
    if len == DEPTH_RESPONSE {
        if current.is(Ns::Dav, "response") {
            if let Some(r) = response.take() {
                on_response(r)?;
            }
        } else if current.is(Ns::Dav, "sync-token") {
            *sync_token = non_empty(text);
        }
    } else if len == DEPTH_RESPONSE_CHILD && at(stack, 1, Ns::Dav, "response") {
        let Some(r) = response.as_mut() else {
            return Ok(());
        };
        if current.is(Ns::Dav, "href") {
            r.hrefs.push(text.trim().to_owned());
        } else if current.is(Ns::Dav, "status") {
            r.status = parse_status(text);
        } else if current.is(Ns::Dav, "propstat")
            && let Some(ps) = propstat.take()
            && ps.status.is_none_or(is_success)
        {
            r.props.merge(ps.props);
        }
    } else if len == DEPTH_PROPSTAT_CHILD
        && at(stack, 2, Ns::Dav, "propstat")
        && current.is(Ns::Dav, "status")
        && let Some(ps) = propstat.as_mut()
    {
        ps.status = parse_status(text);
    } else if len == DEPTH_PROPERTY
        && in_prop(stack)
        && let Some(ps) = propstat.as_mut()
    {
        let p = &mut ps.props;
        let slot = match (current.ns, current.local.as_str()) {
            (Ns::Dav, "displayname") => &mut p.display_name,
            (Ns::Dav, "sync-token") => &mut p.sync_token,
            (Ns::Dav, "getetag") => &mut p.etag,
            (Ns::CalServer, "getctag") => &mut p.ctag,
            (Ns::Apple, "calendar-color") => &mut p.color,
            (Ns::CalDav, "calendar-data") => {
                if !text.trim().is_empty() {
                    p.calendar_data = Some(text.to_owned());
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        *slot = non_empty(text);
    } else if len == DEPTH_PROPERTY_CHILD
        && in_prop(stack)
        && current.is(Ns::Dav, "href")
        && let Some(ps) = propstat.as_mut()
    {
        let href = text.trim().to_owned();
        let p = &mut ps.props;
        let property = &stack[4];
        match (property.ns, property.local.as_str()) {
            (Ns::Dav, "current-user-principal") => p.current_user_principal = Some(href),
            (Ns::CalDav, "calendar-home-set") => p.calendar_home_set.push(href),
            (Ns::CalDav, "schedule-inbox-URL") => p.schedule_inbox = Some(href),
            (Ns::CalDav, "schedule-outbox-URL") => p.schedule_outbox = Some(href),
            _ => {}
        }
    }
    Ok(())
}

fn non_empty(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// The code of an HTTP status line such as `HTTP/1.1 404 Not Found`.
pub(crate) fn parse_status(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

pub(crate) fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// A precondition element of a `DAV:error` body, with the first `href`
/// inside it.
#[derive(Debug, Clone)]
pub(crate) struct Condition {
    pub name: Name,
    pub href: Option<String>,
}

/// Parses a small `DAV:error` body (RFC 4918 §16).
pub(crate) fn parse_error_body(body: &[u8]) -> Vec<Condition> {
    let mut reader = NsReader::from_reader(body);
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut text = String::new();
    let mut found: Vec<Condition> = Vec::new();
    loop {
        buf.clear();
        let Ok((ns, event)) = reader.read_resolved_event_into(&mut buf) else {
            break;
        };
        let ns = classify(&ns);
        match event {
            Event::Start(ref e) | Event::Empty(ref e) if depth == 0 => {
                if !element_name(ns, e).is(Ns::Dav, "error") {
                    break;
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::Start(e) => {
                if depth == 1 {
                    found.push(Condition {
                        name: element_name(ns, &e),
                        href: None,
                    });
                }
                depth += 1;
                text.clear();
            }
            Event::Empty(e) if depth == 1 => found.push(Condition {
                name: element_name(ns, &e),
                href: None,
            }),
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                if depth >= 1
                    && e.local_name().as_ref() == "href"
                    && let Some(c) = found.last_mut()
                    && c.href.is_none()
                {
                    c.href = non_empty(&text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    found
}

/// Whether `body` is a `DAV:error` document, as WebDAV servers send with
/// their errors.
pub(crate) fn is_dav_error(body: &[u8]) -> bool {
    let mut reader = NsReader::from_reader(body);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_resolved_event_into(&mut buf) {
            Ok((ns, Event::Start(e) | Event::Empty(e))) => {
                return element_name(classify(&ns), &e).is(Ns::Dav, "error");
            }
            Ok((_, Event::Eof)) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

pub(crate) fn xml_text(s: &str) -> std::borrow::Cow<'_, str> {
    escape(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(xml: &str) -> (Vec<MsResponse>, Option<String>) {
        let mut out = Vec::new();
        let token = parse_multistatus(xml.as_bytes(), |r| {
            out.push(r);
            Ok(())
        })
        .await
        .unwrap();
        (out, token)
    }

    #[tokio::test]
    async fn propstat_404_is_ignored_and_entities_resolve() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
 <d:response>
  <d:href>/cal/a%20b/</d:href>
  <d:propstat><d:prop><d:displayname>Work &amp; Life &#x263A;</d:displayname>
   <cs:getctag>"42"</cs:getctag></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:sync-token/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;
        let (r, token) = parse(xml).await;
        assert_eq!(token, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].hrefs, vec!["/cal/a%20b/"]);
        assert_eq!(
            r[0].props.display_name.as_deref(),
            Some("Work & Life \u{263A}")
        );
        assert_eq!(r[0].props.ctag.as_deref(), Some("\"42\""));
        assert_eq!(r[0].props.sync_token, None);
    }

    #[tokio::test]
    async fn default_namespace_and_cdata() {
        let xml = "<multistatus xmlns=\"DAV:\"><response><href>/x.ics</href><propstat><prop>\
            <getetag>\"1\"</getetag><calendar-data xmlns=\"urn:ietf:params:xml:ns:caldav\">\
            <![CDATA[BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n]]></calendar-data></prop>\
            <status>HTTP/1.1 200 OK</status></propstat></response>\
            <sync-token>http://x/sync/7</sync-token></multistatus>";
        let (r, token) = parse(xml).await;
        assert_eq!(token.as_deref(), Some("http://x/sync/7"));
        assert_eq!(r[0].props.etag.as_deref(), Some("\"1\""));
        assert_eq!(
            r[0].props.calendar_data.as_deref(),
            Some("BEGIN:VCALENDAR\nEND:VCALENDAR\n")
        );
    }

    #[tokio::test]
    async fn rejects_non_multistatus() {
        let err = parse_multistatus("<html><body/></html>".as_bytes(), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(matches!(err, DavError::Xml(_)));
    }

    #[tokio::test]
    async fn rejects_truncated() {
        let err = parse_multistatus(
            "<d:multistatus xmlns:d=\"DAV:\"><d:response>".as_bytes(),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DavError::Xml(_)));
    }

    #[test]
    fn error_body_conditions() {
        let body = br#"<d:error xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
            <c:no-uid-conflict><d:href>/cal/other.ics</d:href></c:no-uid-conflict></d:error>"#;
        let c = parse_error_body(body);
        assert_eq!(c.len(), 1);
        assert!(c[0].name.is(Ns::CalDav, "no-uid-conflict"));
        assert_eq!(c[0].href.as_deref(), Some("/cal/other.ics"));
        let c = parse_error_body(br#"<error xmlns="DAV:"><valid-sync-token/></error>"#);
        assert!(c[0].name.is(Ns::Dav, "valid-sync-token"));
        assert!(parse_error_body(b"not xml at all").is_empty());
    }
}
