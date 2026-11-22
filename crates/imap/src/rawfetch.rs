//! Minimal parser for FETCH lines that imap-codec rejects.
//!
//! imap-codec fails the whole response on one unknown attribute
//! (`X-GM-MSGID`, `EMAILID`, `PREVIEW`) or on server quirks such as
//! Exchange's malformed BODYSTRUCTURE. imap-next then hands back the raw
//! bytes, and this parser recovers what the engine needs: UID, FLAGS,
//! MODSEQ, RFC822.SIZE, the object ids and `BODY[...]` literals.
//! Anything else is skipped generically. Upstream support in imap-codec
//! would replace this module.

use crate::types::{Flag, MessageMeta, ModSeq, Uid};

#[derive(Debug, PartialEq, Eq)]
enum Value<'a> {
    Nil,
    Number(u64),
    Atom(&'a [u8]),
    String(Vec<u8>),
    List(Vec<Value<'a>>),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

const MAX_DEPTH: usize = 32;

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.i += 1;
        }
    }

    fn eat(&mut self, s: &[u8]) -> bool {
        let end = self.i + s.len();
        if end <= self.b.len() && self.b[self.i..end].eq_ignore_ascii_case(s) {
            self.i = end;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Option<u64> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    fn atom(&mut self) -> &'a [u8] {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| !matches!(c, b' ' | b'(' | b')' | b'\r' | b'\n' | b'"' | b'{'))
        {
            self.i += 1;
        }
        &self.b[start..self.i]
    }

    fn quoted(&mut self) -> Option<Vec<u8>> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    out.push(*self.b.get(self.i + 1)?);
                    self.i += 2;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn literal(&mut self) -> Option<Vec<u8>> {
        self.i += 1;
        let len = usize::try_from(self.number()?).ok()?;
        self.eat(b"+");
        if !self.eat(b"}") {
            return None;
        }
        self.eat(b"\r");
        if !self.eat(b"\n") {
            return None;
        }
        let end = self.i.checked_add(len)?;
        let data = self.b.get(self.i..end)?.to_vec();
        self.i = end;
        Some(data)
    }

    fn value(&mut self, depth: usize) -> Option<Value<'a>> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_spaces();
        match self.peek()? {
            b'(' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.peek()? == b')' {
                        self.i += 1;
                        return Some(Value::List(items));
                    }
                    items.push(self.value(depth + 1)?);
                }
            }
            b'"' => self.quoted().map(Value::String),
            b'{' => self.literal().map(Value::String),
            c if c.is_ascii_digit() => {
                let start = self.i;
                let n = self.number();
                // Atoms may start with digits, e.g. a `1foo` keyword.
                if self.peek().is_some_and(|c| !matches!(c, b' ' | b')')) {
                    self.i = start;
                    return Some(Value::Atom(self.atom()));
                }
                n.map(Value::Number)
            }
            _ => {
                let atom = self.atom();
                if atom.is_empty() {
                    return None;
                }
                if atom.eq_ignore_ascii_case(b"NIL") {
                    Some(Value::Nil)
                } else {
                    Some(Value::Atom(atom))
                }
            }
        }
    }

    /// An attribute name with an optional `[section]` and `<origin>`.
    fn item_name(&mut self) -> Option<(&'a [u8], Option<&'a [u8]>)> {
        self.skip_spaces();
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| !matches!(c, b' ' | b'(' | b')' | b'[' | b'\r' | b'\n'))
        {
            self.i += 1;
        }
        let name = &self.b[start..self.i];
        if name.is_empty() {
            return None;
        }
        let mut section = None;
        if self.peek() == Some(b'[') {
            self.i += 1;
            let s = self.i;
            let mut depth = 0usize;
            loop {
                match self.peek()? {
                    b'(' => depth += 1,
                    b')' => depth = depth.checked_sub(1)?,
                    b']' if depth == 0 => break,
                    b'"' => {
                        self.quoted()?;
                        continue;
                    }
                    _ => {}
                }
                self.i += 1;
            }
            section = Some(&self.b[s..self.i]);
            self.i += 1;
            if self.peek() == Some(b'<') {
                while self.peek()? != b'>' {
                    self.i += 1;
                }
                self.i += 1;
            }
        }
        Some((name, section))
    }
}

