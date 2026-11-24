//! A copy of a raw message small enough to render: the MIME structure and
//! every header block stay, so part ids match the full message, while part
//! bodies are cut to what rendering reads. Read line by line from a stream,
//! so the full message is never in memory.

use std::io::{self, BufRead};

use mail_parser::{Encoding, MessageParser};

use super::BODY_TEXT_CAP;

/// Header carrying the decoded size of a part whose body was cut.
pub const SIZE_HEADER: &str = "X-Tagmail-Decoded-Size";

/// Raw bytes kept of a body text part: enough to decode its cap.
fn text_raw_cap(encoding: Encoding) -> usize {
    let slack = 1024;
    match encoding {
        Encoding::None => BODY_TEXT_CAP + slack,
        Encoding::Base64 => BODY_TEXT_CAP / 3 * 4 * 78 / 76 + slack,
        Encoding::QuotedPrintable => 3 * BODY_TEXT_CAP + slack,
    }
}
/// Raw bytes kept of any other part, enough to sniff an image type.
const HEAD_RAW_CAP: usize = 128;
const HEADER_BLOCK_CAP: usize = 1 << 20;
const HEADERS_CAP: usize = 4 << 20;
/// Open multiparts and messages past which the parser rendering reads the
/// copy with keeps parts as leaves; so does the copy.
const MAX_DEPTH: usize = 64;

enum State {
    Headers(Vec<u8>),
    Body(Leaf),
    /// Preamble or epilogue of a multipart; not part of any part.
    Between,
}

/// A part body being copied into the output after its header block.
struct Leaf {
    /// Where its size header goes if the body is cut.
    insert_at: usize,
    kept: usize,
    keep: usize,
    encoding: Encoding,
    base64_symbols: usize,
    decoded: usize,
    last_eol: usize,
    cut: bool,
}

/// An open `message/rfc822` part: where its size header goes, the depth
/// of the multipart it sits in, and where its raw body starts.
struct Nested {
    insert_at: usize,
    depth: usize,
    start: usize,
}

/// Size headers to insert into the output, by output position.
type Inserts = Vec<(usize, Vec<u8>)>;

fn eol_len(line: &[u8]) -> usize {
    if line.ends_with(b"\r\n") {
        2
    } else {
        usize::from(line.ends_with(b"\n"))
    }
}

fn size_header(size: usize) -> Vec<u8> {
    format!("{SIZE_HEADER}: {size}\r\n").into_bytes()
}

impl Leaf {
    fn push(&mut self, line: &[u8], out: &mut Vec<u8>) {
        match self.encoding {
            Encoding::Base64 => {
                self.base64_symbols += line
                    .iter()
                    .filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
                    .count();
            }
            encoding => self.decoded += encoding.decoded_len(line),
        }
        self.last_eol = eol_len(line);
        let room = self.keep.saturating_sub(self.kept);
        if line.len() > room {
            self.cut = true;
        }
        let kept = &line[..line.len().min(room)];
        self.kept += kept.len();
        out.extend_from_slice(kept);
    }

    /// `delimited`: a delimiter ends the part, taking the last line break.
    fn finish(self, out: &mut Vec<u8>, inserts: &mut Inserts, delimited: bool) {
        if self.cut {
            if delimited && !out.ends_with(b"\n") {
                out.extend_from_slice(b"\r\n");
            }
            let decoded = match self.encoding {
                Encoding::Base64 => self.base64_symbols * 3 / 4,
                _ if delimited => self.decoded.saturating_sub(self.last_eol),
                _ => self.decoded,
            };
            inserts.push((self.insert_at, size_header(decoded)));
        }
    }
}

fn blank_line_start(headers: &[u8]) -> usize {
    if headers.ends_with(b"\r\n") && headers.len() >= 2 {
        headers.len() - 2
    } else {
        headers.len().saturating_sub(1)
    }
}

fn is_blank(line: &[u8]) -> bool {
    matches!(line, b"\r\n" | b"\n")
}

fn is_size_header(line: &[u8]) -> bool {
    line.len() > SIZE_HEADER.len()
        && line[..SIZE_HEADER.len()].eq_ignore_ascii_case(SIZE_HEADER.as_bytes())
        && line[SIZE_HEADER.len()] == b':'
}

/// Where a header block leads.
enum Next {
    Multipart(String),
    Message,
    Leaf { encoding: Encoding, text: bool },
}

