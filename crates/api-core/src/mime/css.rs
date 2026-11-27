//! Author `<style>` sheets for the render document (B1, behind the
//! `--render-author-css` flag): the sheets of a message body are collected
//! by an html5ever pre-pass that builds the same tree ammonia does, parsed
//! with cssparser, and re-serialized from an allowlist. Source text is never
//! passed through: every kept token is written out again.
//!
//! What survives: style rules whose selectors use type, class, id,
//! universal and attribute selectors, combinators, `:first-child`,
//! `:last-child`, `:nth-child()`, `:not()` with simple arguments and
//! `::before`/`::after`; declarations of [`STYLE_PROPERTIES`] and
//! [`EXTRA_PROPERTIES`] whose values hold no function but
//! [`FUNCTIONS`]; `@media` rules on width and orientation, at most
//! [`MAX_MEDIA_DEPTH`] deep. Nothing that loads a resource can be written.
//!
//! The sheets of all HTML parts of a message are concatenated with the
//! parts (`mime::body`), so author CSS from one part applies to all of them.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;

use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser, ToCss, Token,
    parse_nth, serialize_identifier, serialize_string,
};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ParseOpts, QualName, local_name, ns, parse_fragment};

use super::sanitize::{CSS_FORBIDDEN, DROP_WITH_CONTENT, STYLE_PROPERTIES};

/// Combined size of a message's author sheets above which all are dropped.
pub const MAX_SHEET_BYTES: usize = 128 * 1024;
/// Rules (style rules and at-rules, kept or not) above which all sheets
/// are dropped.
pub const MAX_RULES: usize = 2000;
/// `@media` inside `@media` counts; deeper rules are dropped.
pub const MAX_MEDIA_DEPTH: usize = 2;
/// Nested functions and parentheses inside one value, e.g. `calc((1px))`.
const MAX_VALUE_NESTING: usize = 4;

/// Allowed in author sheets on top of [`STYLE_PROPERTIES`].
pub const EXTRA_PROPERTIES: &[&str] = &[
    "box-sizing",
    "clear",
    "float",
    "overflow",
    "overflow-x",
    "overflow-y",
    "visibility",
];

/// The only functions a value may call; none of them loads anything.
pub const FUNCTIONS: &[&str] = &["calc", "clamp", "hsl", "hsla", "max", "min", "rgb", "rgba"];

const MEDIA_KEYWORDS: &[&str] = &["all", "and", "only", "screen"];
const MEDIA_FEATURES: &[&str] = &[
    "device-width",
    "max-device-width",
    "max-width",
    "min-device-width",
    "min-width",
    "orientation",
    "width",
];
const MEDIA_UNITS: &[&str] = &["em", "px", "rem"];
const PSEUDO_CLASSES: &[&str] = &["first-child", "last-child"];
const PSEUDO_ELEMENTS: &[&str] = &["after", "before"];

type Error = ParseError<()>;

fn fail<T>() -> Result<T, Error> {
    Err(ParseError::unexpected_token())
}

fn allowed_property(name: &str) -> bool {
    STYLE_PROPERTIES.contains(&name) || EXTRA_PROPERTIES.contains(&name)
}

/// Strings may not hold anything that could end the `<style>` element or
/// a line.
fn safe_string(s: &str) -> bool {
    !s.contains(['<', '>', '\n', '\r', '\u{c}'])
}

/// Escapes can spell anything, so a prelude or declaration with a
/// backslash anywhere in its source is dropped.
fn escaped(input: &Parser<'_>, start: &ParserState) -> bool {
    input.slice_from(start.position()).contains('\\')
}

fn ident_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_') || !c.is_ascii())
}

/// The author sheets of `html` (a full document or a fragment), sanitized
/// and joined; `None` when nothing survives.
pub fn author_css(html: &str) -> Option<String> {
    if !contains_ignore_case(html.as_bytes(), b"<style") {
        return None;
    }
    sanitize_sheets(&collect_styles(html))
}

/// One sheet, sanitized; `None` when nothing survives.
pub fn sanitize_stylesheet(css: &str) -> Option<String> {
    sanitize_sheets(&[css])
}

