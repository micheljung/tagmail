//! Gmail-style search queries (plan §4 M2 "Search semantics").
//!
//! Juxtaposed terms are ANDed; `OR` (uppercase) binds tighter than the
//! implicit AND, so `a OR b c` is `(a OR b) AND c`. `-x` and `NOT x` negate,
//! parentheses group. Operator values and free text may be quoted with
//! `"…"`, where `\"` and `\\` escape. A trailing `*` on free text or a
//! `subject:` value asks for prefix matching. Unknown `name:value` words are
//! free text. [`Query`]'s `Display` is the canonical form and parses back to
//! the same tree.

use std::fmt;

use thiserror::Error;
use time::{Date, Month};

pub const MAX_QUERY_LEN: usize = 2048;
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// At least two children, none of them `And`.
    And(Vec<Query>),
    /// At least two children, none of them `Or`.
    Or(Vec<Query>),
    Not(Box<Query>),
    Term(Term),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Words anywhere in subject, addresses or body; several words must
    /// appear as a phrase.
    Text(Text),
    Subject(Text),
    /// Sender name or address words.
    From(String),
    /// To or Cc recipient name or address words.
    To(String),
    Cc(String),
    /// A label by name (compared by [`crate::LabelKey`]).
    Label(String),
    In(Location),
    Is(Status),
    HasAttachment,
    /// Strictly before the start of the day (UTC).
    Before(Date),
    /// On or after the start of the day (UTC).
    After(Date),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub value: String,
    /// The last word is a prefix.
    pub prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// `in:anywhere`: also Trash and Junk, which are excluded otherwise.
    Anywhere,
    /// A special-use name (`inbox`, `sent`, `trash`, …) or a folder path;
    /// never `anywhere`.
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unread,
    Read,
    Starred,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Unread => "unread",
            Status::Read => "read",
            Status::Starred => "starred",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryError {
    #[error("the query is empty")]
    Empty,
    #[error("the query is longer than {MAX_QUERY_LEN} bytes")]
    TooLong,
    #[error("parentheses and negations nest deeper than {MAX_DEPTH}")]
    TooDeep,
    #[error("unterminated quote at {0}")]
    UnterminatedQuote(usize),
    #[error("unbalanced parenthesis at {0}")]
    Unbalanced(usize),
    #[error("empty parentheses at {0}")]
    EmptyGroup(usize),
    #[error("OR at {0} needs a term on both sides")]
    DanglingOr(usize),
    #[error("negation at {0} needs a term")]
    DanglingNot(usize),
    #[error("{0}: needs a value")]
    MissingValue(String),
    #[error("is:{0} is not one of unread, read, starred")]
    UnknownStatus(String),
    #[error("has:{0} is not supported; use has:attachment")]
    UnknownHas(String),
    #[error("{op}:{value} is not a date like 2026/01/31")]
    BadDate { op: String, value: String },
}

pub fn parse(input: &str) -> Result<Query, QueryError> {
    if input.len() > MAX_QUERY_LEN {
        return Err(QueryError::TooLong);
    }
    let tokens = lex(input)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        end: input.len(),
    };
    let items = p.conjunction(0)?;
    if let Some(t) = p.tokens.get(p.pos) {
        return Err(QueryError::Unbalanced(t.at));
    }
    if items.is_empty() {
        return Err(QueryError::Empty);
    }
    Ok(and_of(items))
}

impl std::str::FromStr for Query {
    type Err = QueryError;

    fn from_str(s: &str) -> Result<Query, QueryError> {
        parse(s)
    }
}

/// `And` of `items`, flattening nested `And`s; a single item stands alone.
pub fn and_of(items: Vec<Query>) -> Query {
    flatten(items, |q| matches!(q, Query::And(_)), Query::And)
}

/// `Or` of `items`, flattening nested `Or`s; a single item stands alone.
pub fn or_of(items: Vec<Query>) -> Query {
    flatten(items, |q| matches!(q, Query::Or(_)), Query::Or)
}