fn classify(headers: &[u8]) -> Next {
    let Some(message) = MessageParser::new().parse(headers) else {
        return Next::Leaf {
            encoding: Encoding::None,
            text: false,
        };
    };
    let root = message.root().root_part();
    let content_type = root.content_type();
    let ctype = content_type
        .as_ref()
        .map_or("text", |ct| ct.ctype())
        .to_ascii_lowercase();
    let subtype = content_type
        .as_ref()
        .and_then(|ct| ct.subtype())
        .map(str::to_ascii_lowercase);
    if ctype == "multipart"
        && let Some(boundary) = content_type
            .as_ref()
            .and_then(|ct| ct.attribute("boundary"))
    {
        return Next::Multipart(boundary.to_owned());
    }
    if ctype == "message" && matches!(subtype.as_deref(), Some("rfc822" | "global")) {
        return Next::Message;
    }
    Next::Leaf {
        encoding: root.encoding(),
        text: ctype == "text" && !root.is_attachment(),
    }
}

/// Index of the boundary `line` delimits, innermost first, and whether it
/// is the closing delimiter.
fn delimiter(line: &[u8], boundaries: &[String]) -> Option<(usize, bool)> {
    let rest = line.strip_prefix(b"--")?;
    boundaries.iter().enumerate().rev().find_map(|(i, b)| {
        let after = rest.strip_prefix(b.as_bytes())?;
        let (closing, tail) = match after.strip_prefix(b"--") {
            Some(tail) => (true, tail),
            None => (false, after),
        };
        tail.iter()
            .all(u8::is_ascii_whitespace)
            .then_some((i, closing))
    })
}

/// Reads `raw` and returns its reduced copy.
pub fn reduce(mut raw: impl BufRead) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut inserts = Inserts::new();
    let mut boundaries: Vec<String> = Vec::new();
    let mut state = State::Headers(Vec::new());
    let mut nested: Vec<Nested> = Vec::new();
    let mut line = Vec::new();
    let mut read = 0;
    let mut last_eol = 0;
    let mut header_bytes = 0;
    loop {
        line.clear();
        if raw.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let delimits = delimiter(&line, &boundaries);
        if let Some((i, _)) = delimits {
            while let Some(n) = nested.pop_if(|n| n.depth > i) {
                let len = read - n.start;
                let len = if len == 0 { 0 } else { len - last_eol };
                inserts.push((n.insert_at, size_header(len)));
            }
        }
        read += line.len();
        last_eol = eol_len(&line);
        if let Some((i, closing)) = delimits {
            match std::mem::replace(&mut state, State::Between) {
                State::Body(leaf) => leaf.finish(&mut out, &mut inserts, true),
                State::Headers(headers) => out.extend_from_slice(&headers),
                State::Between => {}
            }
            out.extend_from_slice(&line);
            boundaries.truncate(i + 1);
            if closing {
                boundaries.pop();
            } else {
                state = State::Headers(Vec::new());
            }
            continue;
        }
        match &mut state {
            State::Headers(headers) => {
                if is_size_header(&line) {
                    continue;
                }
                if is_blank(&line)
                    || (headers.len() + line.len() <= HEADER_BLOCK_CAP
                        && header_bytes + line.len() <= HEADERS_CAP)
                {
                    header_bytes += line.len();
                    headers.extend_from_slice(&line);
                }
                if !is_blank(&line) {
                    continue;
                }
                let headers = std::mem::take(headers);
                let deep = boundaries.len() + nested.len() > MAX_DEPTH;
                let next = match classify(&headers) {
                    Next::Multipart(_) if deep => Next::Leaf {
                        encoding: Encoding::None,
                        text: true,
                    },
                    Next::Message if deep => Next::Leaf {
                        encoding: Encoding::None,
                        text: false,
                    },
                    next => next,
                };
                state = match next {
                    Next::Multipart(boundary) => {
                        out.extend_from_slice(&headers);
                        boundaries.push(boundary);
                        State::Between
                    }
                    Next::Message => {
                        nested.push(Nested {
                            insert_at: out.len() + blank_line_start(&headers),
                            depth: boundaries.len(),
                            start: read,
                        });
                        out.extend_from_slice(&headers);
                        State::Headers(Vec::new())
                    }
                    Next::Leaf { encoding, text } => {
                        let leaf = Leaf {
                            insert_at: out.len() + blank_line_start(&headers),
                            kept: 0,
                            keep: if text {
                                text_raw_cap(encoding)
                            } else {
                                HEAD_RAW_CAP
                            },
                            encoding,
                            base64_symbols: 0,
                            decoded: 0,
                            last_eol: 0,
                            cut: false,
                        };
                        out.extend_from_slice(&headers);
                        State::Body(leaf)
                    }
                };
            }
            State::Body(leaf) => leaf.push(&line, &mut out),
            State::Between => {}
        }
    }
    match state {
        State::Body(leaf) => leaf.finish(&mut out, &mut inserts, false),
        State::Headers(headers) => out.extend_from_slice(&headers),
        State::Between => {}
    }
    while let Some(n) = nested.pop() {
        inserts.push((n.insert_at, size_header(read - n.start)));
    }
    Ok(splice(out, inserts))
}

