//! Turns FETCH results into cache rows: message identity (plan §3.4),
//! decoded envelope fields, threading keys and text extraction.

use mail_parser::{Charset, Encoding, HeaderForm, html_to_text};
use tagmail_api_core::dto::AddressDto;
use tagmail_core::email_id::normalize_message_id;
use tagmail_core::{EmailId, Flags, HeaderIdentity, KeywordSet, split_imap_flags};
use tagmail_imap::{Address, MessageMeta, PartInfo};

/// Characters kept in `message.snippet`.
pub const SNIPPET_CHARS: usize = 200;
/// Keyword some servers set on messages with a calendar part.
const HAS_CAL_KEYWORD: &str = "$has_cal";

/// Flags and keywords of one location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationState {
    pub uid: u32,
    pub flags: Flags,
    pub keywords: KeywordSet,
    pub modseq: Option<u64>,
}

/// A message as first seen in a mailbox window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub email_id: EmailId,
    /// Own `Message-ID` without angle brackets.
    pub msgid: Option<String>,
    pub in_reply_to: Option<String>,
    /// `In-Reply-To` and `References` ids, deduplicated.
    pub refs: Vec<String>,
    /// Unix seconds: `Date` header, else INTERNALDATE.
    pub date: i64,
    /// Unix seconds the server received it: INTERNALDATE, else `date`.
    /// Unlike the `Date` header, the sender cannot choose it.
    pub received: i64,
    pub subject: Option<String>,
    pub subject_norm: String,
    pub from: Vec<AddressDto>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub snippet: Option<String>,
    pub size: u32,
    pub has_attachment: bool,
    /// From BODYSTRUCTURE, or the server's `$has_cal` hint.
    pub has_calendar: bool,
}

impl NewMessage {
    /// Lowercased addresses of everyone on the message; the subject
    /// fallback needs one in common.
    pub fn participants(&self) -> Vec<String> {
        participants([&self.from, &self.to, &self.cc])
    }
}

pub fn participants<'a>(lists: impl IntoIterator<Item = &'a Vec<AddressDto>>) -> Vec<String> {
    let mut out: Vec<String> = lists
        .into_iter()
        .flatten()
        .map(|a| a.email.to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One FETCH result: always the location state, plus the message when the
/// window fetched envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub location: LocationState,
    pub message: Option<NewMessage>,
}

/// Flags and keywords of a FETCH; invalid keywords are dropped.
fn location_state(uid: u32, meta: &MessageMeta) -> LocationState {
    let names: Vec<&str> = meta
        .flags
        .iter()
        .flatten()
        .map(tagmail_imap::Flag::as_imap)
        .collect();
    let (flags, keywords) = split_imap_flags(names.iter().copied()).unwrap_or_else(|_| {
        let valid = names
            .iter()
            .copied()
            .filter(|n| n.starts_with('\\') || tagmail_core::Keyword::new(*n).is_ok());
        split_imap_flags(valid).unwrap_or_default()
    });
    LocationState {
        uid,
        flags,
        keywords,
        modseq: meta.modseq.map(|m| m.0),
    }
}

/// Converts a FETCH result; `None` without a UID.
pub fn fetched(meta: &MessageMeta) -> Option<Fetched> {
    let uid = meta.uid?.0;
    let location = location_state(uid, meta);
    let message = meta.envelope.as_ref().map(|_| {
        let mut message = new_message(meta);
        message.has_calendar |= location
            .keywords
            .iter()
            .any(|k| k.as_str().eq_ignore_ascii_case(HAS_CAL_KEYWORD));
        message
    });
    Some(Fetched { location, message })
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// RFC 2047-decoded unstructured header text.
pub fn decode_text(raw: &[u8]) -> String {
    let parsed = HeaderForm::Text.parse(raw);
    parsed
        .value()
        .as_text()
        .map_or_else(|| lossy(raw).trim().to_owned(), str::to_owned)
}

fn parse_date(raw: &[u8]) -> Option<i64> {
    let parsed = HeaderForm::Date.parse(raw);
    let date = parsed.value().as_datetime()?;
    date.is_valid().then(|| date.to_timestamp())
}

fn address(a: &Address) -> Option<AddressDto> {
    let mailbox = a.mailbox.as_deref().map(lossy)?;
    let email = match a.host.as_deref().map(lossy) {
        Some(host) if !host.is_empty() => format!("{mailbox}@{host}"),
        _ => mailbox,
    };
    let name = a.name.as_deref().map(decode_text).filter(|n| !n.is_empty());
    Some(AddressDto { name, email })
}

fn addresses(list: &[Address]) -> Vec<AddressDto> {
    list.iter().filter_map(address).collect()
}

/// Ids in `<...>` brackets, whitespace removed.
pub fn message_ids(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        let id = normalize_message_id(&rest[start..=start + len]);
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
        rest = &rest[start + len + 1..];
    }
    out
}

/// The unfolded value of header `name` in a raw header block.
pub fn header_value(block: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let mut value: Option<String> = None;
    for line in text.split("\r\n").flat_map(|l| l.split('\n')) {
        if line.starts_with([' ', '\t']) {
            if let Some(v) = value.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if value.is_some() {
            break;
        }
        if let Some((n, v)) = line.split_once(':')
            && n.trim().eq_ignore_ascii_case(name)
        {
            value = Some(v.trim().to_owned());
        }
    }
    value
}

const REPLY_PREFIXES: &[&str] = &["re", "fw", "fwd", "aw", "wg", "sv", "vs"];

/// Strips one leading `Re:`-style prefix (with an optional `[2]` or `(2)`
/// counter) or `[list]` tag; `None` if there is none.
fn strip_prefix(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[end + 1..].trim_start());
    }
    let word_end = s
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let word = &s[..word_end];
    if !REPLY_PREFIXES.iter().any(|p| p.eq_ignore_ascii_case(word)) {
        return None;
    }
    let mut rest = &s[word_end..];
    for (open, close) in [('[', ']'), ('(', ')')] {
        if let Some(inner) = rest.strip_prefix(open) {
            let end = inner.find(close)?;
            if !inner[..end].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            rest = &inner[end + 1..];
        }
    }
    let rest = rest.trim_start().strip_prefix([':', '：'])?;
    Some(rest.trim_start())
}