fn flatten(
    items: Vec<Query>,
    same: impl Fn(&Query) -> bool,
    make: impl Fn(Vec<Query>) -> Query,
) -> Query {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if same(&item) {
            if let Query::And(children) | Query::Or(children) = item {
                out.extend(children);
            }
        } else {
            out.push(item);
        }
    }
    if out.len() == 1 {
        out.pop().expect("one item")
    } else {
        make(out)
    }
}

impl Query {
    /// Whether any `in:` term occurs; then Trash and Junk are not excluded
    /// by default.
    pub fn mentions_location(&self) -> bool {
        match self {
            Query::And(qs) | Query::Or(qs) => qs.iter().any(Query::mentions_location),
            Query::Not(q) => q.mentions_location(),
            Query::Term(t) => matches!(t, Term::In(_)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    Or,
    And,
    Not,
    Minus,
    Atom(Atom),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Atom {
    /// Lowercased known operator.
    op: Option<&'static str>,
    value: String,
    /// The value ended in `*` outside quotes.
    star: bool,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    at: usize,
}

const OPERATORS: [&str; 10] = [
    "subject", "from", "to", "cc", "label", "in", "is", "has", "before", "after",
];

fn ends_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '"')
}

fn lex(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(at, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let tok = match c {
            '(' => {
                chars.next();
                Tok::LParen
            }
            ')' => {
                chars.next();
                Tok::RParen
            }
            '-' if input[at + 1..]
                .chars()
                .next()
                .is_some_and(|n| !n.is_whitespace() && n != ')') =>
            {
                chars.next();
                Tok::Minus
            }
            '"' => {
                chars.next();
                let value = quoted(&mut chars, at)?;
                let star = chars.next_if(|&(_, c)| c == '*').is_some();
                Tok::Atom(Atom {
                    op: None,
                    value,
                    star,
                })
            }
            _ => {
                let mut word = String::new();
                while let Some((_, c)) = chars.next_if(|&(_, c)| !ends_word(c)) {
                    word.push(c);
                }
                match word.as_str() {
                    "OR" => Tok::Or,
                    "AND" => Tok::And,
                    "NOT" => Tok::Not,
                    _ => Tok::Atom(word_atom(word, &mut chars, at)?),
                }
            }
        };
        tokens.push(Token { tok, at });
    }
    Ok(tokens)
}

type Chars<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

/// The rest of a quoted string after its opening quote at `at`.
fn quoted(chars: &mut Chars<'_>, at: usize) -> Result<String, QueryError> {
    let mut value = String::new();
    loop {
        match chars.next() {
            None => return Err(QueryError::UnterminatedQuote(at)),
            Some((_, '"')) => return Ok(value),
            Some((_, '\\')) => match chars.next() {
                Some((_, c)) => value.push(c),
                None => return Err(QueryError::UnterminatedQuote(at)),
            },
            Some((_, c)) => value.push(c),
        }
    }
}

fn word_atom(word: String, chars: &mut Chars<'_>, at: usize) -> Result<Atom, QueryError> {
    let op = word.split_once(':').and_then(|(name, _)| {
        OPERATORS
            .iter()
            .copied()
            .find(|op| op.eq_ignore_ascii_case(name))
    });
    let Some(op) = op else {
        let (value, star) = strip_star(word);
        return Ok(Atom {
            op: None,
            value,
            star,
        });
    };
    let raw = word[op.len() + 1..].to_owned();
    if raw.is_empty() && chars.next_if(|&(_, c)| c == '"').is_some() {
        let value = quoted(chars, at + op.len() + 1)?;
        let star = chars.next_if(|&(_, c)| c == '*').is_some();
        return Ok(Atom {
            op: Some(op),
            value,
            star,
        });
    }
    let (value, star) = if op == "subject" {
        strip_star(raw)
    } else {
        (raw, false)
    };
    Ok(Atom {
        op: Some(op),
        value,
        star,
    })
}

fn strip_star(mut word: String) -> (String, bool) {
    if word.len() > 1 && word.ends_with('*') {
        word.pop();
        (word, true)
    } else {
        (word, false)
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn at(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |t| t.at)
    }

    /// Terms up to the end or a closing parenthesis (not consumed).
    fn conjunction(&mut self, depth: usize) -> Result<Vec<Query>, QueryError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::RParen) => return Ok(items),
                Some(Tok::And) => self.pos += 1,
                Some(_) => items.push(self.disjunction(depth)?),
            }
        }
    }

    fn disjunction(&mut self, depth: usize) -> Result<Query, QueryError> {
        let mut items = vec![self.unary(depth)?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            items.push(self.unary(depth)?);
        }
        Ok(or_of(items))
    }

    fn unary(&mut self, depth: usize) -> Result<Query, QueryError> {
        if depth >= MAX_DEPTH {
            return Err(QueryError::TooDeep);
        }
        let at = self.at();
        let Some(tok) = self.peek().cloned() else {
            return Err(QueryError::DanglingOr(at));
        };
        self.pos += 1;
        match tok {
            Tok::Minus | Tok::Not => {
                if matches!(self.peek(), None | Some(Tok::RParen | Tok::Or | Tok::And)) {
                    return Err(QueryError::DanglingNot(at));
                }
                Ok(Query::Not(Box::new(self.unary(depth + 1)?)))
            }
            Tok::LParen => {
                let items = self.conjunction(depth + 1)?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(QueryError::Unbalanced(at));
                }
                self.pos += 1;
                if items.is_empty() {
                    return Err(QueryError::EmptyGroup(at));
                }
                Ok(and_of(items))
            }
            Tok::RParen => Err(QueryError::Unbalanced(at)),
            Tok::Or | Tok::And => Err(QueryError::DanglingOr(at)),
            Tok::Atom(atom) => term(atom).map(Query::Term),
        }
    }
}

