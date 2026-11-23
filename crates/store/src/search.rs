//! Executes a [`Query`] against `cache.db` (plan §4 M2 "Search semantics").
//!
//! Free text, `subject:`, `from:` and `to:` compile to one FTS5 `MATCH`
//! (`from:`/`to:` as column phrases over the indexed address words); the
//! other operators compile to SQL over `message`. SQL filters cannot be
//! pushed into FTS5, so the planner probes the `MATCH` with a limit: a
//! narrow `MATCH` drives and the filter is checked per hit; a broad `MATCH`
//! with a narrow filter evaluates the filter to a rowid set first and
//! intersects it with the `MATCH` result, which SQLite builds once (an FTS5
//! `rowid IN` lookup would rescan the doclist per rowid). Results are
//! threads, newest first.

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};
use tagmail_core::query::{Location, Query, Status, Term, Text, day_start};
use tagmail_core::{AccountId, LabelKey, SpecialUse, ThreadId, decode_mailbox_name};

use crate::error::Result;

/// Search results share the id-index cap of plan §3.11.
pub const MAX_HITS: usize = 100_000;

/// Selectivity cut-offs for [`search_threads_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// A `MATCH` with at most this many hits drives the query.
    pub narrow_match: usize,
    /// A filter with at most this many messages is intersected with a
    /// broad `MATCH` via `rowid IN`.
    pub narrow_filter: usize,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds {
            narrow_match: 5_000,
            narrow_filter: 20_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The query can match nothing, e.g. `in:` an unknown folder.
    Nothing,
    /// No constraint besides the default Trash/Junk exclusion.
    Everything,
    MatchOnly,
    FilterOnly,
    /// `MATCH` first, filter joined on rowid.
    MatchFirst,
    /// Filter rowid set first, intersected with the `MATCH` set.
    FilterFirst,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub threads: Vec<ThreadId>,
    pub strategy: Strategy,
}

/// Threads matching `query`, newest first; with `account`, only that
/// account's.
pub fn search_threads(
    conn: &Connection,
    query: &Query,
    account: Option<AccountId>,
    limit: usize,
) -> Result<SearchResult> {
    search_in(conn, query, account, limit, Thresholds::default())
}

pub fn search_threads_with(
    conn: &Connection,
    query: &Query,
    limit: usize,
    thresholds: Thresholds,
) -> Result<SearchResult> {
    search_in(conn, query, None, limit, thresholds)
}