fn splice(out: Vec<u8>, mut inserts: Inserts) -> Vec<u8> {
    inserts.sort_by_key(|(at, _)| *at);
    let mut spliced =
        Vec::with_capacity(out.len() + inserts.iter().map(|(_, h)| h.len()).sum::<usize>());
    let mut from = 0;
    for (at, header) in inserts {
        spliced.extend_from_slice(&out[from..at]);
        spliced.extend_from_slice(&header);
        from = at;
    }
    spliced.extend_from_slice(&out[from..]);
    spliced
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mime::{body, parse, parts};

    fn base64_lines(len: usize) -> String {
        "QUJD"
            .repeat(19)
            .repeat(len.div_ceil(57))
            .as_bytes()
            .chunks(76)
            .fold(String::new(), |mut out, line| {
                out.push_str(std::str::from_utf8(line).unwrap());
                out.push_str("\r\n");
                out
            })
    }

    fn nested() -> String {
        format!(
            concat!(
                "From: a@example.com\r\nSubject: nested\r\nX-Tagmail-Decoded-Size: 1\r\n",
                "Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n",
                "preamble\r\n",
                "--outer\r\nContent-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n",
                "--alt\r\nContent-Type: text/plain\r\n\r\nplain body\r\n",
                "--alt\r\nContent-Type: text/html\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n",
                "<p>html =3D body</p>\r\n",
                "--alt--\r\n",
                "--outer\r\nContent-Type: image/gif\r\nContent-ID: <g@x>\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\nR0lGODlhAQABAAAAACw=\r\n",
                "--outer\r\nContent-Type: message/rfc822\r\n\r\n",
                "From: b@example.com\r\nSubject: inner\r\n",
                "Content-Type: multipart/mixed; boundary=\"inner\"\r\n\r\n",
                "--inner\r\nContent-Type: text/plain\r\n\r\ninner text\r\n",
                "--inner\r\nContent-Type: application/pdf; name=a.pdf\r\n",
                "Content-Disposition: attachment; filename=a.pdf\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n{pdf}",
                "--inner--\r\n",
                "--outer\r\nContent-Type: text/plain; name=log.txt\r\n",
                "Content-Disposition: attachment; filename=log.txt\r\n\r\n{log}\r\n",
                "--outer\r\nContent-Type: application/zip; name=z.zip\r\n",
                "Content-Disposition: attachment; filename=z.zip\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n{zip}",
                "--outer--\r\nepilogue\r\n",
            ),
            pdf = base64_lines(5000),
            log = "log line\r\n".repeat(100),
            zip = base64_lines(300_000),
        )
    }

    fn listed(raw: &[u8]) -> (Vec<super::super::PartInfo>, String) {
        let message = parse(raw).unwrap();
        let body = body(&message, &|id| format!("/api/parts/t{id}"), None);
        (parts(&message, &body.cids), body.html)
    }

    #[test]
    fn keeps_part_ids_types_sizes_and_body() {
        let raw = nested();
        let reduced = reduce(raw.as_bytes()).unwrap();
        assert!(reduced.len() < raw.len() / 10, "{}", reduced.len());
        let (full_parts, full_html) = listed(raw.as_bytes());
        let (reduced_parts, reduced_html) = listed(&reduced);
        assert_eq!(reduced_parts, full_parts);
        assert_eq!(reduced_html, full_html);
        assert!(
            full_parts.iter().any(|p| p.size == 300_048),
            "{full_parts:?}"
        );
        let message = parse(&reduced).unwrap();
        let ids = |m: &mail_parser::Message<'_>| -> Vec<(u32, String)> {
            m.root()
                .parts()
                .map(|p| (p.id(), crate::mime::content_type(&p)))
                .collect()
        };
        assert_eq!(ids(&message), ids(&parse(raw.as_bytes()).unwrap()));
    }

    fn render(raw: &[u8]) -> std::time::Duration {
        let started = std::time::Instant::now();
        let reduced = reduce(raw).unwrap();
        listed(&reduced);
        started.elapsed()
    }

    #[test]
    fn deeply_nested_messages_render_in_linear_time() {
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n{}--b--\r\n",
            "Content-Type: message/rfc822\r\n\r\n".repeat(64 * 1024)
        );
        assert!(raw.len() > 2 << 20);
        let elapsed = render(raw.as_bytes());
        assert!(elapsed.as_millis() < 1000, "{elapsed:?}");
    }

    #[test]
    fn deeply_nested_multiparts_render_in_linear_time() {
        let mut raw: String = (0..40_000)
            .map(|i| format!("Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n"))
            .collect();
        raw.push_str(&"--x\r\n".repeat(40_000));
        assert!(raw.len() > 2 << 20);
        let elapsed = render(raw.as_bytes());
        assert!(elapsed.as_millis() < 1000, "{elapsed:?}");
    }

    #[test]
    fn nesting_up_to_the_parser_limit_keeps_part_ids() {
        for (depth, every) in (60..70)
            .chain([100])
            .flat_map(|d| [(d, 1), (d, 2), (d, 3), (d, d)])
        {
            let mut raw = String::new();
            for i in 0..depth {
                if i % every == 0 {
                    raw.push_str(&format!(
                        "Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n"
                    ));
                } else {
                    raw.push_str("Content-Type: message/rfc822\r\n\r\nSubject: s\r\n");
                }
            }
            raw.push_str(&format!(
                "Content-Type: text/plain\r\n\r\n{}\r\n",
                "t".repeat(300)
            ));
            for i in (0..depth).rev().filter(|i| i % every == 0) {
                raw.push_str(&format!(
                    "--b{i}\r\nContent-Type: image/gif\r\n\r\nGIF89a\r\n--b{i}--\r\n"
                ));
            }
            let reduced = reduce(raw.as_bytes()).unwrap();
            assert_eq!(
                listed(&reduced),
                listed(raw.as_bytes()),
                "depth {depth}/{every}"
            );
        }
    }

    #[test]
    fn a_part_cut_mid_line_keeps_the_next_part() {
        let raw = format!(
            concat!(
                "Content-Type: multipart/mixed; boundary=b\r\n\r\n",
                "--b\r\nContent-Type: application/octet-stream\r\n\r\n{}\r\n",
                "--b\r\nContent-Type: image/gif\r\n\r\nGIF89a\r\n--b--\r\n"
            ),
            "x".repeat(300)
        );
        let reduced = reduce(raw.as_bytes()).unwrap();
        assert_eq!(listed(&reduced), listed(raw.as_bytes()));
    }

    #[test]
    fn header_bytes_are_bounded_in_total() {
        let block = format!("X-Pad: {}\r\n\r\nbody\r\n", "h".repeat(1000));
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n{}--b--\r\n",
            format!("--b\r\n{block}").repeat(20_000)
        );
        let reduced = reduce(raw.as_bytes()).unwrap();
        assert!(reduced.len() < HEADERS_CAP * 2, "{}", reduced.len());
    }

    #[test]
    fn a_forged_size_header_is_dropped() {
        let raw = "Subject: x\r\nX-Tagmail-Decoded-Size: 99\r\n\r\nhi\r\n";
        let reduced = reduce(raw.as_bytes()).unwrap();
        assert_eq!(reduced, b"Subject: x\r\n\r\nhi\r\n");
    }

    #[test]
    fn a_long_text_body_is_cut_with_its_size() {
        let text = "a".repeat(text_raw_cap(Encoding::None) + 10);
        let raw = format!("Subject: x\r\n\r\n{text}\r\n");
        let reduced = reduce(raw.as_bytes()).unwrap();
        assert!(reduced.len() <= text_raw_cap(Encoding::None) + 100);
        let message = parse(&reduced).unwrap();
        let size = message
            .root()
            .root_part()
            .headers()
            .get(SIZE_HEADER)
            .map(|h| h.raw_value().trim_ascii().to_vec());
        assert_eq!(size, Some((text.len() + 2).to_string().into_bytes()));
        let html = body(&message, &|_| unreachable!(), None).html;
        assert!(html.contains("Message too large to display fully"));
    }
}