fn term(atom: Atom) -> Result<Term, QueryError> {
    let Atom { op, value, star } = atom;
    let Some(op) = op else {
        return Ok(Term::Text(Text {
            value,
            prefix: star,
        }));
    };
    if value.is_empty() {
        return Err(QueryError::MissingValue(op.to_owned()));
    }
    Ok(match op {
        "subject" => Term::Subject(Text {
            value,
            prefix: star,
        }),
        "from" => Term::From(value),
        "to" => Term::To(value),
        "cc" => Term::Cc(value),
        "label" => Term::Label(value),
        "in" if value.eq_ignore_ascii_case("anywhere") => Term::In(Location::Anywhere),
        "in" => Term::In(Location::Named(value)),
        "is" => Term::Is(
            [Status::Unread, Status::Read, Status::Starred]
                .into_iter()
                .find(|s| s.as_str().eq_ignore_ascii_case(&value))
                .ok_or(QueryError::UnknownStatus(value))?,
        ),
        "has" if value.eq_ignore_ascii_case("attachment") => Term::HasAttachment,
        "has" => return Err(QueryError::UnknownHas(value)),
        "before" | "after" => {
            let date = parse_date(&value).ok_or_else(|| QueryError::BadDate {
                op: op.to_owned(),
                value,
            })?;
            if op == "before" {
                Term::Before(date)
            } else {
                Term::After(date)
            }
        }
        _ => unreachable!("every OPERATORS entry is handled"),
    })
}

/// `YYYY/M/D` or `YYYY-M-D`.
fn parse_date(s: &str) -> Option<Date> {
    let sep = if s.contains('/') { '/' } else { '-' };
    let mut parts = s.split(sep);
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |p: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
    };
    if parts.next().is_some() || !digits(y, 4..=4) || !digits(m, 1..=2) || !digits(d, 1..=2) {
        return None;
    }
    let month = Month::try_from(m.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(y.parse().ok()?, month, d.parse().ok()?).ok()
}

/// Start of the day in UTC as Unix seconds.
pub fn day_start(date: Date) -> i64 {
    date.midnight().assume_utc().unix_timestamp()
}

fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            f.write_str("\\")?;
        }
        write!(f, "{c}")?;
    }
    f.write_str("\"")
}

