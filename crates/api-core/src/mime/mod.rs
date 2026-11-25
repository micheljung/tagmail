//! Reading a raw message for display: its MIME tree, the body as safe
//! HTML (sanitized HTML, or plain text turned into HTML), and the parts a
//! reader can download.

pub mod reduce;
pub mod sanitize;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;

use mail_parser::{Message, MessageParser, MessagePart, PartKind};

use crate::dto::{AddressDto, BodyKind};
use sanitize::{Links, RemoteImages, Stats, normalize_cid};

pub use sanitize::BLOCKED_IMAGE;

pub fn parse(raw: &[u8]) -> Option<Message<'_>> {
    MessageParser::new().parse(raw)
}

/// Raster image type of `bytes` by magic number; SVG and everything else
/// is `None`.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"BM", "image/bmp"),
        (b"\x00\x00\x01\x00", "image/x-icon"),
    ];
    if let Some((_, ct)) = SIGNATURES.iter().find(|(sig, _)| bytes.starts_with(sig)) {
        return Some(ct);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"avif" | b"avis") {
        return Some("image/avif");
    }
    None
}

const CALENDAR_TYPES: [&str; 2] = ["text/calendar", "application/ics"];
const CALENDAR_EXTENSION: &[u8] = b".ics";

/// Whether a part carries iCalendar data, by lowercase `type/subtype` or
/// file name; sync and the invitation extractor agree through this.
pub fn is_calendar(content_type: &str, name: Option<&[u8]>) -> bool {
    CALENDAR_TYPES.contains(&content_type)
        || name.is_some_and(|n| {
            n.len() >= CALENDAR_EXTENSION.len()
                && n[n.len() - CALENDAR_EXTENSION.len()..].eq_ignore_ascii_case(CALENDAR_EXTENSION)
        })
}

/// `type/subtype` of a part, lowercased; `text/plain` when absent.
pub fn content_type(part: &MessagePart<'_>) -> String {
    part.content_type().map_or_else(
        || "text/plain".to_owned(),
        |ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            None => ct.ctype().to_ascii_lowercase(),
        },
    )
}