fn search_in(
    conn: &Connection,
    query: &Query,
    account: Option<AccountId>,
    limit: usize,
    thresholds: Thresholds,
) -> Result<SearchResult> {
    let mailboxes = mailboxes(conn)?;
    let node = simplify(lower(query, &mailboxes));
    let excluded: Vec<i64> = if query.mentions_location() {
        Vec::new()
    } else {
        mailboxes
            .iter()
            .filter(|m| m.trash_or_junk())
            .map(|m| m.id)
            .collect()
    };

    let conjuncts = match node {
        Node::False => {
            return Ok(SearchResult {
                threads: Vec::new(),
                strategy: Strategy::Nothing,
            });
        }
        Node::True => Vec::new(),
        Node::And(nodes) => nodes,
        other => vec![other],
    };
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    let mut rest = Vec::new();
    for c in conjuncts {
        match c {
            Node::Not(inner) if fts_expr(&inner).is_some() => negative.push(*inner),
            c if fts_expr(&c).is_some() => positive.push(c),
            c => rest.push(c),
        }
    }
    let match_expr = if positive.is_empty() {
        rest.extend(negative.into_iter().map(|n| Node::Not(Box::new(n))));
        None
    } else {
        let mut expr = positive
            .iter()
            .filter_map(fts_expr)
            .map(|e| format!("({e})"))
            .collect::<Vec<_>>()
            .join(" AND ");
        for n in negative.iter().filter_map(fts_expr) {
            expr = format!("({expr}) NOT ({n})");
        }
        Some(expr)
    };
    let mut filter = Sql::default();
    for (i, node) in rest.iter().enumerate() {
        if i > 0 {
            filter.text.push_str(" AND ");
        }
        sql(node, &mut filter);
    }

    let mut driver = Sql::default();
    let strategy = match (&match_expr, filter.text.is_empty()) {
        (None, true) => Strategy::Everything,
        (None, false) => Strategy::FilterOnly,
        (Some(expr), true) => {
            driver.push_match(expr);
            Strategy::MatchOnly
        }
        (Some(expr), false) => {
            if count_match(conn, expr, thresholds.narrow_match)? <= thresholds.narrow_match {
                driver.push_match(expr);
                Strategy::MatchFirst
            } else {
                let ids = filter_ids(conn, &filter, thresholds.narrow_filter)?;
                if ids.len() <= thresholds.narrow_filter {
                    driver.text.push_str("SELECT value FROM json_each(?)");
                    driver.params.push(Value::Text(json_ids(&ids)));
                    filter = Sql::default();
                    filter.text.push_str("m.id IN (");
                    filter.push_match(expr);
                    filter.text.push(')');
                    Strategy::FilterFirst
                } else {
                    driver.push_match(expr);
                    Strategy::MatchFirst
                }
            }
        }
    };

    let mut q = Sql::default();
    q.text
        .push_str("SELECT t.id FROM thread t WHERE t.merged_into IS NULL");
    if let Some(account) = account {
        q.text.push_str(" AND t.account_id = ?");
        q.params.push(Value::Integer(account.0));
    }
    q.text
        .push_str(" AND t.id IN (SELECT m.thread_id FROM message m WHERE m.tombstoned_at IS NULL");
    if !driver.text.is_empty() {
        q.text.push_str(" AND m.id IN (");
        q.append(driver);
        q.text.push(')');
    }
    if !filter.text.is_empty() {
        q.text.push_str(" AND (");
        q.append(filter);
        q.text.push(')');
    }
    if !excluded.is_empty() {
        let list = id_list(&excluded);
        q.text.push_str(&format!(
            " AND m.id NOT IN (SELECT l.message_id FROM location l WHERE l.mailbox_id IN ({list}) \
             AND NOT EXISTS (SELECT 1 FROM location o WHERE o.message_id = l.message_id \
             AND o.mailbox_id NOT IN ({list})))"
        ));
    }
    q.text.push_str(") ORDER BY t.last_date DESC, t.id LIMIT ?");
    q.params
        .push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&q.text)?;
    let threads = stmt
        .query_map(params_from_iter(&q.params), |r| r.get(0).map(ThreadId))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tracing::debug!(?strategy, hits = threads.len(), "search");
    Ok(SearchResult { threads, strategy })
}

fn count_match(conn: &Connection, expr: &str, cap: usize) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "SELECT count(*) FROM (SELECT rowid FROM message_fts WHERE message_fts MATCH ?1 LIMIT ?2)",
    )?;
    let cap = i64::try_from(cap.saturating_add(1)).unwrap_or(i64::MAX);
    let n: i64 = stmt.query_row(rusqlite::params![expr, cap], |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/// Up to `cap + 1` ids of messages passing `filter`.
fn filter_ids(conn: &Connection, filter: &Sql, cap: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id FROM message m WHERE m.tombstoned_at IS NULL AND ({}) LIMIT {}",
        filter.text,
        cap.saturating_add(1)
    ))?;
    let ids = stmt
        .query_map(params_from_iter(&filter.params), |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

fn json_ids(ids: &[i64]) -> String {
    format!("[{}]", id_list(ids))
}

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

struct Mailbox {
    id: i64,
    /// Decoded, with the server's delimiter replaced by `/`.
    path: String,
    special_use: Option<SpecialUse>,
}

const TRASH_OR_JUNK_NAMES: [&str; 3] = ["trash", "junk", "spam"];

impl Mailbox {
    fn trash_or_junk(&self) -> bool {
        match self.special_use {
            Some(u) => matches!(u, SpecialUse::Trash | SpecialUse::Junk),
            None => TRASH_OR_JUNK_NAMES
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&self.path)),
        }
    }

    /// `in:` matches a special-use name (Gmail's `in:spam` too), `inbox`,
    /// or the folder path compared case-insensitively.
    fn matches(&self, name: &str) -> bool {
        let special = match name.to_lowercase().as_str() {
            "sent" => Some(SpecialUse::Sent),
            "drafts" | "draft" => Some(SpecialUse::Drafts),
            "trash" | "bin" => Some(SpecialUse::Trash),
            "junk" | "spam" => Some(SpecialUse::Junk),
            "archive" => Some(SpecialUse::Archive),
            "all" => Some(SpecialUse::All),
            "important" => Some(SpecialUse::Important),
            "flagged" => Some(SpecialUse::Flagged),
            _ => None,
        };
        (special.is_some() && self.special_use == special)
            || self.path.to_lowercase() == name.to_lowercase()
    }
}