/// Whether an operator value can be written without quotes.
fn bare_value(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(ends_word) && !s.contains('\\')
}

/// Whether free text can be written as a bare word.
fn bare_text(s: &str) -> bool {
    bare_value(s)
        && !s.starts_with('-')
        && !s.ends_with('*')
        && !s.contains(':')
        && !matches!(s, "OR" | "AND" | "NOT")
}

fn write_text(f: &mut fmt::Formatter<'_>, t: &Text) -> fmt::Result {
    if bare_text(&t.value) {
        f.write_str(&t.value)?;
    } else {
        write_quoted(f, &t.value)?;
    }
    if t.prefix {
        f.write_str("*")?;
    }
    Ok(())
}

fn write_value(f: &mut fmt::Formatter<'_>, op: &str, value: &str) -> fmt::Result {
    write!(f, "{op}:")?;
    if bare_value(value) {
        f.write_str(value)
    } else {
        write_quoted(f, value)
    }
}

fn write_date(f: &mut fmt::Formatter<'_>, op: &str, d: Date) -> fmt::Result {
    write!(
        f,
        "{op}:{:04}/{:02}/{:02}",
        d.year(),
        u8::from(d.month()),
        d.day()
    )
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Term::Text(t) => write_text(f, t),
            Term::Subject(t) => {
                f.write_str("subject:")?;
                write_text(f, t)
            }
            Term::From(v) => write_value(f, "from", v),
            Term::To(v) => write_value(f, "to", v),
            Term::Cc(v) => write_value(f, "cc", v),
            Term::Label(v) => write_value(f, "label", v),
            Term::In(Location::Anywhere) => f.write_str("in:anywhere"),
            Term::In(Location::Named(v)) => write_value(f, "in", v),
            Term::Is(s) => write!(f, "is:{}", s.as_str()),
            Term::HasAttachment => f.write_str("has:attachment"),
            Term::Before(d) => write_date(f, "before", *d),
            Term::After(d) => write_date(f, "after", *d),
        }
    }
}