fn sanitize_sheets<S: AsRef<str>>(sheets: &[S]) -> Option<String> {
    let total: usize = sheets.iter().map(|s| s.as_ref().len()).sum();
    if total > MAX_SHEET_BYTES {
        return None;
    }
    let rules = Cell::new(0);
    let mut out = String::new();
    for sheet in sheets {
        let mut parser = Parser::new(sheet.as_ref());
        rule_list(&mut parser, &rules, 0, &mut out);
        if rules.get() > MAX_RULES {
            return None;
        }
    }
    // Backstop: the allowlist cannot write these, so seeing one means a bug.
    if out.is_empty() || out.contains('<') || out.contains("<!--") || out.contains("-->") {
        return None;
    }
    Some(out)
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn rule_list(input: &mut Parser<'_>, rules: &Cell<usize>, depth: usize, out: &mut String) {
    let mut parser = Rules { rules, depth };
    for rule in StyleSheetParser::new(input, &mut parser) {
        if let Ok(rule) = rule {
            out.push_str(&rule);
        }
        if rules.get() > MAX_RULES {
            return;
        }
    }
}

struct Rules<'c> {
    rules: &'c Cell<usize>,
    depth: usize,
}

impl Rules<'_> {
    fn count(&self) -> Result<(), Error> {
        self.rules.set(self.rules.get() + 1);
        if self.rules.get() > MAX_RULES {
            return fail();
        }
        Ok(())
    }
}

impl<'i> QualifiedRuleParser<'i> for Rules<'_> {
    type Prelude = String;
    type QualifiedRule = String;
    type Error = ();

    fn parse_prelude(&mut self, input: &mut Parser<'i>) -> Result<String, Error> {
        self.count()?;
        let start = input.state();
        let selectors = selector_list(input)?;
        if escaped(input, &start) {
            return fail();
        }
        Ok(selectors)
    }

    fn parse_block(
        &mut self,
        selectors: String,
        _start: &ParserState,
        input: &mut Parser<'i>,
    ) -> Result<String, Error> {
        let mut parser = Declarations;
        let declarations: Vec<String> = RuleBodyParser::new(input, &mut parser)
            .filter_map(Result::ok)
            .collect();
        if declarations.is_empty() {
            return Ok(String::new());
        }
        Ok(format!("{selectors}{{{}}}", declarations.join(";")))
    }
}

impl<'i> AtRuleParser<'i> for Rules<'_> {
    type Prelude = String;
    type AtRule = String;
    type Error = ();

    fn parse_prelude(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i>,
    ) -> Result<String, Error> {
        self.count()?;
        if !name.eq_ignore_ascii_case("media") || self.depth >= MAX_MEDIA_DEPTH {
            return fail();
        }
        let start = input.state();
        let queries = media_queries(input)?;
        if escaped(input, &start) {
            return fail();
        }
        Ok(queries)
    }

    fn parse_block(
        &mut self,
        queries: String,
        _start: &ParserState,
        input: &mut Parser<'i>,
    ) -> Result<String, Error> {
        let mut inner = String::new();
        rule_list(input, self.rules, self.depth + 1, &mut inner);
        if inner.is_empty() {
            return Ok(String::new());
        }
        Ok(format!("@media {queries}{{{inner}}}"))
    }
}

/// `only screen and (max-width:600px), (orientation:portrait)`.
fn media_queries(input: &mut Parser<'_>) -> Result<String, Error> {
    let mut out = String::new();
    // Expecting a keyword or feature (start, after `,`, `and` or `only`).
    let mut need = true;
    let mut space = false;
    while let Ok(token) = input.next_including_whitespace_and_comments() {
        match token.clone() {
            Token::WhiteSpace(_) | Token::Comment(_) => space = true,
            Token::Comma if !need => {
                out.push(',');
                need = true;
                space = false;
            }
            Token::Ident(word) => {
                let word = word.to_ascii_lowercase();
                if !MEDIA_KEYWORDS.contains(&word.as_str()) {
                    return fail();
                }
                if space && !out.is_empty() && !out.ends_with(',') {
                    out.push(' ');
                }
                out.push_str(&word);
                need = word == "and" || word == "only";
                space = false;
            }
            Token::ParenthesisBlock => {
                if space && !out.is_empty() && !out.ends_with(',') {
                    out.push(' ');
                }
                out.push('(');
                input.parse_nested_block(|i| media_feature(i, &mut out))?;
                out.push(')');
                need = false;
                space = false;
            }
            _ => return fail(),
        }
    }
    if need { fail() } else { Ok(out) }
}