/// Threading subject (plan §3.5): reply/forward prefixes and `[list]` tags
/// removed, whitespace collapsed, lowercased.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    while let Some(rest) = strip_prefix(s) {
        s = rest;
    }
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn new_message(meta: &MessageMeta) -> NewMessage {
    let env = meta.envelope.clone().unwrap_or_default();
    let subject = env.subject.as_deref().map(decode_text);
    let from = addresses(&env.from);
    let date = env
        .date
        .as_deref()
        .and_then(parse_date)
        .or(meta.internal_date)
        .unwrap_or(0);
    let raw_msgid = env.message_id.as_deref().map(lossy);
    let msgid = raw_msgid
        .as_deref()
        .map(normalize_message_id)
        .filter(|id| !id.is_empty());
    let header_block = meta.header_fields.as_deref().unwrap_or_default();
    let in_reply_to_raw = env
        .in_reply_to
        .as_deref()
        .map(lossy)
        .or_else(|| header_value(header_block, "In-Reply-To"));
    let in_reply_to = in_reply_to_raw
        .as_deref()
        .and_then(|v| message_ids(v).into_iter().next());
    let mut refs = header_value(header_block, "References")
        .map(|v| message_ids(&v))
        .unwrap_or_default();
    if let Some(id) = &in_reply_to
        && !refs.contains(id)
    {
        refs.push(id.clone());
    }
    refs.retain(|r| Some(r) != msgid.as_ref());

    let email_id = meta.server_email_id().unwrap_or_else(|| {
        EmailId::from_headers(&HeaderIdentity {
            message_id: raw_msgid.as_deref(),
            date: env.date.as_deref().and_then(parse_date),
            from: from.first().map(|a| a.email.as_str()),
            subject: subject.as_deref(),
            size: meta.size.map(u64::from),
        })
    });
    let body = meta.body.clone().unwrap_or_default();
    let snippet = meta.snippet.as_deref().and_then(|bytes| {
        let part = body.parts.iter().find(|p| p.section == "1")?;
        part.mime_type
            .starts_with("text/")
            .then(|| snippet_of(&decode_part(part, bytes)))
            .filter(|s| !s.is_empty())
    });
    NewMessage {
        email_id,
        msgid,
        in_reply_to,
        refs,
        date,
        received: meta.internal_date.unwrap_or(date),
        subject_norm: subject
            .as_deref()
            .map(normalize_subject)
            .unwrap_or_default(),
        subject,
        from,
        to: addresses(&env.to),
        cc: addresses(&env.cc),
        snippet,
        size: meta.size.unwrap_or(0),
        has_attachment: body.has_attachment(),
        has_calendar: body.parts.iter().any(|p| {
            let name = p.filename.as_deref().map(decode_text);
            tagmail_api_core::mime::is_calendar(&p.mime_type, name.as_deref().map(str::as_bytes))
        }),
    }
}