fn write_child(f: &mut fmt::Formatter<'_>, q: &Query, group: bool) -> fmt::Result {
    if group {
        write!(f, "({q})")
    } else {
        write!(f, "{q}")
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Query::And(qs) => {
                for (i, q) in qs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write_child(f, q, matches!(q, Query::And(_)))?;
                }
                Ok(())
            }
            Query::Or(qs) => {
                for (i, q) in qs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" OR ")?;
                    }
                    write_child(f, q, matches!(q, Query::And(_) | Query::Or(_)))?;
                }
                Ok(())
            }
            Query::Not(q) => {
                f.write_str("-")?;
                write_child(f, q, matches!(**q, Query::And(_) | Query::Or(_)))
            }
            Query::Term(t) => write!(f, "{t}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn text(v: &str) -> Query {
        Query::Term(Term::Text(Text {
            value: v.to_owned(),
            prefix: false,
        }))
    }

    fn not(q: Query) -> Query {
        Query::Not(Box::new(q))
    }

    fn date(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
    }

    fn p(s: &str) -> Query {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn each_operator() {
        let t = |s: &str| match p(s) {
            Query::Term(t) => t,
            q => panic!("{s}: {q:?}"),
        };
        assert_eq!(
            t("subject:invoice"),
            Term::Subject(Text {
                value: "invoice".into(),
                prefix: false
            })
        );
        assert_eq!(
            t("subject:\"weekly rep\"*"),
            Term::Subject(Text {
                value: "weekly rep".into(),
                prefix: true
            })
        );
        assert_eq!(t("from:x@y.com"), Term::From("x@y.com".into()));
        assert_eq!(t("From:\"Alice Smith\""), Term::From("Alice Smith".into()));
        assert_eq!(t("to:bob"), Term::To("bob".into()));
        assert_eq!(t("cc:carol@z"), Term::Cc("carol@z".into()));
        assert_eq!(t("label:\"Work Stuff\""), Term::Label("Work Stuff".into()));
        assert_eq!(t("in:inbox"), Term::In(Location::Named("inbox".into())));
        assert_eq!(
            t("in:Projects/2026"),
            Term::In(Location::Named("Projects/2026".into()))
        );
        assert_eq!(t("in:Anywhere"), Term::In(Location::Anywhere));
        assert_eq!(t("is:unread"), Term::Is(Status::Unread));
        assert_eq!(t("is:READ"), Term::Is(Status::Read));
        assert_eq!(t("is:starred"), Term::Is(Status::Starred));
        assert_eq!(t("has:attachment"), Term::HasAttachment);
        assert_eq!(t("before:2026/01/31"), Term::Before(date(2026, 1, 31)));
        assert_eq!(t("after:2026-2-3"), Term::After(date(2026, 2, 3)));
        assert_eq!(
            t("invo*"),
            Term::Text(Text {
                value: "invo".into(),
                prefix: true
            })
        );
        assert_eq!(
            t("\"quick brown\""),
            Term::Text(Text {
                value: "quick brown".into(),
                prefix: false
            })
        );
        assert_eq!(
            t("https://x.y/z"),
            Term::Text(Text {
                value: "https://x.y/z".into(),
                prefix: false
            })
        );
    }

    #[test]
    fn precedence() {
        assert_eq!(
            p("a OR b c"),
            Query::And(vec![Query::Or(vec![text("a"), text("b")]), text("c")])
        );
        assert_eq!(
            p("a b OR c"),
            Query::And(vec![text("a"), Query::Or(vec![text("b"), text("c")])])
        );
        assert_eq!(p("-a OR b"), Query::Or(vec![not(text("a")), text("b")]));
        assert_eq!(p("NOT a b"), Query::And(vec![not(text("a")), text("b")]));
        assert_eq!(
            p("-(a b) c"),
            Query::And(vec![not(Query::And(vec![text("a"), text("b")])), text("c")])
        );
        assert_eq!(
            p("a (b OR c) AND d"),
            Query::And(vec![
                text("a"),
                Query::Or(vec![text("b"), text("c")]),
                text("d")
            ])
        );
        assert_eq!(
            p("(a OR b) OR c"),
            Query::Or(vec![text("a"), text("b"), text("c")])
        );
        assert_eq!(p("((a))"), text("a"));
        assert_eq!(
            p("a or b"),
            Query::And(vec![text("a"), text("or"), text("b")])
        );
    }

    #[test]
    fn negation_and_literal_dashes() {
        assert_eq!(p("--a"), not(not(text("a"))));
        assert_eq!(
            p("a - b"),
            Query::And(vec![text("a"), text("-"), text("b")])
        );
        assert_eq!(p("e-mail"), text("e-mail"));
        assert_eq!(p("-is:unread"), not(Query::Term(Term::Is(Status::Unread))));
    }

    #[test]
    fn escaping() {
        assert_eq!(p(r#""say \"hi\" \\ bye""#), text(r#"say "hi" \ bye"#));
        let q = Query::Term(Term::From(r#"a"b\c"#.into()));
        assert_eq!(q.to_string(), r#"from:"a\"b\\c""#);
        assert_eq!(text("OR").to_string(), "\"OR\"");
        assert_eq!(text("-x").to_string(), "\"-x\"");
        assert_eq!(text("a:b").to_string(), "\"a:b\"");
        assert_eq!(text("*").to_string(), "\"*\"");
        assert_eq!(p("*"), text("*"));
    }

    #[test]
    fn errors() {
        assert_eq!(parse(""), Err(QueryError::Empty));
        assert_eq!(parse("  AND "), Err(QueryError::Empty));
        assert_eq!(parse("\"abc"), Err(QueryError::UnterminatedQuote(0)));
        assert_eq!(
            parse("subject:\"abc"),
            Err(QueryError::UnterminatedQuote(8))
        );
        assert_eq!(parse("(a"), Err(QueryError::Unbalanced(0)));
        assert_eq!(parse("a)"), Err(QueryError::Unbalanced(1)));
        assert_eq!(parse("()"), Err(QueryError::EmptyGroup(0)));
        assert_eq!(parse("a OR"), Err(QueryError::DanglingOr(4)));
        assert_eq!(parse("OR a"), Err(QueryError::DanglingOr(0)));
        assert_eq!(parse("NOT"), Err(QueryError::DanglingNot(0)));
        assert_eq!(parse("from:"), Err(QueryError::MissingValue("from".into())));
        assert_eq!(
            parse("label:\"\""),
            Err(QueryError::MissingValue("label".into()))
        );
        assert_eq!(
            parse("is:big"),
            Err(QueryError::UnknownStatus("big".into()))
        );
        assert_eq!(parse("has:pdf"), Err(QueryError::UnknownHas("pdf".into())));
        for bad in [
            "2026/13/01",
            "2026/02/30",
            "26/01/01",
            "2026/1",
            "yesterday",
            "7d",
        ] {
            assert!(
                matches!(
                    parse(&format!("after:{bad}")),
                    Err(QueryError::BadDate { .. })
                ),
                "{bad}"
            );
        }
        assert_eq!(parse(&"(".repeat(40)), Err(QueryError::TooDeep));
        assert_eq!(parse(&"a ".repeat(MAX_QUERY_LEN)), Err(QueryError::TooLong));
    }

    #[test]
    fn day_start_is_utc_midnight() {
        assert_eq!(day_start(date(2026, 1, 1)), 1_767_225_600);
    }

    #[test]
    fn mentions_location() {
        assert!(!p("a -b").mentions_location());
        assert!(p("a OR -in:trash").mentions_location());
    }

    fn arb_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9äß@.:*()\"\\\\ -]{0,8}"
    }

    fn arb_text() -> impl Strategy<Value = Text> {
        (arb_string(), any::<bool>()).prop_map(|(value, prefix)| Text { value, prefix })
    }

    fn arb_value() -> impl Strategy<Value = String> {
        arb_string().prop_filter("non-empty", |s| !s.is_empty())
    }

    fn arb_term() -> impl Strategy<Value = Term> {
        let date = (1970i32..2100, 1u8..=12, 1u8..=28).prop_map(|(y, m, d)| date(y, m, d));
        prop_oneof![
            arb_text().prop_map(Term::Text),
            (arb_value(), any::<bool>())
                .prop_map(|(value, prefix)| Term::Subject(Text { value, prefix })),
            arb_value().prop_map(Term::From),
            arb_value().prop_map(Term::To),
            arb_value().prop_map(Term::Cc),
            arb_value().prop_map(Term::Label),
            Just(Term::In(Location::Anywhere)),
            arb_value()
                .prop_filter("not anywhere", |v| !v.eq_ignore_ascii_case("anywhere"))
                .prop_map(|v| Term::In(Location::Named(v))),
            prop_oneof![
                Just(Status::Unread),
                Just(Status::Read),
                Just(Status::Starred)
            ]
            .prop_map(Term::Is),
            Just(Term::HasAttachment),
            date.clone().prop_map(Term::Before),
            date.prop_map(Term::After),
        ]
    }

    fn arb_query() -> impl Strategy<Value = Query> {
        arb_term()
            .prop_map(Query::Term)
            .prop_recursive(4, 24, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 2..4).prop_map(and_of),
                    prop::collection::vec(inner.clone(), 2..4).prop_map(or_of),
                    inner.prop_map(|q| Query::Not(Box::new(q))),
                ]
            })
    }

    proptest! {
        #[test]
        fn display_round_trips(q in arb_query()) {
            let shown = q.to_string();
            prop_assert_eq!(parse(&shown), Ok(q), "{}", shown);
        }

        #[test]
        fn never_panics(s in "\\PC{0,64}") {
            let _ = parse(&s);
        }

        #[test]
        fn canonical_form_is_a_fixed_point(s in "[a-z :()\"*-]{0,24}|[A-Z]{2,3} [a-z]") {
            if let Ok(q) = parse(&s) {
                let shown = q.to_string();
                prop_assert_eq!(parse(&shown).map(|q| q.to_string()), Ok(shown));
            }
        }
    }
}