fn as_bytes(v: Value<'_>) -> Option<Vec<u8>> {
    match v {
        Value::String(s) => Some(s),
        Value::Atom(a) => Some(a.to_vec()),
        Value::Number(n) => Some(n.to_string().into_bytes()),
        Value::Nil | Value::List(_) => None,
    }
}

/// An RFC 8474 object id, `(id)` or `NIL`.
fn object_id(v: Value<'_>) -> Option<String> {
    let inner = match v {
        Value::List(mut items) if items.len() == 1 => items.pop()?,
        other => other,
    };
    String::from_utf8(as_bytes(inner)?).ok()
}

/// Parses `* <seq> FETCH (...)`; `None` if `bytes` is not a FETCH response
/// or is too broken to recover the attribute list.
pub(crate) fn parse_fetch(bytes: &[u8]) -> Option<MessageMeta> {
    let mut p = Parser { b: bytes, i: 0 };
    if !p.eat(b"* ") {
        return None;
    }
    let seq = u32::try_from(p.number()?).ok()?;
    if !p.eat(b" FETCH ") {
        return None;
    }
    p.skip_spaces();
    if !p.eat(b"(") {
        return None;
    }
    let mut meta = MessageMeta {
        seq,
        ..MessageMeta::default()
    };
    loop {
        p.skip_spaces();
        if p.eat(b")") {
            return Some(meta);
        }
        let (name, section) = p.item_name()?;
        let value = p.value(0)?;
        let name = name.to_ascii_uppercase();
        match (name.as_slice(), value) {
            (b"UID", Value::Number(n)) => meta.uid = u32::try_from(n).ok().map(Uid),
            (b"FLAGS", Value::List(items)) => {
                meta.flags = Some(
                    items
                        .into_iter()
                        .filter_map(as_bytes)
                        .filter_map(|f| String::from_utf8(f).ok())
                        .filter_map(|f| Flag::parse(&f))
                        .collect(),
                );
            }
            (b"MODSEQ", Value::List(items)) => {
                if let [Value::Number(n)] = items.as_slice() {
                    meta.modseq = Some(ModSeq(*n));
                }
            }
            (b"RFC822.SIZE", Value::Number(n)) => meta.size = u32::try_from(n).ok(),
            (b"X-GM-MSGID", Value::Number(n)) => meta.gmail_msgid = Some(n),
            (b"X-GM-THRID", Value::Number(n)) => meta.gmail_thrid = Some(n),
            (b"EMAILID", v) => meta.email_id = object_id(v),
            (b"THREADID", v) => meta.thread_id = object_id(v),
            (b"BODY", v) => {
                let section = section.unwrap_or_default().to_ascii_uppercase();
                if section.starts_with(b"HEADER.FIELDS") {
                    meta.header_fields = as_bytes(v);
                } else if section == b"1" {
                    meta.snippet = as_bytes(v);
                }
            }
            _ => {}
        }
    }
}