fn mailboxes(conn: &Connection) -> Result<Vec<Mailbox>> {
    let mut stmt = conn.prepare_cached("SELECT id, name, delimiter, special_use FROM mailbox")?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(1)?;
        let delimiter: Option<String> = r.get(2)?;
        let special: Option<String> = r.get(3)?;
        let decoded = decode_mailbox_name(&name);
        let path = match delimiter.as_deref() {
            Some(d) if !d.is_empty() && d != "/" => decoded.replace(d, "/"),
            _ => decoded,
        };
        Ok(Mailbox {
            id: r.get(0)?,
            path,
            special_use: special.and_then(|s| s.parse().ok()),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A query lowered to FTS atoms and SQL predicates over `message m`.
#[derive(Debug, Clone)]
enum Node {
    True,
    False,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Fts(String),
    Sql(Sql),
}

#[derive(Debug, Clone, Default)]
struct Sql {
    text: String,
    params: Vec<Value>,
}

impl Sql {
    fn leaf(text: &str, params: Vec<Value>) -> Node {
        Node::Sql(Sql {
            text: text.to_owned(),
            params,
        })
    }

    fn append(&mut self, other: Sql) {
        self.text.push_str(&other.text);
        self.params.extend(other.params);
    }

    fn push_match(&mut self, expr: &str) {
        self.text
            .push_str("SELECT rowid FROM message_fts WHERE message_fts MATCH ?");
        self.params.push(Value::Text(expr.to_owned()));
    }
}

/// An FTS5 phrase over `column`, or `None` if the text has no token
/// characters (the unicode61 tokenizer would index nothing).
fn phrase(column: Option<&str>, text: &str, prefix: bool) -> Option<String> {
    if !text.chars().any(char::is_alphanumeric) {
        return None;
    }
    let mut out = String::new();
    if let Some(c) = column {
        out.push_str(c);
        out.push_str(" : ");
    }
    out.push('"');
    out.push_str(&text.replace('"', "\"\""));
    out.push('"');
    if prefix {
        out.push('*');
    }
    Some(out)
}

fn fts_node(column: Option<&str>, text: &Text) -> Node {
    phrase(column, &text.value, text.prefix).map_or(Node::True, Node::Fts)
}

fn like_pattern(value: &str) -> String {
    let mut out = String::from("%");
    for c in value.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

const UNREAD: &str =
    "EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & 1) = 0)";
const STARRED: &str =
    "EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & 2) != 0)";
const LABEL: &str = "m.id IN (SELECT ml.message_id FROM message_label ml \
     JOIN label lb ON lb.id = ml.label_id WHERE lb.kind = 'keyword' AND lb.label_key = ? \
     UNION ALL SELECT l.message_id FROM location l \
     JOIN label lb ON lb.mailbox_id = l.mailbox_id WHERE lb.kind = 'folder' AND lb.label_key = ?)";

fn lower(query: &Query, mailboxes: &[Mailbox]) -> Node {
    match query {
        Query::And(qs) => Node::And(qs.iter().map(|q| lower(q, mailboxes)).collect()),
        Query::Or(qs) => Node::Or(qs.iter().map(|q| lower(q, mailboxes)).collect()),
        Query::Not(q) => Node::Not(Box::new(lower(q, mailboxes))),
        Query::Term(term) => match term {
            Term::Text(t) => fts_node(None, t),
            Term::Subject(t) => fts_node(Some("subject"), t),
            Term::From(v) => phrase(Some("from_txt"), v, false).map_or(Node::True, Node::Fts),
            Term::To(v) => phrase(Some("to_txt"), v, false).map_or(Node::True, Node::Fts),
            Term::Cc(v) => Node::And(vec![
                phrase(Some("to_txt"), v, false).map_or(Node::True, Node::Fts),
                Sql::leaf(
                    "m.cc_json LIKE ? ESCAPE '\\'",
                    vec![Value::Text(like_pattern(v))],
                ),
            ]),
            Term::Label(name) => {
                let key = LabelKey::of(name).as_str().to_owned();
                Sql::leaf(LABEL, vec![Value::Text(key.clone()), Value::Text(key)])
            }
            Term::In(Location::Anywhere) => Node::True,
            Term::In(Location::Named(name)) => {
                let ids: Vec<i64> = mailboxes
                    .iter()
                    .filter(|m| m.matches(name))
                    .map(|m| m.id)
                    .collect();
                if ids.is_empty() {
                    Node::False
                } else {
                    Sql::leaf(
                        &format!(
                            "m.id IN (SELECT message_id FROM location \
                             WHERE mailbox_id IN ({}) AND hidden = 0)",
                            id_list(&ids)
                        ),
                        Vec::new(),
                    )
                }
            }
            Term::Is(Status::Unread) => Sql::leaf(UNREAD, Vec::new()),
            Term::Is(Status::Read) => Sql::leaf(&format!("NOT {UNREAD}"), Vec::new()),
            Term::Is(Status::Starred) => Sql::leaf(STARRED, Vec::new()),
            Term::HasAttachment => Sql::leaf("m.has_attachment = 1", Vec::new()),
            Term::Before(d) => Sql::leaf("m.date < ?", vec![Value::Integer(day_start(*d))]),
            Term::After(d) => Sql::leaf("m.date >= ?", vec![Value::Integer(day_start(*d))]),
        },
    }
}

/// Folds `True`/`False` away; the result is `True`, `False` or free of
/// both.
fn simplify(node: Node) -> Node {
    match node {
        Node::And(nodes) => {
            let mut out = Vec::new();
            for n in nodes.into_iter().map(simplify) {
                match n {
                    Node::True => {}
                    Node::False => return Node::False,
                    Node::And(inner) => out.extend(inner),
                    n => out.push(n),
                }
            }
            match out.len() {
                0 => Node::True,
                1 => out.pop().expect("one node"),
                _ => Node::And(out),
            }
        }
        Node::Or(nodes) => {
            let mut out = Vec::new();
            for n in nodes.into_iter().map(simplify) {
                match n {
                    Node::False => {}
                    Node::True => return Node::True,
                    Node::Or(inner) => out.extend(inner),
                    n => out.push(n),
                }
            }
            match out.len() {
                0 => Node::False,
                1 => out.pop().expect("one node"),
                _ => Node::Or(out),
            }
        }
        Node::Not(inner) => match simplify(*inner) {
            Node::True => Node::False,
            Node::False => Node::True,
            Node::Not(n) => *n,
            n => Node::Not(Box::new(n)),
        },
        n => n,
    }
}

/// The subtree as one FTS5 expression, if it only has FTS atoms and every
/// negation has a positive sibling (FTS5's `NOT` is binary).
fn fts_expr(node: &Node) -> Option<String> {
    match node {
        Node::Fts(e) => Some(e.clone()),
        Node::Or(nodes) => Some(
            nodes
                .iter()
                .map(|n| fts_expr(n).map(|e| format!("({e})")))
                .collect::<Option<Vec<_>>>()?
                .join(" OR "),
        ),
        Node::And(nodes) => {
            let mut positive = Vec::new();
            let mut negative = Vec::new();
            for n in nodes {
                match n {
                    Node::Not(inner) => negative.push(fts_expr(inner)?),
                    n => positive.push(format!("({})", fts_expr(n)?)),
                }
            }
            if positive.is_empty() {
                return None;
            }
            let mut expr = positive.join(" AND ");
            for n in negative {
                expr = format!("({expr}) NOT ({n})");
            }
            Some(expr)
        }
        Node::Not(_) | Node::Sql(_) | Node::True | Node::False => None,
    }
}

/// Writes a predicate over `message m`.
fn sql(node: &Node, out: &mut Sql) {
    if let Some(expr) = fts_expr(node) {
        out.text.push_str("m.id IN (");
        out.push_match(&expr);
        out.text.push(')');
        return;
    }
    match node {
        Node::True => out.text.push('1'),
        Node::False => out.text.push('0'),
        Node::And(nodes) | Node::Or(nodes) => {
            let op = if matches!(node, Node::And(_)) {
                " AND "
            } else {
                " OR "
            };
            out.text.push('(');
            for (i, n) in nodes.iter().enumerate() {
                if i > 0 {
                    out.text.push_str(op);
                }
                sql(n, out);
            }
            out.text.push(')');
        }
        Node::Not(inner) => {
            out.text.push_str("NOT (");
            sql(inner, out);
            out.text.push(')');
        }
        Node::Sql(leaf) => {
            out.text.push('(');
            out.append(leaf.clone());
            out.text.push(')');
        }
        Node::Fts(_) => unreachable!("FTS atoms are handled by fts_expr"),
    }
}
