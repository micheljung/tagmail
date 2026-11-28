//! A copy of a raw message small enough to render: the MIME structure and
//! every header block stay, so part ids match the full message, while part
//! bodies are cut to what rendering reads. Read line by line from a stream,
//! so the full message is never in memory.

use std::io::{self, BufRead};

use mail_parser::{Charset, Encoding, MessageParser};

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
/// Nesting depth (open multiparts plus messages) past which the parser
/// that renders the copy keeps parts as leaves; the copy does the same.
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

/// Raw bytes kept of one calendar part.
pub const CALENDAR_PART_CAP: usize = 1 << 20;
/// Calendar parts kept per message; later ones are only counted.
pub const CALENDAR_PARTS_MAX: usize = 8;
/// Raw bytes kept of all calendar parts together.
pub const CALENDAR_TOTAL_CAP: usize = 2 << 20;
/// Longest piece read at once, so a body without line breaks never sits
/// in memory whole.
const CHUNK_CAP: usize = 64 << 10;
const SENDER_FIELDS: [&[u8]; 3] = [b"from", b"sender", b"authentication-results"];

/// An iCalendar part of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarPart {
    /// The part id, as in [`super::PartInfo::id`].
    pub id: u32,
    pub content_type: String,
    pub name: Option<String>,
    /// Transfer- and charset-decoded; empty when `too_large`.
    pub text: String,
    /// Over [`CALENDAR_PART_CAP`], past [`CALENDAR_TOTAL_CAP`] with the
    /// parts before it, or cut off by the end of a truncated message.
    pub too_large: bool,
}

/// What an invitation is read from: the top-level calendar parts and the
/// sender fields of the message header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarParts {
    /// The `From`, `Sender` and `Authentication-Results` fields of the
    /// top-level header block as raw folded lines, in their order.
    pub headers: Vec<u8>,
    pub parts: Vec<CalendarPart>,
    /// Calendar parts past [`CALENDAR_PARTS_MAX`].
    pub omitted: usize,
}

impl CalendarParts {
    pub fn truncated(&self) -> bool {
        self.omitted > 0 || self.parts.iter().any(|p| p.too_large)
    }
}

/// A calendar part body being collected.
struct Collect {
    part: CalendarPart,
    encoding: Encoding,
    charset: Option<String>,
    raw: Vec<u8>,
    last_eol: usize,
}

enum Calendar {
    Headers(Vec<u8>),
    Body(Collect),
    /// A body that is not kept, a preamble or an epilogue.
    Skip,
}

enum Block {
    Multipart(String),
    Message,
    Leaf(Option<Collect>),
}

fn block(headers: &[u8], id: u32, deep: bool) -> Block {
    let Some(message) = MessageParser::new().parse(headers) else {
        return Block::Leaf(None);
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
        return if deep {
            Block::Leaf(None)
        } else {
            Block::Multipart(boundary.to_owned())
        };
    }
    if ctype == "message" && matches!(subtype.as_deref(), Some("rfc822" | "global")) {
        return if deep {
            Block::Leaf(None)
        } else {
            Block::Message
        };
    }
    let mime_type = super::content_type(&root);
    let name = root.attachment_name();
    if !super::is_calendar(&mime_type, name.map(str::as_bytes)) {
        return Block::Leaf(None);
    }
    Block::Leaf(Some(Collect {
        part: CalendarPart {
            id,
            content_type: mime_type,
            name: name.map(str::to_owned),
            text: String::new(),
            too_large: false,
        },
        encoding: root.encoding(),
        charset: content_type
            .as_ref()
            .and_then(|ct| ct.attribute("charset"))
            .map(str::to_owned),
        raw: Vec::new(),
        last_eol: 0,
    }))
}

impl Collect {
    fn cut(&mut self) {
        self.part.too_large = true;
        self.raw = Vec::new();
    }

    fn push(&mut self, chunk: &[u8], room: usize) {
        if self.part.too_large {
            return;
        }
        if self.raw.len() + chunk.len() > room {
            self.cut();
            return;
        }
        self.last_eol = eol_len(chunk);
        self.raw.extend_from_slice(chunk);
    }

    /// `delimited`: a delimiter ends the part, taking the last line break.
    fn finish(mut self, delimited: bool) -> CalendarPart {
        if self.part.too_large {
            return self.part;
        }
        if delimited {
            self.raw.truncate(self.raw.len() - self.last_eol);
        }
        let decoded = self.encoding.decode(&self.raw);
        let charset = self
            .charset
            .as_deref()
            .and_then(|c| Charset::from_label(c.as_bytes()))
            .unwrap_or_default();
        self.part.text = charset.decode(&decoded).into_owned();
        self.part
    }
}