/// `max-width:600px`, `orientation:portrait`, or a bare feature name.
fn media_feature(input: &mut Parser<'_>, out: &mut String) -> Result<(), Error> {
    let feature = input.expect_ident()?.to_ascii_lowercase();
    if !MEDIA_FEATURES.contains(&feature.as_str()) {
        return fail();
    }
    out.push_str(&feature);
    if input.is_exhausted() {
        return Ok(());
    }
    input.expect_colon()?;
    out.push(':');
    match input.next()?.clone() {
        Token::Ident(v) if feature == "orientation" => {
            let v = v.to_ascii_lowercase();
            if v != "portrait" && v != "landscape" {
                return fail();
            }
            out.push_str(&v);
        }
        Token::Dimension { value, unit, .. }
            if feature != "orientation"
                && MEDIA_UNITS.contains(&unit.to_ascii_lowercase().as_str()) =>
        {
            write_number(out, value);
            out.push_str(&unit.to_ascii_lowercase());
        }
        Token::Number { value, .. } if feature != "orientation" && value == 0.0 => out.push('0'),
        _ => return fail(),
    }
    input.expect_exhausted()?;
    Ok(())
}

fn write_number(out: &mut String, value: f32) {
    let _ = write!(out, "{value}");
}

/// A selector list, written with normalized whitespace.
fn selector_list(input: &mut Parser<'_>) -> Result<String, Error> {
    let mut out = String::new();
    let mut need = true;
    let mut space = false;
    while let Ok(token) = input.next_including_whitespace_and_comments() {
        let token = token.clone();
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => {
                space = true;
                continue;
            }
            Token::Comma => {
                if need {
                    return fail();
                }
                out.push(',');
                need = true;
                space = false;
                continue;
            }
            Token::Delim(c @ ('>' | '+' | '~')) => {
                if need {
                    return fail();
                }
                let _ = write!(out, " {c} ");
                need = true;
                space = false;
                continue;
            }
            _ => {}
        }
        if space && !need {
            out.push(' ');
        }
        space = false;
        simple_selector(&token, input, &mut out, true)?;
        need = false;
    }
    if need { fail() } else { Ok(out) }
}

/// One simple selector (or pseudo) starting with `token`; with `full`
/// false (inside `:not()`) only type, universal, class, id and attribute
/// selectors pass.
fn simple_selector(
    token: &Token<'_>,
    input: &mut Parser<'_>,
    out: &mut String,
    full: bool,
) -> Result<(), Error> {
    match token {
        Token::Ident(name) => {
            serialize_identifier(name, out).map_err(|_| ParseError::unexpected_token())?;
        }
        Token::Delim('*') => out.push('*'),
        Token::IDHash(_) => {
            token
                .to_css(out)
                .map_err(|_| ParseError::unexpected_token())?;
        }
        Token::Delim('.') => match input.next_including_whitespace_and_comments()? {
            Token::Ident(class) => {
                out.push('.');
                serialize_identifier(class, out).map_err(|_| ParseError::unexpected_token())?;
            }
            _ => return fail(),
        },
        Token::SquareBracketBlock => {
            out.push('[');
            input.parse_nested_block(|i| attribute_selector(i, out))?;
            out.push(']');
        }
        Token::Colon if full => pseudo(input, out)?,
        _ => return fail(),
    }
    Ok(())
}

fn pseudo(input: &mut Parser<'_>, out: &mut String) -> Result<(), Error> {
    match input.next_including_whitespace_and_comments()?.clone() {
        Token::Ident(name) => {
            let name = name.to_ascii_lowercase();
            if !PSEUDO_CLASSES.contains(&name.as_str()) {
                return fail();
            }
            let _ = write!(out, ":{name}");
        }
        Token::Colon => match input.next_including_whitespace_and_comments()?.clone() {
            Token::Ident(name) => {
                let name = name.to_ascii_lowercase();
                if !PSEUDO_ELEMENTS.contains(&name.as_str()) {
                    return fail();
                }
                let _ = write!(out, "::{name}");
            }
            _ => return fail(),
        },
        Token::Function(name) if name.eq_ignore_ascii_case("nth-child") => {
            let (a, b) = input.parse_nested_block(|i| {
                let nth = parse_nth(i)?;
                i.expect_exhausted()?;
                Ok::<_, Error>(nth)
            })?;
            let _ = write!(out, ":nth-child({a}n{b:+})");
        }
        Token::Function(name) if name.eq_ignore_ascii_case("not") => {
            out.push_str(":not(");
            input.parse_nested_block(|i| not_arguments(i, out))?;
            out.push(')');
        }
        _ => return fail(),
    }
    Ok(())
}

