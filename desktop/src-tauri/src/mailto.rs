//! Links the OS hands the app: `mailto:` (RFC 6068) and
//! `tagmail://pair?server=…&code=…`.

use std::sync::Mutex;

use percent_encoding::percent_decode_str;
use tagmail_api_core::desktop_dto::{DesktopPairDto, MailtoDto};
use url::Url;

use crate::events::Emit;

pub const MAILTO_EVENT: &str = "desktop:mailto";
pub const PAIR_EVENT: &str = "desktop:pair";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Mailto(MailtoDto),
    Pair(DesktopPairDto),
}

/// Hands links to the SPA. A link that arrives before the SPA listens (the
/// app was started by it) waits until the SPA asks for the status, which it
/// does once its listeners are registered.
pub struct LinkQueue {
    emit: Box<dyn Emit>,
    state: Mutex<(bool, Vec<Link>)>,
}

impl LinkQueue {
    pub fn new(emit: Box<dyn Emit>) -> LinkQueue {
        LinkQueue {
            emit,
            state: Mutex::new((false, Vec::new())),
        }
    }

    fn send(&self, link: &Link) {
        let (event, payload) = match link {
            Link::Mailto(m) => (MAILTO_EVENT, serde_json::to_value(m)),
            Link::Pair(p) => (PAIR_EVENT, serde_json::to_value(p)),
        };
        self.emit
            .emit(event, payload.expect("DTOs serialize infallibly"));
    }

    pub fn push(&self, link: Link) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 {
            self.send(&link);
        } else {
            state.1.push(link);
        }
    }

    /// The SPA listens now: deliver what waited.
    pub fn ready(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = true;
        for link in std::mem::take(&mut state.1) {
            self.send(&link);
        }
    }

    /// The page is going away (reload); hold links until it is back.
    pub fn unready(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = false;
    }
}

pub fn parse_link(raw: &str) -> Option<Link> {
    let raw = raw.trim();
    let scheme = raw.split_once(':')?.0;
    if scheme.eq_ignore_ascii_case("mailto") {
        return Some(Link::Mailto(parse_mailto(raw)?));
    }
    if !scheme.eq_ignore_ascii_case("tagmail") {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.host_str() != Some("pair") {
        return None;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    Some(Link::Pair(DesktopPairDto {
        server: param("server")?,
        code: param("code")?,
    }))
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn addresses(list: &str) -> impl Iterator<Item = String> + '_ {
    list.split(',')
        .map(|a| decode(a).trim().to_owned())
        .filter(|a| !a.is_empty())
}

/// `to` addresses come from the path and every `to` hfield; `cc`, `bcc`,
/// `subject` and `body` are kept, every other hfield (e.g. `attach`) is
/// dropped. `+` is a literal plus, as RFC 6068 says.
pub fn parse_mailto(raw: &str) -> Option<MailtoDto> {
    let (scheme, rest) = raw.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("mailto") {
        return None;
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut out = MailtoDto {
        to: addresses(path).collect(),
        ..MailtoDto::default()
    };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match decode(name).to_ascii_lowercase().as_str() {
            "to" => out.to.extend(addresses(value)),
            "cc" => out.cc.extend(addresses(value)),
            "bcc" => out.bcc.extend(addresses(value)),
            "subject" => out.subject = Some(decode(value)),
            "body" => out.body = Some(decode(value).replace("\r\n", "\n")),
            _ => {}
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<(String, Value)>>>);

    impl Emit for Recorder {
        fn emit(&self, event: &str, payload: Value) {
            self.0.lock().unwrap().push((event.to_owned(), payload));
        }
    }

    #[test]
    fn links_wait_until_the_spa_listens() {
        let seen = Recorder::default();
        let queue = LinkQueue::new(Box::new(seen.clone()));
        queue.push(parse_link("mailto:a@x.example").unwrap());
        assert!(seen.0.lock().unwrap().is_empty());
        queue.ready();
        queue.push(parse_link("tagmail://pair?server=s&code=c").unwrap());
        let events: Vec<String> = seen
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        assert_eq!(events, [MAILTO_EVENT, PAIR_EVENT]);
        assert_eq!(seen.0.lock().unwrap()[0].1["to"][0], "a@x.example");
        queue.unready();
        queue.push(parse_link("mailto:b@x.example").unwrap());
        assert_eq!(seen.0.lock().unwrap().len(), 2);
    }

    fn mailto(raw: &str) -> MailtoDto {
        parse_mailto(raw).unwrap()
    }

    #[test]
    fn follows_rfc_6068() {
        let m = mailto(
            "mailto:a@x.example,b@x.example?to=c@x.example&cc=d@x.example&bcc=e@x.example&subject=Hi%20there&body=Line%201%0D%0ALine%202",
        );
        assert_eq!(m.to, ["a@x.example", "b@x.example", "c@x.example"]);
        assert_eq!(m.cc, ["d@x.example"]);
        assert_eq!(m.bcc, ["e@x.example"]);
        assert_eq!(m.subject.as_deref(), Some("Hi there"));
        assert_eq!(m.body.as_deref(), Some("Line 1\nLine 2"));
    }

    #[test]
    fn decodes_reserved_characters_and_keeps_plus() {
        let m = mailto("mailto:%22not%40me%22@x.example?subject=a%3Fb%26c%3Dd+e");
        assert_eq!(m.to, ["\"not@me\"@x.example"]);
        assert_eq!(m.subject.as_deref(), Some("a?b&c=d+e"));
        let m = mailto("MAILTO:user+tag@x.example");
        assert_eq!(m.to, ["user+tag@x.example"]);
    }

    #[test]
    fn ignores_attachments_and_unknown_fields() {
        let m = mailto(
            "mailto:a@x.example?attach=/etc/passwd&attachment=%2Fetc%2Fshadow&in-reply-to=%3Cx%3E&X-Foo=1",
        );
        assert_eq!(
            m,
            MailtoDto {
                to: vec!["a@x.example".into()],
                ..MailtoDto::default()
            }
        );
    }

    #[test]
    fn allows_an_empty_address_list() {
        let m = mailto("mailto:?subject=x");
        assert!(m.to.is_empty());
        assert_eq!(m.subject.as_deref(), Some("x"));
    }

    #[test]
    fn recognises_pair_links_only() {
        assert_eq!(
            parse_link("tagmail://pair?server=https%3A%2F%2Fmail.example.com&code=K7QD2XPM"),
            Some(Link::Pair(DesktopPairDto {
                server: "https://mail.example.com".into(),
                code: "K7QD2XPM".into(),
            }))
        );
        assert_eq!(parse_link("tagmail://pair?server=x"), None);
        assert_eq!(parse_link("tagmail://other?server=x&code=y"), None);
        assert_eq!(parse_link("https://example.com"), None);
        assert!(matches!(
            parse_link("mailto:a@x.example"),
            Some(Link::Mailto(_))
        ));
    }
}
