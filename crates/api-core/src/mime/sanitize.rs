//! HTML email sanitizer (plan §3.9) on ammonia with a strict allowlist:
//! no scripts, frames, forms, embedded objects, `base`, `meta`, SVG, MathML
//! or `<style>` blocks; inline `style` is filtered per declaration and by
//! property; links open in a new browsing context without opener or
//! referrer; every resource URL is rewritten: `cid:` to a signed part URL,
//! remote images to a signed proxy URL or a blocked placeholder.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ammonia::{Builder, UrlRelative};

use crate::signing::{PARTS_PREFIX, PROXY_PREFIX};

/// 1×1 transparent GIF shown in place of a blocked remote image.
pub const BLOCKED_IMAGE: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

const TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// Removed together with their content.
const DROP_WITH_CONTENT: &[&str] = &[
    "applet", "audio", "embed", "frame", "frameset", "head", "iframe", "math", "noembed",
    "noframes", "noscript", "object", "script", "select", "style", "svg", "template", "textarea",
    "title", "video", "xmp",
];

const GENERIC_ATTRIBUTES: &[&str] = &[
    "align",
    "background",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "dir",
    "height",
    "lang",
    "nowrap",
    "style",
    "title",
    "valign",
    "width",
];

const TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href"]),
    ("img", &["src", "alt"]),
    ("ol", &["start", "type"]),
    ("ul", &["type"]),
    ("li", &["value"]),
    ("td", &["colspan", "rowspan"]),
    ("th", &["colspan", "rowspan", "scope"]),
    ("col", &["span"]),
    ("colgroup", &["span"]),
    ("font", &["color", "face", "size"]),
    ("time", &["datetime"]),
];

/// CSS properties kept in `style` attributes; positioning is left out so a
/// message cannot overlay its own frame.
const STYLE_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-style",
    "border-top-width",
    "border-width",
    "color",
    "direction",
    "display",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "list-style",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "overflow-wrap",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-indent",
    "text-transform",
    "vertical-align",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
];

/// Substrings that drop a whole CSS declaration (checked lowercased with
/// whitespace removed); backslashes are refused outright since CSS escapes
/// can spell any of them.
const CSS_FORBIDDEN: &[&str] = &[
    "url(",
    "image(",
    "image-set(",
    "expression(",
    "@import",
    "javascript:",
    "vbscript:",
    "behavior",
    "-moz-binding",
    "\\",
];

const DATA_IMAGE_PREFIXES: &[&str] = &[
    "data:image/png;base64,",
    "data:image/gif;base64,",
    "data:image/jpeg;base64,",
    "data:image/jpg;base64,",
    "data:image/webp;base64,",
];

/// Maps a remote image URL to the URL the page should load, or `None` to
/// block it.
pub type RemoteImages = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How resource URLs are rewritten for one message.
pub struct Links {
    /// Content-ID (without angle brackets, lowercased) → signed part URL.
    pub cids: HashMap<String, String>,
    /// `None`: remote images are blocked.
    pub remote: Option<RemoteImages>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Remote images seen, proxied or blocked.
    pub remote_images: u32,
}