/// Compound selectors without pseudos or combinators, comma-separated.
fn not_arguments(input: &mut Parser<'_>, out: &mut String) -> Result<(), Error> {
    let mut need = true;
    let mut space = false;
    while let Ok(token) = input.next_including_whitespace_and_comments() {
        let token = token.clone();
        match token {
            Token::WhiteSpace(_) | Token::Comment(_) => space = true,
            Token::Comma if !need => {
                out.push(',');
                need = true;
                space = false;
            }
            _ => {
                if space && !need {
                    // A descendant combinator: not a simple argument.
                    return fail();
                }
                simple_selector(&token, input, out, false)?;
                need = false;
                space = false;
            }
        }
    }
    if need { fail() } else { Ok(()) }
}

/// `[name]`, `[name=value]`, `[name~="value" i]` and the other matchers.
fn attribute_selector(input: &mut Parser<'_>, out: &mut String) -> Result<(), Error> {
    let name = input.expect_ident()?.clone();
    serialize_identifier(&name, out).map_err(|_| ParseError::unexpected_token())?;
    if input.is_exhausted() {
        return Ok(());
    }
    let matcher = match input.next()? {
        Token::Delim('=') => "=",
        Token::IncludeMatch => "~=",
        Token::DashMatch => "|=",
        Token::PrefixMatch => "^=",
        Token::SuffixMatch => "$=",
        Token::SubstringMatch => "*=",
        _ => return fail(),
    };
    out.push_str(matcher);
    match input.next()?.clone() {
        Token::Ident(v) => {
            serialize_identifier(&v, out).map_err(|_| ParseError::unexpected_token())?;
        }
        Token::QuotedString(v) if safe_string(&v) => {
            serialize_string(&v, out).map_err(|_| ParseError::unexpected_token())?;
        }
        _ => return fail(),
    }
    if let Ok(flag) = input.try_parse(|i| i.expect_ident().cloned()) {
        let flag = flag.to_ascii_lowercase();
        if flag != "i" && flag != "s" {
            return fail();
        }
        let _ = write!(out, " {flag}");
    }
    input.expect_exhausted()?;
    Ok(())
}

/// The declarations of one style rule; nested rules and at-rules are
/// dropped.
struct Declarations;

impl<'i> DeclarationParser<'i> for Declarations {
    type Declaration = String;
    type Error = ();

    fn parse_value(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i>,
        start: &ParserState,
    ) -> Result<String, Error> {
        let name = name.to_ascii_lowercase();
        if !allowed_property(&name) {
            return fail();
        }
        let mut out = format!("{name}:");
        let important = value(input, 0, &mut out)?;
        if out.len() == name.len() + 1 || escaped(input, start) {
            return fail();
        }
        if important {
            out.push_str("!important");
        }
        // The same substring guard as inline `style` attributes.
        let squashed: String = out
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if CSS_FORBIDDEN.iter().any(|f| squashed.contains(f)) {
            return fail();
        }
        Ok(out)
    }
}