/// Drops an incomplete trailing escape of a cut quoted-printable body so
/// the decoder does not reject the whole text.
fn trim_qp_tail(bytes: &[u8]) -> &[u8] {
    let n = bytes.len();
    if n >= 1 && bytes[n - 1] == b'=' {
        return &bytes[..n - 1];
    }
    if n >= 2 && bytes[n - 2] == b'=' && bytes[n - 1].is_ascii_hexdigit() {
        return &bytes[..n - 2];
    }
    bytes
}

/// Transfer- and charset-decodes a text part (possibly cut short) and
/// converts HTML to text.
pub fn decode_part(part: &PartInfo, bytes: &[u8]) -> String {
    let decoded = match part.encoding.as_str() {
        "quoted-printable" => Encoding::QuotedPrintable.decode(trim_qp_tail(bytes)),
        "base64" => Encoding::Base64.decode(bytes),
        _ => std::borrow::Cow::Borrowed(bytes),
    };
    let charset = part
        .charset
        .as_deref()
        .and_then(|c| Charset::from_label(c.as_bytes()))
        .unwrap_or_default();
    let text = charset.decode(&decoded);
    let text = text.trim_end_matches('\u{fffd}');
    if part.mime_type == "text/html" {
        html_to_text(text)
    } else {
        text.to_owned()
    }
}

/// The first [`SNIPPET_CHARS`] characters with whitespace collapsed.
pub fn snippet_of(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= SNIPPET_CHARS {
            break;
        }
    }
    out.chars().take(SNIPPET_CHARS).collect()
}