/// Content-ID as written in `cid:` URLs and `Content-ID` headers, for
/// lookups: without `<>`, percent-decoded, lowercased.
pub fn normalize_cid(cid: &str) -> String {
    let cid = percent_decode(cid.trim());
    let cid = cid
        .strip_prefix('<')
        .and_then(|c| c.strip_suffix('>'))
        .unwrap_or(&cid);
    cid.to_ascii_lowercase()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(Some(h)), Some(Some(l))) = (
                bytes.get(i + 1).map(|b| hex(*b)),
                bytes.get(i + 2).map(|b| hex(*b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn has_scheme(url: &str, scheme: &str) -> bool {
    url.len() > scheme.len()
        && url.as_bytes()[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes())
        && url.as_bytes()[scheme.len()] == b':'
}

/// Keeps the declarations of a `style` value that are free of resource
/// loads and script; ammonia then keeps only [`STYLE_PROPERTIES`].
fn filter_style(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(';')
        .filter(|decl| {
            let squashed: String = decl
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();
            !squashed.is_empty() && !CSS_FORBIDDEN.iter().any(|f| squashed.contains(f))
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join(";"))
}

struct Filter {
    links: Links,
    remote_images: Arc<AtomicU32>,
}

impl Filter {
    fn image(&self, value: &str) -> Option<String> {
        let url = value.trim();
        if has_scheme(url, "cid") {
            return self.links.cids.get(&normalize_cid(&url[4..])).cloned();
        }
        if has_scheme(url, "http") || has_scheme(url, "https") {
            self.remote_images.fetch_add(1, Ordering::Relaxed);
            return Some(match &self.links.remote {
                Some(rewrite) => rewrite(url).unwrap_or_else(|| BLOCKED_IMAGE.to_owned()),
                None => BLOCKED_IMAGE.to_owned(),
            });
        }
        let lower = url.to_ascii_lowercase();
        DATA_IMAGE_PREFIXES
            .iter()
            .any(|p| lower.starts_with(p))
            .then(|| url.to_owned())
    }

    fn apply<'v>(&self, element: &str, attribute: &str, value: &'v str) -> Option<Cow<'v, str>> {
        match (element, attribute) {
            (_, "style") => filter_style(value).map(Cow::Owned),
            ("img", "src") | (_, "background") => self.image(value).map(Cow::Owned),
            ("a", "href") => {
                let url = value.trim();
                ["http", "https", "mailto"]
                    .iter()
                    .any(|s| has_scheme(url, s))
                    .then(|| Cow::Owned(url.to_owned()))
            }
            _ => Some(Cow::Borrowed(value)),
        }
    }
}

/// Relative URLs pass only if they are the API's own signed URLs.
fn own_url(url: &str) -> Option<Cow<'_, str>> {
    (url.starts_with(PARTS_PREFIX) || url.starts_with(PROXY_PREFIX)).then_some(Cow::Borrowed(url))
}

/// Sanitizes an HTML body (a full document or a fragment) into a body
/// fragment.
pub fn sanitize(html: &str, links: Links) -> (String, Stats) {
    let remote_images = Arc::new(AtomicU32::new(0));
    let filter = Filter {
        links,
        remote_images: remote_images.clone(),
    };
    let tag_attributes: HashMap<&str, HashSet<&str>> = TAG_ATTRIBUTES
        .iter()
        .map(|(tag, attrs)| (*tag, attrs.iter().copied().collect()))
        .collect();
    let out = Builder::empty()
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(DROP_WITH_CONTENT.iter().copied().collect())
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(tag_attributes)
        .url_schemes(["http", "https", "mailto", "cid", "data"].into())
        .url_relative(UrlRelative::Custom(Box::new(own_url)))
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .strip_comments(true)
        .filter_style_properties(STYLE_PROPERTIES.iter().copied().collect())
        .attribute_filter(move |element, attribute, value| filter.apply(element, attribute, value))
        .clean(html)
        .to_string();
    let stats = Stats {
        remote_images: remote_images.load(Ordering::Relaxed),
    };
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked() -> Links {
        Links {
            cids: HashMap::from([("logo@x".to_owned(), "/api/parts/tok".to_owned())]),
            remote: None,
        }
    }

    fn proxied() -> Links {
        Links {
            cids: HashMap::new(),
            remote: Some(Box::new(|url| {
                Some(format!("/api/proxy/img/{}", url.len()))
            })),
        }
    }

    fn clean(html: &str) -> String {
        sanitize(html, blocked()).0
    }

    /// Fails on anything that could run script, load a resource from
    /// outside, navigate the frame or submit data.
    fn assert_inert(input: &str, out: &str) {
        let lower = out.to_ascii_lowercase();
        for needle in [
            "<script",
            "javascript:",
            "vbscript:",
            "livescript:",
            "<iframe",
            "<frame",
            "<object",
            "<embed",
            "<applet",
            "<form",
            "<input",
            "<button",
            "<textarea",
            "<select",
            "<base",
            "<meta",
            "<link",
            "<style",
            "<svg",
            "<math",
            "<video",
            "<audio",
            "<source",
            "<template",
            "<isindex",
            "srcdoc",
            "srcset",
            "poster",
            "formaction",
            "http-equiv",
            "data:text",
            "data:application",
            "data:image/svg",
            "expression(",
            "url(",
            "@import",
            "behavior",
            "-moz-binding",
            "xlink:",
            "evil.example",
        ] {
            assert!(
                !lower.contains(needle),
                "{needle} survived {input:?}: {out}"
            );
        }
        let bytes = lower.as_bytes();
        let mut in_tag = false;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'<' => in_tag = true,
                b'>' => in_tag = false,
                b'o' if in_tag
                    && i > 0
                    && matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'/' | b'"' | b'\'')
                    && bytes.get(i + 1) == Some(&b'n') =>
                {
                    let rest = &lower[i + 2..];
                    let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                    let after = rest[name_len..].trim_start();
                    assert!(
                        name_len == 0 || !after.starts_with('='),
                        "event handler survived {input:?}: {out}"
                    );
                }
                _ => {}
            }
        }
    }

    const XSS: &[&str] = &[
        "<script>alert(1)</script>",
        "<SCRIPT SRC=//evil.example/x.js></SCRIPT>",
        "<scr<script>ipt>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "<img src=\"x\" ONERROR=\"alert(1)\">",
        "<img/src=x/onerror=alert(1)>",
        "<body onload=alert(1)>hi</body>",
        "<div onmouseover=\"alert(1)\">x</div>",
        "<a href=\"javascript:alert(1)\">x</a>",
        "<a href=\"JaVaScRiPt:alert(1)\">x</a>",
        "<a href=\" javascript:alert(1)\">x</a>",
        "<a href=\"java&#x09;script:alert(1)\">x</a>",
        "<a href=\"&#106;&#97;&#118;&#97;&#115;&#99;&#114;&#105;&#112;&#116;&#58;alert(1)\">x</a>",
        "<a href=\"vbscript:msgbox(1)\">x</a>",
        "<a href=\"data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==\">x</a>",
        "<img src=\"data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=\">",
        "<img src=\"data:text/html,<script>alert(1)</script>\">",
        "<svg onload=alert(1)><circle r=1 /></svg>",
        "<svg><a xlink:href=\"javascript:alert(1)\"><text>x</text></a></svg>",
        "<math><mtext><table><mglyph><style><img src=x onerror=alert(1)>",
        "<iframe src=\"https://evil.example\"></iframe>",
        "<iframe srcdoc=\"<script>alert(1)</script>\"></iframe>",
        "<object data=\"https://evil.example/x.swf\"></object>",
        "<embed src=\"https://evil.example/x.swf\">",
        "<applet code=\"x\"></applet>",
        "<form action=\"https://evil.example\"><input name=p><button>go</button></form>",
        "<button formaction=\"javascript:alert(1)\">x</button>",
        "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">",
        "<base href=\"https://evil.example/\"><a href=\"x\">x</a>",
        "<link rel=stylesheet href=\"https://evil.example/x.css\">",
        "<style>@import 'https://evil.example/x.css';</style>",
        "<style>body{background:url(https://evil.example/x)}</style>",
        "<div style=\"background:url(https://evil.example/x)\">x</div>",
        "<div style=\"background-image: URL( 'https://evil.example/x' )\">x</div>",
        "<div style=\"width: expression(alert(1))\">x</div>",
        "<div style=\"behavior: url(x.htc)\">x</div>",
        "<div style=\"-moz-binding: url(https://evil.example/x.xml#x)\">x</div>",
        "<div style=\"background:u\\72l(https://evil.example/x)\">x</div>",
        "<div style=\"color:red;background-image:image-set('https://evil.example/x' 1x)\">x</div>",
        "<table background=\"javascript:alert(1)\"><tr><td>x</td></tr></table>",
        "<td background=\"https://evil.example/bg.png\">x</td>",
        "<img srcset=\"https://evil.example/a.png 1x\" src=\"cid:logo@x\">",
        "<picture><source srcset=\"https://evil.example/a.png\"><img src=\"cid:logo@x\"></picture>",
        "<video poster=\"https://evil.example/p.png\"><source src=\"x.mp4\"></video>",
        "<audio src=\"https://evil.example/a.mp3\"></audio>",
        "<isindex action=\"javascript:alert(1)\">",
        "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\"></noscript>",
        "<template><script>alert(1)</script></template>",
        "<a href=\"https://ok.example\" ping=\"https://evil.example/ping\">x</a>",
        "<img src=\"https://evil.example/track.gif\" width=1 height=1>",
        "<!--<img src=x onerror=alert(1)>-->",
        "<xmp><script>alert(1)</script></xmp>",
        "<div id=x tabindex=1 onfocus=alert(1) autofocus>x</div>",
        "<details open ontoggle=alert(1)>x</details>",
        "<marquee onstart=alert(1)>x</marquee>",
    ];

    #[test]
    fn xss_corpus_is_neutralized() {
        assert!(XSS.len() >= 40);
        for input in XSS {
            assert_inert(input, &clean(input));
            assert_inert(
                input,
                &sanitize(input, proxied()).0.replace("/api/proxy/img/", ""),
            );
        }
    }

    #[test]
    fn links_open_in_a_new_context_without_referrer() {
        let out =
            clean("<a href=\"https://ok.example/a?b=c\" target=\"_self\" rel=\"opener\">x</a>");
        assert_eq!(
            out,
            "<a href=\"https://ok.example/a?b=c\" target=\"_blank\" rel=\"noopener noreferrer\">x</a>"
        );
        assert!(clean("<a href=\"mailto:a@b.c\">m</a>").contains("href=\"mailto:a@b.c\""));
        assert!(!clean("<a href=\"/relative\">r</a>").contains("href"));
        assert!(!clean("<a href=\"/api/parts/x\">r</a>").contains("href"));
    }

    #[test]
    fn remote_images_are_blocked_or_proxied() {
        let html = "<img src=\"https://ok.example/a.png\" alt=a>\
                    <table><tr><td background=\"http://ok.example/b.png\">x</td></tr></table>";
        let (out, stats) = sanitize(html, blocked());
        assert_eq!(stats.remote_images, 2);
        assert_eq!(out.matches(BLOCKED_IMAGE).count(), 2, "{out}");
        assert!(!out.contains("ok.example"));

        let (out, stats) = sanitize(html, proxied());
        assert_eq!(stats.remote_images, 2);
        assert!(out.contains("src=\"/api/proxy/img/24\""), "{out}");
        assert!(out.contains("background=\"/api/proxy/img/23\""), "{out}");
    }

    #[test]
    fn cid_images_get_signed_part_urls() {
        let out = clean(
            "<img src=\"cid:LOGO@x\"><img src=\"CID:%3Clogo@x%3E\"><img src=\"cid:other@x\">",
        );
        assert_eq!(out.matches("src=\"/api/parts/tok\"").count(), 2, "{out}");
        assert_eq!(out.matches("<img").count(), 3);
        assert_eq!(out.matches("src=").count(), 2);
    }

    #[test]
    fn data_images_and_safe_styles_survive() {
        let png = "data:image/png;base64,iVBORw0KGgo=";
        assert!(clean(&format!("<img src=\"{png}\">")).contains(png));
        let out = clean(
            "<p style=\"color: red; position: fixed; background: url(x); font-weight: bold\">x</p>",
        );
        assert!(out.contains("color"), "{out}");
        assert!(out.contains("font-weight"), "{out}");
        assert!(!out.contains("position"), "{out}");
        assert!(!out.contains("url"), "{out}");
    }

    #[test]
    fn documents_lose_head_and_keep_body() {
        let out = clean(
            "<html><head><title>T</title><style>p{}</style></head><body><p>Hello</p></body></html>",
        );
        assert_eq!(out, "<p>Hello</p>");
    }

    #[test]
    fn cids_normalize() {
        assert_eq!(normalize_cid(" <Part1.ABC@x> "), "part1.abc@x");
        assert_eq!(normalize_cid("a%40b"), "a@b");
        assert_eq!(normalize_cid("%zz"), "%zz");
    }
}