/// The tag and status of a tagged response imap-codec could not parse.
pub(crate) fn parse_tagged_status<'a>(bytes: &'a [u8], tag: &str) -> Option<&'a [u8]> {
    let rest = bytes.strip_prefix(tag.as_bytes())?.strip_prefix(b" ")?;
    let end = rest.iter().position(|&c| c == b' ' || c == b'\r')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_attributes_with_flags() {
        let m = parse_fetch(
            b"* 1 FETCH (X-GM-THRID 1278455344230334865 X-GM-MSGID 1278455344230334866 \
              X-GM-LABELS (\\Inbox \\Sent Important \"Muy Importante\") UID 4 FLAGS (\\Seen $label_Work))\r\n",
        )
        .unwrap();
        assert_eq!(m.seq, 1);
        assert_eq!(m.uid, Some(Uid(4)));
        assert_eq!(m.gmail_msgid, Some(1_278_455_344_230_334_866));
        assert_eq!(m.gmail_thrid, Some(1_278_455_344_230_334_865));
        assert_eq!(
            m.flags,
            Some(vec![Flag::Seen, Flag::Keyword("$label_Work".into())])
        );
    }

    #[test]
    fn objectid_attributes_and_modseq() {
        let m = parse_fetch(
            b"* 7 FETCH (UID 9 MODSEQ (12345) EMAILID (M6d99ac3275bb4e) THREADID (T64b478a75b7ea9))\r\n",
        )
        .unwrap();
        assert_eq!(m.uid, Some(Uid(9)));
        assert_eq!(m.modseq, Some(ModSeq(12345)));
        assert_eq!(m.email_id.as_deref(), Some("M6d99ac3275bb4e"));
        assert_eq!(m.thread_id.as_deref(), Some("T64b478a75b7ea9"));
        let nil = parse_fetch(b"* 7 FETCH (UID 9 THREADID NIL)\r\n").unwrap();
        assert_eq!(nil.thread_id, None);
    }

    #[test]
    fn literals_sections_and_unknown_items() {
        let m = parse_fetch(
            b"* 3 FETCH (UID 5 PREVIEW \"hello (world)\" SAVEDATE \"22-Nov-2026 10:00:00 +0000\" \
              BODY[HEADER.FIELDS (REFERENCES IN-REPLY-TO)] {14}\r\nReferences: x\n BODY[1]<0> {5}\r\nhe)lo \
              RFC822.SIZE 1234 BINARY.SIZE[1] 100 X-UNKNOWN ((a b) \"c\" NIL))\r\n",
        )
        .unwrap();
        assert_eq!(m.uid, Some(Uid(5)));
        assert_eq!(m.header_fields.as_deref(), Some(&b"References: x\n"[..]));
        assert_eq!(m.snippet.as_deref(), Some(&b"he)lo"[..]));
        assert_eq!(m.size, Some(1234));
    }

    #[test]
    fn exchange_and_yahoo_quirks_recover_uid_and_flags() {
        for line in [
            &b"* 1 FETCH (UID 3 FLAGS (\\Seen) BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"us-ascii\") NIL NIL NIL 100 5 NIL NIL NIL NIL))\r\n"[..],
            b"* 1 FETCH (UID 3 FLAGS (\\Seen) RFC822.SIZE -1)\r\n",
            b"* 1 FETCH (UID 3 FLAGS (\\Seen [Gmail]Foo \\XFoo NonJunk) INTERNALDATE \"31-Feb-2026 10:00:00 +0000\")\r\n",
            b"* 1 FETCH (UID 3 FLAGS (\\Seen) BODYSTRUCTURE (\"message\" \"rfc822\" NIL NIL NIL \"7bit\" 300 NIL NIL NIL NIL))\n",
        ] {
            let m = parse_fetch(line).unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(line)));
            assert_eq!(m.uid, Some(Uid(3)));
            assert!(m.flags.as_ref().unwrap().contains(&Flag::Seen));
        }
    }

    #[test]
    fn rejects_non_fetch_and_truncated_input() {
        assert!(parse_fetch(b"* ESEARCH (TAG \"A5\") UID ALL 1:3\r\n").is_none());
        assert!(parse_fetch(b"* 1 FETCH (UID 1 BODY[] {10}\r\nabc").is_none());
        assert!(parse_fetch(b"* 1 FETCH (UID 1 FLAGS (\\Seen)").is_none());
        let deep = format!("* 1 FETCH (X {}{})\r\n", "(".repeat(100), ")".repeat(100));
        assert!(parse_fetch(deep.as_bytes()).is_none());
    }

    #[test]
    fn tagged_status_fallback() {
        assert_eq!(
            parse_tagged_status(b"T4 OK [WEIRD (x] done\r\n", "T4"),
            Some(&b"OK"[..])
        );
        assert_eq!(parse_tagged_status(b"T44 OK done\r\n", "T4"), None);
    }
}