/// `Name <email>` pairs joined for the FTS address columns.
pub fn address_text(lists: &[&[AddressDto]]) -> String {
    let mut out = String::new();
    for a in lists.iter().copied().flatten() {
        if !out.is_empty() {
            out.push(' ');
        }
        if let Some(name) = &a.name {
            out.push_str(name);
            out.push(' ');
        }
        out.push_str(&a.email);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tagmail_imap::{BodySummary, Envelope, Flag, ModSeq, Uid};

    fn part(mime: &str, enc: &str, charset: Option<&str>) -> PartInfo {
        PartInfo {
            section: "1".into(),
            mime_type: mime.into(),
            charset: charset.map(Into::into),
            encoding: enc.into(),
            size: 0,
            disposition: None,
            filename: None,
        }
    }

    #[test]
    fn subject_normalization_strips_prefixes_and_tags() {
        for (input, want) in [
            ("Re: Lunch", "lunch"),
            ("RE: Fwd: [team] Lunch  plans", "lunch plans"),
            ("AW: WG: SV: VS: Lunch", "lunch"),
            ("Re[2]: Re(3): Lunch", "lunch"),
            ("Fw : Lunch", "lunch"),
            ("Return of the Jedi", "return of the jedi"),
            ("Re:", ""),
            ("[list]", ""),
            ("Regarding: x", "regarding: x"),
        ] {
            assert_eq!(normalize_subject(input), want, "{input:?}");
        }
    }

    #[test]
    fn message_ids_are_extracted_and_deduplicated() {
        assert_eq!(
            message_ids("<a@x> <b@y>\r\n <a@x> junk <c @z>"),
            ["a@x", "b@y", "c@z"]
        );
        assert!(message_ids("no ids").is_empty());
    }

    #[test]
    fn header_value_unfolds() {
        let block = b"In-Reply-To: <p@x>\r\nReferences: <a@x>\r\n <b@x>\r\nList-ID: l\r\n\r\n";
        assert_eq!(
            header_value(block, "references").as_deref(),
            Some("<a@x> <b@x>")
        );
        assert_eq!(header_value(block, "missing"), None);
    }

    #[test]
    fn truncated_quoted_printable_and_base64_tails_decode() {
        let qp = part("text/plain", "quoted-printable", Some("iso-8859-1"));
        assert_eq!(decode_part(&qp, b"caf=E9 ol=C"), "café ol");
        assert_eq!(decode_part(&qp, b"caf=E9 ol="), "café ol");
        let b64 = part("text/plain", "base64", Some("utf-8"));
        // "héllo wörld" with the last quantum cut.
        assert!(decode_part(&b64, b"aMOpbGxvIHfDtnJsZA").starts_with("héllo w"));
        let utf8_cut = part("text/plain", "8bit", Some("utf-8"));
        assert_eq!(
            decode_part(&utf8_cut, "abcé".as_bytes()[..4].as_ref()),
            "abc"
        );
    }

    #[test]
    fn html_part_becomes_text_and_charsets_decode() {
        let html = part("text/html", "7bit", None);
        assert_eq!(
            snippet_of(&decode_part(&html, b"<p>Hello <b>there</b></p>")),
            "Hello there"
        );
        let sjis = part("text/plain", "8bit", Some("shift_jis"));
        assert_eq!(decode_part(&sjis, b"\x82\xa0"), "あ");
    }

    #[test]
    fn fetched_builds_identity_threading_keys_and_snippet() {
        let meta = MessageMeta {
            uid: Some(Uid(7)),
            flags: Some(vec![
                Flag::Seen,
                Flag::Keyword("$label_Work".into()),
                Flag::Keyword("bad\u{7f}".into()),
            ]),
            modseq: Some(ModSeq(9)),
            internal_date: Some(5),
            size: Some(120),
            envelope: Some(Envelope {
                date: Some(b"Sun, 22 Nov 2026 10:00:00 +0000".to_vec()),
                subject: Some(b"Re: =?utf-8?q?Caf=C3=A9?=".to_vec()),
                from: vec![Address {
                    name: Some(b"=?utf-8?q?J=C3=B6rg?=".to_vec()),
                    mailbox: Some(b"joerg".to_vec()),
                    host: Some(b"example.com".to_vec()),
                }],
                in_reply_to: Some(b"<parent@x>".to_vec()),
                message_id: Some(b"<self@x>".to_vec()),
                ..Envelope::default()
            }),
            body: Some(BodySummary {
                parts: vec![part("text/plain", "quoted-printable", Some("utf-8"))],
            }),
            header_fields: Some(b"References: <root@x> <parent@x>\r\n\r\n".to_vec()),
            snippet: Some(b"Hi  there,\r\nsee=20you".to_vec()),
            ..MessageMeta::default()
        };
        let f = fetched(&meta).unwrap();
        assert_eq!(f.location.uid, 7);
        assert_eq!(f.location.flags, Flags::SEEN);
        assert_eq!(f.location.keywords.to_storage(), "$label_Work");
        assert_eq!(f.location.modseq, Some(9));
        let m = f.message.unwrap();
        assert_eq!(m.subject.as_deref(), Some("Re: Café"));
        assert_eq!(m.subject_norm, "café");
        assert_eq!(m.msgid.as_deref(), Some("self@x"));
        assert_eq!(m.in_reply_to.as_deref(), Some("parent@x"));
        assert_eq!(m.refs, ["root@x", "parent@x"]);
        assert_eq!(m.date, 1_795_341_600);
        assert_eq!(m.from[0].name.as_deref(), Some("Jörg"));
        assert_eq!(m.from[0].email, "joerg@example.com");
        assert_eq!(m.snippet.as_deref(), Some("Hi there, see you"));
        assert_eq!(m.email_id.source(), tagmail_core::EmailIdSource::HeaderHash);

        let with_server_id = MessageMeta {
            email_id: Some("M1".into()),
            ..meta
        };
        let m = fetched(&with_server_id).unwrap().message.unwrap();
        assert_eq!(m.email_id.as_str(), "o:M1");
        assert!(!m.has_calendar);
    }

    #[test]
    fn calendar_parts_and_the_server_hint_mark_invitations() {
        let meta = |parts: Vec<PartInfo>, flags: Vec<Flag>| MessageMeta {
            uid: Some(Uid(1)),
            flags: Some(flags),
            envelope: Some(Envelope::default()),
            body: Some(BodySummary { parts }),
            ..MessageMeta::default()
        };
        let has_calendar = |m: MessageMeta| fetched(&m).unwrap().message.unwrap().has_calendar;
        let named = |mime: &str, name: &[u8]| PartInfo {
            filename: Some(name.to_vec()),
            ..part(mime, "base64", None)
        };
        assert!(has_calendar(meta(
            vec![
                part("text/plain", "7bit", None),
                part("text/calendar", "7bit", None)
            ],
            vec![],
        )));
        assert!(has_calendar(meta(
            vec![part("application/ics", "base64", None)],
            vec![]
        )));
        assert!(has_calendar(meta(
            vec![named("application/octet-stream", b"Invite.ICS")],
            vec![],
        )));
        assert!(has_calendar(meta(
            vec![part("text/plain", "7bit", None)],
            vec![Flag::Keyword("$HAS_CAL".into())],
        )));
        assert!(has_calendar(meta(
            vec![named(
                "application/octet-stream",
                b"=?utf-8?q?Termin=2Eics?="
            )],
            vec![],
        )));
        assert!(!has_calendar(meta(
            vec![named("application/pdf", b"ics.pdf")],
            vec![],
        )));
    }
}