/// The fields of `headers` named in [`SENDER_FIELDS`], with their folded
/// continuation lines.
fn sender_fields(headers: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut keep = false;
    for line in headers.split_inclusive(|b| *b == b'\n') {
        if !line.starts_with(b" ") && !line.starts_with(b"\t") {
            let name = line.split(|b| *b == b':').next().unwrap_or_default();
            keep = line.contains(&b':')
                && SENDER_FIELDS
                    .iter()
                    .any(|f| name.trim_ascii().eq_ignore_ascii_case(f));
        }
        if keep {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Reads up to a line break or [`CHUNK_CAP`] bytes into `chunk`.
fn read_chunk(raw: &mut impl BufRead, chunk: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        let available = match raw.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(chunk.len());
        }
        let room = CHUNK_CAP - chunk.len();
        let (take, done) = match available.iter().take(room).position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len().min(room), available.len() >= room),
        };
        chunk.extend_from_slice(&available[..take]);
        raw.consume(take);
        if done {
            return Ok(chunk.len());
        }
    }
}

/// Reads `raw` as a stream and returns its top-level calendar parts and
/// sender fields. Parts of attached messages are skipped: an invitation
/// counts only from the message's own sender. Memory stays within the
/// calendar and header caps, whatever the message size.
pub fn calendar_parts(mut raw: impl BufRead) -> io::Result<CalendarParts> {
    let mut out = CalendarParts::default();
    let mut boundaries: Vec<String> = Vec::new();
    let mut nested: Vec<usize> = Vec::new();
    let mut state = Calendar::Headers(Vec::new());
    let mut chunk = Vec::new();
    let mut line_start = true;
    let mut next_id = 0;
    let mut header_bytes = 0;
    let mut kept = 0;
    loop {
        chunk.clear();
        if read_chunk(&mut raw, &mut chunk)? == 0 {
            break;
        }
        let starts_line = line_start;
        line_start = chunk.ends_with(b"\n");
        let delimits = starts_line
            .then(|| delimiter(&chunk, &boundaries))
            .flatten();
        if let Some((i, closing)) = delimits {
            nested.retain(|depth| *depth <= i);
            if let Calendar::Body(collect) = std::mem::replace(&mut state, Calendar::Skip) {
                kept += collect.raw.len();
                out.parts.push(collect.finish(true));
            }
            boundaries.truncate(i + 1);
            if closing {
                boundaries.pop();
            } else {
                state = Calendar::Headers(Vec::new());
            }
            continue;
        }
        match &mut state {
            Calendar::Headers(headers) => {
                if headers.len() + chunk.len() <= HEADER_BLOCK_CAP
                    && header_bytes + chunk.len() <= HEADERS_CAP
                {
                    header_bytes += chunk.len();
                    headers.extend_from_slice(&chunk);
                }
                if !(starts_line && is_blank(&chunk)) {
                    continue;
                }
                let headers = std::mem::take(headers);
                let id = next_id;
                next_id += 1;
                if id == 0 {
                    out.headers = sender_fields(&headers);
                }
                let deep = boundaries.len() + nested.len() > MAX_DEPTH;
                state = match block(&headers, id, deep) {
                    Block::Multipart(boundary) => {
                        boundaries.push(boundary);
                        Calendar::Skip
                    }
                    Block::Message => {
                        nested.push(boundaries.len());
                        Calendar::Headers(Vec::new())
                    }
                    Block::Leaf(Some(collect)) if nested.is_empty() => {
                        if out.parts.len() < CALENDAR_PARTS_MAX {
                            Calendar::Body(collect)
                        } else {
                            out.omitted += 1;
                            Calendar::Skip
                        }
                    }
                    Block::Leaf(_) => Calendar::Skip,
                };
            }
            Calendar::Body(collect) => {
                let room = CALENDAR_PART_CAP.min(CALENDAR_TOTAL_CAP.saturating_sub(kept));
                collect.push(&chunk, room);
            }
            Calendar::Skip => {}
        }
    }
    if let Calendar::Body(mut collect) = state {
        if !boundaries.is_empty() {
            collect.cut();
        }
        out.parts.push(collect.finish(false));
    }
    Ok(out)
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
        assert!(elapsed.as_millis() < 5000, "{elapsed:?}");
    }

    #[test]
    fn deeply_nested_multiparts_render_in_linear_time() {
        let mut raw: String = (0..40_000)
            .map(|i| format!("Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n"))
            .collect();
        raw.push_str(&"--x\r\n".repeat(40_000));
        assert!(raw.len() > 2 << 20);
        let elapsed = render(raw.as_bytes());
        assert!(elapsed.as_millis() < 5000, "{elapsed:?}");
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

    const INVITE: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\nEND:VEVENT\r\nEND:VCALENDAR";

    fn invite_message(attachment: &str) -> String {
        format!(
            concat!(
                "Received: from mx\r\nAuthentication-Results: mx.example.net;\r\n",
                " dmarc=pass header.from=example.com\r\n",
                "From: Org <org@example.com>\r\nSubject: Meeting\r\nSender: s@example.com\r\n",
                "Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n",
                "--m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n",
                "--a\r\nContent-Type: text/plain\r\n\r\nYou are invited\r\n",
                "--a\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n",
                "{invite}\r\n",
                "--a--\r\n",
                "--m\r\nContent-Type: message/rfc822\r\n\r\n",
                "From: other@example.org\r\nContent-Type: multipart/mixed; boundary=\"n\"\r\n\r\n",
                "--n\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
                "--n--\r\n",
                "--m\r\nContent-Type: application/octet-stream; name=big.bin\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n{attachment}",
                "--m\r\nContent-Type: application/octet-stream; name=invite.ICS\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n",
                "--m--\r\n",
            ),
            invite = INVITE,
            attachment = attachment,
            encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, INVITE),
        )
    }

    #[test]
    fn calendar_parts_keep_top_level_invites_and_sender_fields() {
        let raw = invite_message(&base64_lines(1000));
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert_eq!(
            found.headers,
            b"Authentication-Results: mx.example.net;\r\n dmarc=pass header.from=example.com\r\n\
              From: Org <org@example.com>\r\nSender: s@example.com\r\n"
        );
        let message = parse(raw.as_bytes()).unwrap();
        let expected: Vec<(u32, String)> = message
            .root()
            .parts()
            .filter(|p| {
                crate::mime::is_calendar(
                    &crate::mime::content_type(p),
                    p.attachment_name().map(str::as_bytes),
                )
            })
            .map(|p| (p.id(), String::from_utf8(p.decoded().into_owned()).unwrap()))
            .collect();
        let got: Vec<(u32, String)> = found.parts.iter().map(|p| (p.id, p.text.clone())).collect();
        assert_eq!(got, expected);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(_, text)| text == INVITE));
        assert_eq!(found.parts[1].name.as_deref(), Some("invite.ICS"));
        assert!(!found.truncated());
    }

    #[test]
    fn an_invite_after_a_25_mb_attachment_is_found() {
        let raw = invite_message(&base64_lines(25 << 20));
        assert!(raw.len() > 25 << 20);
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert_eq!(found.parts.len(), 2);
        assert_eq!(found.parts[1].text, INVITE);
    }

    #[test]
    fn a_calendar_message_without_multipart_is_part_zero() {
        let raw = format!("From: a@example.com\r\nContent-Type: text/calendar\r\n\r\n{INVITE}\r\n");
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert_eq!(found.parts.len(), 1);
        assert_eq!(found.parts[0].id, 0);
        assert_eq!(found.parts[0].text, format!("{INVITE}\r\n"));
    }

    fn calendar_multipart(sizes: &[usize]) -> String {
        let mut raw = String::from("Content-Type: multipart/mixed; boundary=b\r\n\r\n");
        for size in sizes {
            raw.push_str("--b\r\nContent-Type: text/calendar\r\n\r\n");
            raw.push_str(&"x".repeat(*size));
            raw.push_str("\r\n");
        }
        raw.push_str("--b--\r\n");
        raw
    }

    #[test]
    fn a_part_over_its_cap_is_reported_too_large() {
        let raw = calendar_multipart(&[CALENDAR_PART_CAP + 1, 10]);
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert!(found.parts[0].too_large);
        assert!(found.parts[0].text.is_empty());
        assert!(!found.parts[1].too_large);
        assert!(found.truncated());
    }

    #[test]
    fn parts_past_the_total_cap_are_reported_too_large() {
        let size = CALENDAR_TOTAL_CAP / 3 + 1;
        let raw = calendar_multipart(&[size, size, size]);
        let found = calendar_parts(raw.as_bytes()).unwrap();
        let too_large: Vec<bool> = found.parts.iter().map(|p| p.too_large).collect();
        assert_eq!(too_large, [false, false, true]);
    }

    #[test]
    fn calendar_parts_past_the_maximum_are_counted() {
        let raw = calendar_multipart(&[10; CALENDAR_PARTS_MAX + 2]);
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert_eq!(found.parts.len(), CALENDAR_PARTS_MAX);
        assert_eq!(found.omitted, 2);
        assert!(found.truncated());
    }

    #[test]
    fn a_line_longer_than_a_chunk_does_not_end_the_part() {
        let long = format!("{}--b--", "x".repeat(CHUNK_CAP));
        let raw = calendar_multipart(&[0]).replace("\r\n--b--", &format!("{long}\r\n--b--"));
        let found = calendar_parts(raw.as_bytes()).unwrap();
        assert_eq!(found.parts.len(), 1);
        assert_eq!(found.parts[0].text, long);
    }

    #[test]
    fn a_part_cut_off_by_the_end_of_the_message_is_too_large() {
        let raw = calendar_multipart(&[10]);
        let cut = &raw[..raw.len() - "\r\n--b--\r\n".len()];
        let found = calendar_parts(cut.as_bytes()).unwrap();
        assert!(found.parts[0].too_large);
        let single = format!("Content-Type: text/calendar\r\n\r\n{INVITE}");
        let found = calendar_parts(single.as_bytes()).unwrap();
        assert!(!found.parts[0].too_large, "a single-part message just ends");
    }
}