pub fn addresses(list: Option<mail_parser::AddressList<'_>>) -> Vec<AddressDto> {
    list.map(|l| {
        l.mailboxes()
            .filter_map(|m| {
                Some(AddressDto {
                    name: m.name().map(str::to_owned).filter(|n| !n.is_empty()),
                    email: m.address()?.to_owned(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// A part offered to the reader: everything that is neither a body text
/// nor a multipart container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInfo {
    pub id: u32,
    pub content_type: String,
    pub name: Option<String>,
    /// Decoded size in bytes.
    pub size: u64,
    pub content_id: Option<String>,
    /// Shown inside the body (referenced by `cid:` or an inline image in
    /// the body list) rather than as an attachment.
    pub inline: bool,
}

/// Decoded size of a part, also of one whose body [`reduce::reduce`] cut.
fn decoded_size(part: &MessagePart<'_>) -> u64 {
    part.headers()
        .get(reduce::SIZE_HEADER)
        .and_then(|h| std::str::from_utf8(h.raw_value()).ok()?.trim().parse().ok())
        .unwrap_or(part.decoded_len() as u64)
}

fn is_body_text(part: &MessagePart<'_>) -> bool {
    matches!(part.kind(), PartKind::Text | PartKind::Html)
        && (part.in_html_body() || part.in_text_body())
}

pub fn parts(message: &Message<'_>, referenced_cids: &[String]) -> Vec<PartInfo> {
    message
        .root()
        .parts()
        .filter(|p| !p.is_multipart() && !is_body_text(p) && p.id() != 0)
        .map(|p| {
            let content_id = p.content_id().map(str::to_owned);
            let referenced = content_id
                .as_deref()
                .is_some_and(|c| referenced_cids.contains(&normalize_cid(c)));
            PartInfo {
                id: p.id(),
                content_type: content_type(&p),
                name: p.attachment_name().map(str::to_owned),
                size: decoded_size(&p),
                content_id,
                inline: referenced || (p.in_html_body() && !p.is_attachment()),
            }
        })
        .collect()
}

/// A part's decoded bytes and type, if `id` names a leaf part.
pub fn part_content(message: &Message<'_>, id: u32) -> Option<(Vec<u8>, PartInfo)> {
    let part = message.part(id)?;
    if part.is_multipart() {
        return None;
    }
    let info = PartInfo {
        id,
        content_type: content_type(&part),
        name: part.attachment_name().map(str::to_owned),
        size: part.decoded_len() as u64,
        content_id: part.content_id().map(str::to_owned),
        inline: false,
    };
    Some((part.decoded().into_owned(), info))
}

/// Plain text as HTML: escaped, line breaks kept, `http(s)` URLs linked.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::from("<div style=\"white-space: pre-wrap\">");
    let mut rest = text;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = ["https://", "http://"]
            .iter()
            .filter_map(|p| lower.find(p))
            .min()
        else {
            escape_into(&mut out, rest);
            break;
        };
        escape_into(&mut out, &rest[..start]);
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
            .unwrap_or(tail.len());
        while tail[..end].ends_with(['.', ',', ';', ':', '!', '?', ')', ']', '\'']) {
            end -= 1;
        }
        let url = &tail[..end];
        let has_host = url
            .split_once("://")
            .is_some_and(|(_, host)| !host.is_empty());
        if has_host {
            out.push_str("<a href=\"");
            escape_into(&mut out, url);
            out.push_str("\">");
            escape_into(&mut out, url);
            out.push_str("</a>");
        } else {
            escape_into(&mut out, url);
        }
        rest = &tail[end..];
    }
    out.push_str("</div>");
    out
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// A message body ready to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub kind: BodyKind,
    /// Sanitized body fragment.
    pub html: String,
    pub stats: Stats,
    /// Normalized Content-IDs the body references.
    pub cids: Vec<String>,
}

/// Sanitized body of `message`. `part_url` signs a part id into a URL;
/// `remote` rewrites remote image URLs (`None` blocks them).
pub fn body(
    message: &Message<'_>,
    part_url: &dyn Fn(u32) -> String,
    remote: Option<RemoteImages>,
) -> Body {
    let root = message.root();
    let cid_parts: HashMap<String, u32> = root
        .parts()
        .filter_map(|p| Some((normalize_cid(p.content_id()?), p.id())))
        .collect();
    let mut source = String::new();
    let mut kind = BodyKind::None;
    let mut inline_images = HashMap::new();
    for part in root.html_body() {
        match part.kind() {
            PartKind::Html => {
                kind = BodyKind::Html;
                let (text, truncated) = capped_text(&part);
                source.push_str(&text);
                if truncated {
                    source.push_str(TRUNCATED_NOTICE);
                }
            }
            PartKind::Text => {
                if kind == BodyKind::None {
                    kind = BodyKind::Text;
                }
                let (text, truncated) = capped_text(&part);
                source.push_str(&text_to_html(&text));
                if truncated {
                    source.push_str(TRUNCATED_NOTICE);
                }
            }
            PartKind::InlineBinary | PartKind::Binary if sniff_image(&head(&part)).is_some() => {
                // Signed part URLs only pass the sanitizer as cid: targets.
                let key = format!("{INLINE_IMAGE_CID}{}", part.id());
                let _ = write!(source, "<img src=\"cid:{key}\">");
                inline_images.insert(key, part_url(part.id()));
            }
            _ => {}
        }
    }
    let referenced: Vec<String> = cid_parts
        .keys()
        .filter(|cid| contains_cid(&source, cid))
        .cloned()
        .collect();
    let mut cids: HashMap<String, String> = referenced
        .iter()
        .map(|cid| (cid.clone(), part_url(cid_parts[cid])))
        .collect();
    cids.extend(inline_images);
    let (html, stats) = sanitize::sanitize(&source, Links { cids, remote });
    Body {
        kind,
        html,
        stats,
        cids: referenced,
    }
}

/// Content-ID prefix standing for an inline image part of the body list.
const INLINE_IMAGE_CID: &str = "tagmail-inline-part-";

/// Bytes of a body text part decoded for display; the rest is cut.
pub const BODY_TEXT_CAP: usize = 1 << 20;

const TRUNCATED_NOTICE: &str = "<p><em>Message too large to display fully</em></p>";

/// The part's text up to [`BODY_TEXT_CAP`] bytes, and whether it was cut.
fn capped_text<'m>(part: &MessagePart<'m>) -> (Cow<'m, str>, bool) {
    let longest_char = 4;
    let Some(text) = part.text_prefix_bytes(BODY_TEXT_CAP + longest_char) else {
        return (Cow::Borrowed(""), false);
    };
    if text.len() <= BODY_TEXT_CAP {
        return (text, false);
    }
    let end = text.floor_char_boundary(BODY_TEXT_CAP);
    let cut = match text {
        Cow::Borrowed(t) => Cow::Borrowed(&t[..end]),
        Cow::Owned(mut t) => {
            t.truncate(end);
            Cow::Owned(t)
        }
    };
    (cut, true)
}

/// The first decoded bytes of a part, enough to sniff an image type
/// without decoding the whole part.
fn head(part: &MessagePart<'_>) -> Vec<u8> {
    let sniff_raw_bytes = 64;
    let raw = part.raw_body();
    part.encoding()
        .decode(&raw[..raw.len().min(sniff_raw_bytes)])
        .into_owned()
}

fn contains_cid(html: &str, cid: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.match_indices("cid:").any(|(i, _)| {
        let rest = &html[i + 4..];
        let end = rest
            .find(|c: char| matches!(c, '"' | '\'' | ')' | '>') || c.is_whitespace())
            .unwrap_or(rest.len());
        normalize_cid(&rest[..end]) == cid
    })
}

/// The document served by the render route; the body is already
/// sanitized and the response carries [`crate::api::RENDER_CSP`].
pub fn document(body_html: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <style>html,body{{margin:0;padding:0}}body{{font-family:system-ui,sans-serif;\
         overflow-wrap:anywhere;word-break:break-word}}img{{max-width:100%;height:auto}}\
         table{{max-width:100%}}</style></head><body>{body_html}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = concat!(
        "From: Ann <ann@example.com>\r\n",
        "To: me@example.com\r\n",
        "Subject: Logo\r\n",
        "Content-Type: multipart/mixed; boundary=\"m\"\r\n",
        "\r\n",
        "--m\r\n",
        "Content-Type: multipart/related; boundary=\"r\"\r\n",
        "\r\n",
        "--r\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<p>Hi <img src=\"cid:logo@x\"> <img src=\"https://t.example/p.gif\"></p>\r\n",
        "--r\r\n",
        "Content-Type: image/png\r\n",
        "Content-ID: <logo@x>\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "iVBORw0KGgoAAAANSUhEUg==\r\n",
        "--r--\r\n",
        "--m\r\n",
        "Content-Type: application/pdf; name=report.pdf\r\n",
        "Content-Disposition: attachment; filename=report.pdf\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "JVBERi0xLjQK\r\n",
        "--m--\r\n",
    );

    #[test]
    fn html_body_with_cid_image_and_attachment() {
        let msg = parse(MIXED.as_bytes()).unwrap();
        let body = body(&msg, &|id| format!("/api/parts/t{id}"), None);
        assert_eq!(body.kind, BodyKind::Html);
        assert_eq!(body.stats.remote_images, 1);
        assert_eq!(body.cids, ["logo@x"]);
        let logo = parts(&msg, &body.cids)
            .into_iter()
            .find(|p| p.content_type == "image/png")
            .unwrap();
        assert!(logo.inline);
        assert!(
            body.html
                .contains(&format!("src=\"/api/parts/t{}\"", logo.id)),
            "{}",
            body.html
        );
        assert!(body.html.contains(BLOCKED_IMAGE));

        let all = parts(&msg, &body.cids);
        assert_eq!(all.len(), 2, "{all:?}");
        let pdf = all
            .iter()
            .find(|p| p.content_type == "application/pdf")
            .unwrap();
        assert_eq!(pdf.name.as_deref(), Some("report.pdf"));
        assert!(!pdf.inline);
        assert_eq!(pdf.size, 9);
        let (bytes, info) = part_content(&msg, pdf.id).unwrap();
        assert_eq!(bytes, b"%PDF-1.4\n");
        assert_eq!(info.content_type, "application/pdf");
        let (png, _) = part_content(&msg, logo.id).unwrap();
        assert_eq!(sniff_image(&png), Some("image/png"));
        assert!(part_content(&msg, 0).is_none(), "multipart root");
        assert!(part_content(&msg, 999).is_none());
        assert_eq!(
            addresses(msg.from()),
            [AddressDto {
                name: Some("Ann".into()),
                email: "ann@example.com".into()
            }]
        );
    }

    #[test]
    fn inline_body_images_get_signed_part_urls_and_nothing_else_does() {
        let raw = concat!(
            "Subject: inline\r\n",
            "Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n",
            "--m\r\nContent-Type: text/html\r\n\r\n",
            "<p>see</p><img src=\"/api/parts/forged\"><img src=\"/etc/x.png\">",
            "<img src=\"//evil.example/x.png\"><img src=\"cid:tagmail-inline-part-9\">\r\n",
            "--m\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgoAAAANSUhEUg==\r\n",
            "--m--\r\n",
        );
        let msg = parse(raw.as_bytes()).unwrap();
        let body = body(&msg, &|id| format!("/api/parts/t{id}"), None);
        let png = parts(&msg, &body.cids)
            .into_iter()
            .find(|p| p.content_type == "image/png")
            .unwrap();
        let srcs: Vec<&str> = body
            .html
            .match_indices("src=\"")
            .map(|(i, _)| body.html[i + 5..].split('"').next().unwrap())
            .collect();
        assert_eq!(srcs, [format!("/api/parts/t{}", png.id)], "{}", body.html);
        assert!(png.inline);
    }

    #[test]
    fn plain_text_becomes_escaped_linked_html() {
        let raw = "Subject: t\r\n\r\n<b>x</b> & see https://ok.example/a?b=1&c=2.\r\nbye\r\n";
        let msg = parse(raw.as_bytes()).unwrap();
        let body = body(&msg, &|_| unreachable!(), None);
        assert_eq!(body.kind, BodyKind::Text);
        assert!(
            body.html.contains("&lt;b&gt;x&lt;/b&gt; &amp; see "),
            "{}",
            body.html
        );
        assert!(
            body.html.contains(
                "<a href=\"https://ok.example/a?b=1&amp;c=2\" target=\"_blank\" rel=\"noopener noreferrer\">"
            ),
            "{}",
            body.html
        );
        assert!(!body.html.contains("<b>"));
    }

    #[test]
    fn text_to_html_edges() {
        assert_eq!(
            text_to_html(""),
            "<div style=\"white-space: pre-wrap\"></div>"
        );
        assert!(text_to_html("http:// x").contains("http:// x"));
        assert!(text_to_html("(https://a.example)").contains("href=\"https://a.example\""));
        assert!(text_to_html("\"><script>").contains("&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn sniffs_raster_images_only() {
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(b"\0\0\0\x1cftypavif"), Some("image/avif"));
        assert_eq!(
            sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            None
        );
        assert_eq!(sniff_image(b"<html>"), None);
        assert_eq!(sniff_image(b""), None);
    }
}
