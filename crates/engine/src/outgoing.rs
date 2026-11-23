//! Building outgoing messages (plan §3.5): RFC 5322 with mail-builder,
//! multipart/alternative when there is HTML, attachments, reply
//! references. HTML is sanitized before it leaves (no script, no event
//! handlers, no forms).
//!
//! Attachments never sit in memory: mail-builder lays out the message with
//! a placeholder per attachment, and [`Outgoing::write_to`] replaces each
//! with the file's base64, encoded chunk by chunk while it is written.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mail_builder::mime::MimePart;

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::message_id::MessageId;
use tagmail_api_core::dto::AddressDto;

/// Bytes of randomness in a generated Message-ID.
const MESSAGE_ID_RANDOM: usize = 16;
/// Input bytes per base64 line (76 characters, RFC 2045).
const LINE_INPUT: usize = 57;
/// Attachment bytes read at a time: whole base64 lines.
const READ_CHUNK: usize = LINE_INPUT * 1024;

/// An attachment, read from its file while the message is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingFile {
    pub name: String,
    pub content_type: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub from: AddressDto,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    /// Without angle brackets.
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Unix seconds.
    pub date: i64,
    pub attachments: Vec<OutgoingFile>,
}

/// Whether the Bcc header is written: kept in a draft so it survives a
/// reload, never in what is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Draft,
    Send,
}

/// `<random>@<domain of the From address>`, without angle brackets.
pub fn new_message_id(from: &str) -> String {
    let domain = from
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_graphic() && b != b'>'))
        .unwrap_or("tagmail.invalid");
    let mut random = [0u8; MESSAGE_ID_RANDOM];
    getrandom::fill(&mut random).expect("the OS random source is available");
    let mut id = String::with_capacity(MESSAGE_ID_RANDOM * 2 + 1 + domain.len());
    for b in random {
        let _ = write!(id, "{b:02x}");
    }
    id.push('@');
    id.push_str(domain);
    id
}

/// Sanitized HTML for an outgoing body.
pub fn sanitize_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_generic_attributes(["style"])
        .clean(html)
        .to_string()
}

fn address(a: &AddressDto) -> Address<'_> {
    Address::new_address(a.name.as_deref(), a.email.as_str())
}

fn list(addresses: &[AddressDto]) -> Address<'_> {
    Address::new_list(addresses.iter().map(address).collect())
}

impl Outgoing {
    /// Every recipient (To, Cc and Bcc), each address once.
    pub fn recipient_list(&self) -> Vec<AddressDto> {
        let mut seen = HashSet::new();
        self.to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .filter(|a| seen.insert(a.email.to_ascii_lowercase()))
            .cloned()
            .collect()
    }

    /// Writes the message to `out`, attachments streamed from their files.
    pub fn write_to(&self, purpose: Purpose, out: &mut impl Write) -> io::Result<()> {
        let html = self.html.as_deref().map(sanitize_html);
        let text = match (&self.text, &html) {
            (Some(text), _) => text.clone(),
            (None, Some(html)) => mail_parser::html_to_text(html),
            (None, None) => String::new(),
        };
        let mut m = MessageBuilder::new()
            .from(address(&self.from))
            .subject(self.subject.as_str())
            .date(self.date)
            .message_id(self.message_id.as_str());
        if !self.to.is_empty() {
            m = m.to(list(&self.to));
        }
        if !self.cc.is_empty() {
            m = m.cc(list(&self.cc));
        }
        if purpose == Purpose::Draft && !self.bcc.is_empty() {
            m = m.bcc(list(&self.bcc));
        }
        if let Some(parent) = &self.in_reply_to {
            m = m.in_reply_to(parent.as_str());
        }
        if !self.references.is_empty() {
            m = m.references(MessageId::new_list(
                self.references.iter().map(String::as_str),
            ));
        }
        m = m.text_body(text);
        if let Some(html) = html {
            m = m.html_body(html);
        }
        let token = placeholder_token();
        let placeholders: Vec<String> = (0..self.attachments.len())
            .map(|i| format!("{token}x{i}"))
            .collect();
        for (a, placeholder) in self.attachments.iter().zip(&placeholders) {
            let part = MimePart::new(a.content_type.as_str(), placeholder.as_str())
                .attachment(a.name.as_str())
                .transfer_encoding("base64");
            m.attachments.get_or_insert_with(Vec::new).push(part);
        }
        let layout = m.write_to_vec()?;
        let mut rest = layout.as_slice();
        for (a, placeholder) in self.attachments.iter().zip(&placeholders) {
            let at = find(rest, placeholder.as_bytes()).ok_or_else(|| {
                io::Error::other("attachment placeholder missing from the layout")
            })?;
            out.write_all(&rest[..at])?;
            write_base64(&mut File::open(&a.path)?, out)?;
            rest = &rest[at + placeholder.len()..];
        }
        out.write_all(rest)
    }
}