impl<'i> AtRuleParser<'i> for Declarations {
    type Prelude = ();
    type AtRule = String;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for Declarations {
    type Prelude = ();
    type QualifiedRule = String;
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, String, ()> for Declarations {
    fn parse_declarations(&self) -> bool {
        true
    }

    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Writes the allowed tokens of a declaration value; returns whether it
/// ended in `!important`.
fn value(input: &mut Parser<'_>, depth: usize, out: &mut String) -> Result<bool, Error> {
    let mut space = false;
    let start = out.len();
    while let Ok(token) = input.next_including_whitespace_and_comments() {
        let token = token.clone();
        if let Token::WhiteSpace(_) | Token::Comment(_) = token {
            space = true;
            continue;
        }
        if space && out.len() > start {
            out.push(' ');
        }
        space = false;
        match token {
            Token::Ident(_)
            | Token::Number { .. }
            | Token::Percentage { .. }
            | Token::Dimension { .. }
            | Token::Hash(_)
            | Token::IDHash(_) => token
                .to_css(out)
                .map_err(|_| ParseError::unexpected_token())?,
            Token::Comma => out.push(','),
            Token::Delim(c @ ('/' | '+' | '-' | '*')) => out.push(c),
            Token::Delim('!') if depth == 0 => {
                input.expect_ident_matching("important")?;
                input.expect_exhausted()?;
                return Ok(true);
            }
            Token::QuotedString(s) if safe_string(&s) => {
                serialize_string(&s, out).map_err(|_| ParseError::unexpected_token())?;
            }
            Token::Function(name) if depth < MAX_VALUE_NESTING => {
                let name = name.to_ascii_lowercase();
                if !FUNCTIONS.contains(&name.as_str()) {
                    return fail();
                }
                let _ = write!(out, "{name}(");
                input.parse_nested_block(|i| value(i, depth + 1, out))?;
                out.push(')');
            }
            Token::ParenthesisBlock if depth > 0 && depth < MAX_VALUE_NESTING => {
                // Never glue `(` to a name: `url (` is no function call.
                if ident_char(out.chars().last()) {
                    out.push(' ');
                }
                out.push('(');
                input.parse_nested_block(|i| value(i, depth + 1, out))?;
                out.push(')');
            }
            _ => return fail(),
        }
    }
    Ok(false)
}

/// Collects the text of `<style>` elements that ammonia's parse (a
/// fragment in a `<div>`) puts in the HTML namespace, outside SVG, MathML,
/// `<template>` contents and the other elements ammonia drops with their
/// content.
fn collect_styles(html: &str) -> Vec<String> {
    let sink = parse_fragment(
        StyleSink::default(),
        ParseOpts::default(),
        QualName::new(None, ns!(html), local_name!("div")),
        vec![],
        false,
    )
    .one(html);
    sink.styles()
}

/// What the tree builder sees of a node.
struct Meta {
    id: usize,
    name: Option<QualName>,
    integration_point: bool,
}

type Handle = Rc<Meta>;

#[derive(Default)]
struct Entry {
    name: Option<QualName>,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Text children, kept for `style` elements only.
    text: String,
    template_contents: Option<Handle>,
}

/// A minimal arena DOM: element names, parent links and the text of
/// `style` elements. Node 0 is the document.
struct StyleSink {
    entries: RefCell<Vec<Entry>>,
    document: Handle,
    /// Returned by `elem_name` for a non-element, which the tree builder
    /// never asks for.
    no_name: QualName,
}

impl Default for StyleSink {
    fn default() -> StyleSink {
        StyleSink {
            entries: RefCell::new(vec![Entry::default()]),
            document: Rc::new(Meta {
                id: 0,
                name: None,
                integration_point: false,
            }),
            no_name: QualName::new(None, ns!(), local_name!("")),
        }
    }
}

impl StyleSink {
    fn node(&self, name: Option<QualName>, integration_point: bool) -> Handle {
        let mut entries = self.entries.borrow_mut();
        let id = entries.len();
        entries.push(Entry {
            name: name.clone(),
            ..Entry::default()
        });
        Rc::new(Meta {
            id,
            name,
            integration_point,
        })
    }

    fn is_style(entry: &Entry) -> bool {
        entry
            .name
            .as_ref()
            .is_some_and(|n| n.local == local_name!("style"))
    }

    fn detach(entries: &mut [Entry], id: usize) {
        if let Some(parent) = entries[id].parent.take() {
            entries[parent].children.retain(|&c| c != id);
        }
    }

    fn insert(&self, parent: usize, before: Option<usize>, child: NodeOrText<Handle>) {
        let mut entries = self.entries.borrow_mut();
        match child {
            NodeOrText::AppendText(text) => {
                if Self::is_style(&entries[parent]) {
                    entries[parent].text.push_str(&text);
                }
            }
            NodeOrText::AppendNode(node) => {
                Self::detach(&mut entries, node.id);
                entries[node.id].parent = Some(parent);
                let children = &mut entries[parent].children;
                match before.and_then(|b| children.iter().position(|&c| c == b)) {
                    Some(at) => children.insert(at, node.id),
                    None => children.push(node.id),
                }
            }
        }
    }

    /// Style texts in document order; iterative, so deep trees cannot
    /// exhaust the stack.
    fn styles(&self) -> Vec<String> {
        let entries = self.entries.borrow();
        let mut out = Vec::new();
        let mut stack = vec![0];
        while let Some(id) = stack.pop() {
            let entry = &entries[id];
            if let Some(name) = &entry.name {
                if name.ns != ns!(html)
                    || (name.local != local_name!("head")
                        && name.local != local_name!("style")
                        && DROP_WITH_CONTENT.contains(&&*name.local))
                {
                    continue;
                }
                if name.local == local_name!("style") {
                    out.push(entry.text.clone());
                    continue;
                }
            }
            stack.extend(entry.children.iter().rev());
        }
        out
    }
}

impl TreeSink for StyleSink {
    type Handle = Handle;
    type Output = StyleSink;
    type ElemName<'a> = &'a QualName;

    fn finish(self) -> StyleSink {
        self
    }

    fn parse_error(&self, _msg: std::borrow::Cow<'static, str>) {}

    fn get_document(&self) -> Handle {
        self.document.clone()
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> &'a QualName {
        target.name.as_ref().unwrap_or(&self.no_name)
    }

    fn create_element(
        &self,
        name: QualName,
        _attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Handle {
        let node = self.node(Some(name), flags.mathml_annotation_xml_integration_point);
        if flags.template {
            // Detached, so its text never reaches `styles`.
            let contents = self.node(None, false);
            self.entries.borrow_mut()[node.id].template_contents = Some(contents);
        }
        node
    }

    fn create_comment(&self, _text: StrTendril) -> Handle {
        self.node(None, false)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> Handle {
        self.node(None, false)
    }

    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        self.insert(parent.id, None, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        let has_parent = self.entries.borrow()[element.id].parent.is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn get_template_contents(&self, target: &Handle) -> Handle {
        let contents = self.entries.borrow()[target.id].template_contents.clone();
        contents.unwrap_or_else(|| self.node(None, false))
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        x.id == y.id
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        let parent = self.entries.borrow()[sibling.id].parent;
        if let Some(parent) = parent {
            self.insert(parent, Some(sibling.id), new_node);
        }
    }

    fn add_attrs_if_missing(&self, _target: &Handle, _attrs: Vec<Attribute>) {}

    fn remove_from_parent(&self, target: &Handle) {
        Self::detach(&mut self.entries.borrow_mut(), target.id);
    }

    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        let mut entries = self.entries.borrow_mut();
        let children = std::mem::take(&mut entries[node.id].children);
        for &child in &children {
            entries[child].parent = Some(new_parent.id);
        }
        entries[new_parent.id].children.extend(children);
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        handle.integration_point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn css(sheet: &str) -> String {
        sanitize_stylesheet(sheet).unwrap_or_default()
    }

    #[test]
    fn rules_are_rewritten_not_copied() {
        assert_eq!(
            css(
                "p { color : red ; font: 14px/1.4 Arial, \"Helvetica Neue\", sans-serif !important }"
            ),
            "p{color:red;font:14px/1.4 Arial, \"Helvetica Neue\", sans-serif !important}"
        );
        assert_eq!(
            css("p{color:rgb(1,2,3);background:#FFF;border:1px solid hsla(0,0%,0%,.5)}"),
            "p{color:rgb(1,2,3);background:#FFF;border:1px solid hsla(0,0%,0%,0.5)}"
        );
        assert_eq!(
            css("td{padding:calc(10px + 2%);width:min(100%, max(20em, clamp(1px, 2vw, 3px)))}"),
            "td{padding:calc(10px + 2%);width:min(100%, max(20em, clamp(1px, 2vw, 3px)))}"
        );
        assert_eq!(
            css(
                "p{float:left;clear:both;box-sizing:border-box;overflow-x:hidden;visibility:hidden}"
            ),
            "p{float:left;clear:both;box-sizing:border-box;overflow-x:hidden;visibility:hidden}"
        );
    }

    #[test]
    fn selectors_keep_the_allowed_forms() {
        assert_eq!(
            css(concat!(
                "a[href^=\"https\"] > b + i ~ u, *.x:first-child, td:nth-child(2n+1),",
                "p:not(.a, b), p::before, #id, [lang|=en i], li:last-child, tr:nth-child(odd)",
                "{margin:0 auto}"
            )),
            concat!(
                "a[href^=\"https\"] > b + i ~ u,*.x:first-child,td:nth-child(2n+1),",
                "p:not(.a,b),p::before,#id,[lang|=en i],li:last-child,tr:nth-child(2n+1)",
                "{margin:0 auto}"
            )
        );
        for rejected in [
            "body:has(a){display:none}",
            "a:hover{color:red}",
            "a:visited{color:red}",
            "input:checked{color:red}",
            "p::first-line{color:red}",
            "p::selection{color:red}",
            "p:not(a b){color:red}",
            "p:not(:first-child){color:red}",
            "& p{color:red}",
            "svg|a{color:red}",
            "[svg|href]{color:red}",
            "p:is(a){color:red}",
            "p:where(a){color:red}",
            "td:nth-child(2n of .x){color:red}",
            "p,{color:red}",
            "> p{color:red}",
        ] {
            assert_eq!(sanitize_stylesheet(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn media_rules_on_width_and_orientation_survive() {
        assert_eq!(
            css("@media only screen and (max-width: 600px) { .col { display:block!important } }"),
            "@media only screen and (max-width:600px){.col{display:block!important}}"
        );
        assert_eq!(
            css(
                "@media (orientation:landscape), all and (min-device-width:20em), (width){p{float:left}}"
            ),
            "@media (orientation:landscape),all and (min-device-width:20em),(width){p{float:left}}"
        );
        assert_eq!(
            css("@media screen{@media (max-width:0){p{color:red}}}"),
            "@media screen{@media (max-width:0){p{color:red}}}"
        );
        for rejected in [
            "@media screen{@media screen{@media screen{p{color:red}}}}",
            "@media (prefers-color-scheme: dark){p{color:red}}",
            "@media print{p{color:red}}",
            "@media not screen{p{color:red}}",
            "@media (width <= 600px){p{color:red}}",
            "@media (-webkit-min-device-pixel-ratio:2){p{color:red}}",
            "@media (max-width:600cm){p{color:red}}",
            "@media (orientation:600px){p{color:red}}",
            "@media screen and{p{color:red}}",
            "@supports (display:grid){p{color:red}}",
            "@layer x{p{color:red}}",
            "@container (width > 1px){p{color:red}}",
            "@keyframes k{from{color:red}}",
            "@page{margin:0}",
        ] {
            assert_eq!(sanitize_stylesheet(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn values_with_anything_but_the_allowed_functions_are_dropped() {
        for rejected in [
            "p{background:url(https://evil.example/x)}",
            "p{background:url( \"https://evil.example/x\" )}",
            "p{background:URL(x)}",
            "p{background:u\\72l(https://evil.example/x)}",
            "p{background:url/**/(\"https://evil.example/x\")}",
            "p{width:calc(url/**/(\"https://evil.example/x\"))}",
            "p{background:image-set(\"https://evil.example/x\" 1x)}",
            "p{background:-webkit-image-set(\"https://evil.example/x\" 1x)}",
            "p{background:image(\"https://evil.example/x\")}",
            "p{background:cross-fade(url(a), url(b))}",
            "p{background:src(\"https://evil.example/x\")}",
            "p{background:element(#x)}",
            "p{background:paint(x)}",
            "p{width:expression(alert(1))}",
            "p{color:var(--x)}",
            "p{--x:red}",
            "p{width:env(safe-area-inset-left)}",
            "p{width:attr(data-w px)}",
            "p{color:foo(1)}",
            "p{behavior:url(x.htc)}",
            "p{-moz-binding:url(https://evil.example/x.xml#x)}",
            "p{position:fixed}",
            "p{position:absolute;top:0}",
            "p{content:\"x\"}",
            "p{font-family:\"a<b\"}",
            "p{font-family:\"a>b\"}",
            "p{font-family:\"\\3c/style\\3e\"}",
            "p{c\\olor:red}",
            "p{color:red !ie}",
            "p{color:red !important !important}",
            "p{color:[red]}",
            "p{color:{red}}",
            "p{color:}",
            "p{width:calc(((((((1px)))))))}",
        ] {
            assert_eq!(sanitize_stylesheet(rejected), None, "{rejected}");
        }
        assert_eq!(
            css("p{color:red;background:url(x);width:1px}"),
            "p{color:red;width:1px}",
            "one bad declaration does not drop its neighbours"
        );
    }

    #[test]
    fn other_at_rules_are_dropped() {
        assert_eq!(
            css(concat!(
                "@charset \"utf-8\";@import 'https://evil.example/x.css';",
                "@import url(https://evil.example/y.css);@\\69mport \"z.css\";",
                "@namespace svg url(http://www.w3.org/2000/svg);",
                "@font-face{font-family:x;src:url(https://evil.example/f.woff)}",
                "p{color:red}"
            )),
            "p{color:red}"
        );
    }

    #[test]
    fn html_comment_markers_and_brackets_never_reach_the_output() {
        assert_eq!(css("<!-- p{color:red} -->"), "p{color:red}");
        assert_eq!(css("p{color:red}<!--"), "p{color:red}");
        for sheet in [
            "a-->b{color:red}",
            "p{font-family:\"-->\"}",
            "p{font-family:\"<!--\"}",
        ] {
            let out = css(sheet);
            assert!(!out.contains('<') && !out.contains("-->"), "{sheet}: {out}");
        }
    }

    #[test]
    fn caps_drop_the_whole_sheet() {
        let rule = "p{color:red}";
        assert!(sanitize_stylesheet(&rule.repeat(MAX_RULES)).is_some());
        assert_eq!(sanitize_stylesheet(&rule.repeat(MAX_RULES + 1)), None);
        let big = format!("p{{font-family:\"{}\"}}", "a".repeat(MAX_SHEET_BYTES));
        assert_eq!(sanitize_stylesheet(&big), None);
        assert_eq!(
            sanitize_sheets(&["p{color:red}", &"x".repeat(MAX_SHEET_BYTES)]),
            None
        );
        assert_eq!(
            sanitize_sheets(&[rule.repeat(MAX_RULES / 2 + 1), rule.repeat(MAX_RULES / 2)]),
            None,
            "the rule cap spans all sheets of a message"
        );
    }

    #[test]
    fn deep_nesting_neither_panics_nor_survives() {
        for sheet in [
            "{".repeat(100_000),
            "(".repeat(100_000),
            format!(
                "p{{width:{}1px{}}}",
                "calc(".repeat(10_000),
                ")".repeat(10_000)
            ),
            format!(
                "{}p{{color:red}}{}",
                "@media screen{".repeat(10_000),
                "}".repeat(10_000)
            ),
            format!("p:not({})", ":not(".repeat(10_000)),
            format!("p[{}]{{color:red}}", "[".repeat(10_000)),
        ] {
            assert_eq!(sanitize_stylesheet(&sheet), None);
        }
    }

    #[test]
    fn style_text_comes_from_html_namespace_styles_only() {
        let styles = |html: &str| collect_styles(html);
        assert_eq!(
            styles("<style>a{}</style><p>x<style>b{}</style></p><table><style>c{}</style></table>"),
            ["a{}", "b{}", "c{}"]
        );
        assert_eq!(
            styles("<html><head><style>h{}</style></head><body>x</body></html>"),
            ["h{}"]
        );
        for html in [
            "<svg><style>a{}</style></svg>",
            "<math><style>a{}</style></math>",
            "<svg><foreignObject><style>a{}</style></foreignObject></svg>",
            "<math><mtext><style>a{}</style></mtext></math>",
            "<math><annotation-xml encoding=\"text/html\"><style>a{}</style></annotation-xml></math>",
            "<template><style>a{}</style></template>",
            "<noscript><style>a{}</style></noscript>",
            "<select><style>a{}</style></select>",
            "<textarea><style>a{}</style></textarea>",
            "<title><style>a{}</style></title>",
            "<xmp><style>a{}</style></xmp>",
            "<noembed><style>a{}</style></noembed>",
            "<iframe><style>a{}</style></iframe>",
            "<object><style>a{}</style></object>",
            "<!-- <style>a{}</style> -->",
        ] {
            assert!(
                styles(html).iter().all(String::is_empty),
                "{html}: {:?}",
                styles(html)
            );
        }
    }

    #[test]
    fn author_css_skips_bodies_without_style() {
        assert_eq!(author_css("<p>no sheets</p>"), None);
        assert_eq!(
            author_css("<STYLE>p{color:red}</STYLE>").as_deref(),
            Some("p{color:red}")
        );
        assert_eq!(
            author_css("<style>a{color:red}</style><style>b{color:blue}</style>").as_deref(),
            Some("a{color:red}b{color:blue}")
        );
    }
}