fn placeholder_token() -> String {
    let mut random = [0u8; MESSAGE_ID_RANDOM];
    getrandom::fill(&mut random).expect("the OS random source is available");
    let mut token = String::from("tagmailattachment");
    for b in random {
        let _ = write!(token, "{b:02x}");
    }
    token
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Base64 in CRLF-terminated 76-character lines, without a final CRLF.
fn write_base64(input: &mut impl Read, out: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut line = [0u8; 78];
    let mut first = true;
    loop {
        let mut filled = 0;
        while filled < READ_CHUNK {
            let n = input.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        for chunk in buf[..filled].chunks(LINE_INPUT) {
            if !first {
                out.write_all(b"\r\n")?;
            }
            first = false;
            let n = STANDARD
                .encode_slice(chunk, &mut line)
                .map_err(io::Error::other)?;
            out.write_all(&line[..n])?;
        }
        if filled < READ_CHUNK {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use mail_parser::MessageParser;
    use tempfile::TempDir;

    use super::*;

    const PDF: &[u8] = b"%PDF-1.4 binary \x00\xff";

    fn build(o: &Outgoing, purpose: Purpose) -> Vec<u8> {
        let mut out = Vec::new();
        o.write_to(purpose, &mut out).unwrap();
        out
    }

    fn file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn addr(name: Option<&str>, email: &str) -> AddressDto {
        AddressDto {
            name: name.map(str::to_owned),
            email: email.to_owned(),
        }
    }

    fn outgoing(dir: &TempDir) -> Outgoing {
        Outgoing {
            from: addr(Some("Me Myself"), "me@a.example"),
            to: vec![addr(Some("Ann"), "ann@x.example")],
            cc: vec![addr(None, "cat@x.example")],
            bcc: vec![addr(None, "secret@y.example"), addr(None, "ANN@x.example")],
            subject: "Grüße".to_owned(),
            text: None,
            html: Some(
                "<p onclick=\"x()\">Hello <b>there</b></p><script>alert(1)</script>".to_owned(),
            ),
            message_id: "m1@a.example".to_owned(),
            in_reply_to: Some("p2@x.example".to_owned()),
            references: vec!["p1@x.example".to_owned(), "p2@x.example".to_owned()],
            date: 1_790_000_000,
            attachments: vec![OutgoingFile {
                name: "report.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                path: file(dir, "report.pdf", PDF),
            }],
        }
    }

    #[test]
    fn sent_message_has_no_bcc_and_threads() {
        let dir = TempDir::new().unwrap();
        let o = outgoing(&dir);
        let raw = build(&o, Purpose::Send);
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.to_ascii_lowercase().contains("bcc:"), "{text}");
        assert!(!text.contains("secret@y.example"), "{text}");
        let m = MessageParser::new().parse(&raw).unwrap();
        assert_eq!(m.message_id(), Some("m1@a.example"));
        assert_eq!(
            m.in_reply_to().and_then(|l| l.first()),
            Some("p2@x.example")
        );
        let refs: Vec<&str> = m.references().unwrap().iter().collect();
        assert_eq!(refs, ["p1@x.example", "p2@x.example"]);
        assert_eq!(m.subject(), Some("Grüße"));
        assert_eq!(m.date().unwrap().to_timestamp(), 1_790_000_000);
        let from = m.from().unwrap().first().unwrap();
        assert_eq!(
            (from.name(), from.address()),
            (Some("Me Myself"), Some("me@a.example"))
        );
        assert_eq!(
            m.to().unwrap().first().unwrap().address(),
            Some("ann@x.example")
        );

        let html = m.body_html(0).unwrap();
        assert!(html.contains("<b>there</b>"), "{html}");
        assert!(
            !html.contains("script") && !html.contains("onclick"),
            "{html}"
        );
        let plain = m.body_text(0).unwrap();
        assert!(plain.contains("Hello there"), "{plain}");
        assert!(
            m.root_part().is_content_type("multipart", "mixed"),
            "{text}"
        );

        let attachments: Vec<_> = m.attachments().collect();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].attachment_name(), Some("report.pdf"));
        assert_eq!(&*attachments[0].decoded(), PDF);

        let emails: Vec<String> = o.recipient_list().into_iter().map(|a| a.email).collect();
        assert_eq!(
            emails,
            ["ann@x.example", "cat@x.example", "secret@y.example"]
        );
    }

    #[test]
    fn draft_keeps_bcc() {
        let dir = TempDir::new().unwrap();
        let raw = build(&outgoing(&dir), Purpose::Draft);
        let m = MessageParser::new().parse(&raw).unwrap();
        assert_eq!(
            m.bcc().unwrap().first().unwrap().address(),
            Some("secret@y.example")
        );
    }

    #[test]
    fn plain_message_is_single_part() {
        let dir = TempDir::new().unwrap();
        let mut o = outgoing(&dir);
        o.html = None;
        o.attachments.clear();
        o.in_reply_to = None;
        o.references.clear();
        o.text = Some("Just text\n".to_owned());
        let raw = build(&o, Purpose::Send);
        let m = MessageParser::new().parse(&raw).unwrap();
        assert!(m.root_part().is_content_type("text", "plain"));
        assert_eq!(m.in_reply_to(), None);
        assert_eq!(m.body_text(0).as_deref(), Some("Just text\r\n"));
    }

    #[test]
    fn large_attachments_stream_as_base64_lines() {
        let dir = TempDir::new().unwrap();
        let big: Vec<u8> = (0..(READ_CHUNK * 3 + 100))
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let mut o = outgoing(&dir);
        o.attachments.push(OutgoingFile {
            name: "big.bin".to_owned(),
            content_type: "application/octet-stream".to_owned(),
            path: file(&dir, "big.bin", &big),
        });
        o.attachments.push(OutgoingFile {
            name: "empty.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            path: file(&dir, "empty.txt", b""),
        });
        let raw = build(&o, Purpose::Send);
        assert!(
            raw.split(|&b| b == b'\n').all(|line| line.len() <= 78),
            "lines within RFC 5322 limits"
        );
        assert!(!String::from_utf8_lossy(&raw).contains("tagmailattachment"));
        let m = MessageParser::new().parse(&raw).unwrap();
        let attachments: Vec<_> = m.attachments().collect();
        assert_eq!(attachments.len(), 3);
        assert_eq!(&*attachments[0].decoded(), PDF);
        assert_eq!(&*attachments[1].decoded(), big.as_slice());
        assert!(attachments[2].decoded().is_empty());
    }

    #[test]
    fn message_ids_are_unique_on_the_sender_domain() {
        let a = new_message_id("me@a.example");
        let b = new_message_id("me@a.example");
        assert_ne!(a, b);
        assert!(a.ends_with("@a.example"), "{a}");
        assert!(new_message_id("bad").ends_with("@tagmail.invalid"));
    }
}
