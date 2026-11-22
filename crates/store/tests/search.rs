//! Planner correctness: every strategy returns what a brute-force
//! evaluation of the query over the synthetic store returns.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use proptest::prelude::*;
use tagmail_core::query::{Location, Query, Status, Term, Text, and_of, day_start, or_of, parse};
use tagmail_core::{LabelKey, MessageRowId, ThreadId};
use tagmail_store::rusqlite::{Connection, OpenFlags, params};
use tagmail_store::search::{Strategy as Plan, Thresholds, search_threads_with};
use tagmail_store::{CacheDb, FtsDocument, fts};
use tempfile::TempDir;
use time::{Date, Month};

const WORDS: [&str; 10] = [
    "alpha", "beta", "gamma", "delta", "invoice", "report", "budget", "quick", "brown", "fox",
];
const CONTACTS: [(&str, &str); 4] = [
    ("Alice Smith", "alice@example.com"),
    ("Bob Jones", "bob@example.org"),
    ("Carol White", "carol@x.test"),
    ("Dan Brown", "dan@y.test"),
];
/// id, name, delimiter, special-use.
const MAILBOXES: [(i64, &str, &str, Option<&str>); 7] = [
    (1, "INBOX", "/", None),
    (2, "Sent", "/", Some("\\Sent")),
    (3, "Trash", "/", Some("\\Trash")),
    (4, "Junk", "/", Some("\\Junk")),
    (5, "Archive", "/", Some("\\Archive")),
    (6, "Projects.2026", ".", None),
    (7, "Entw&APw-rfe", "/", None),
];
const TRASH_OR_JUNK: [i64; 2] = [3, 4];
/// Keyword labels by id; label 4 is the folder label of mailbox 6.
const KEYWORD_LABELS: [(i64, &str); 3] = [(1, "Work"), (2, "Travel"), (3, "Receipts")];
const FOLDER_LABEL: (i64, &str, i64) = (4, "Clients", 6);
const MESSAGES: i64 = 400;
const THREADS: i64 = 80;
const DAY: i64 = 86_400;
const START: i64 = 1_767_225_600;

#[derive(Debug)]
struct Msg {
    id: i64,
    thread: i64,
    date: i64,
    subject: String,
    from: String,
    to: String,
    cc_json: String,
    body: String,
    attachment: bool,
    tombstoned: bool,
    /// (mailbox, flags)
    locations: Vec<(i64, u8)>,
    labels: Vec<i64>,
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    messages: Vec<Msg>,
    /// (id, last_date) of threads that are not merged.
    threads: Vec<(i64, i64)>,
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn words(&mut self, n: u64) -> String {
        (0..n)
            .map(|_| *self.pick(&WORDS))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn address_json(c: &[(&str, &str)]) -> String {
    let items: Vec<String> = c
        .iter()
        .map(|(n, e)| format!("{{\"name\":\"{n}\",\"email\":\"{e}\"}}"))
        .collect();
    format!("[{}]", items.join(","))
}

fn address_text(c: &[(&str, &str)]) -> String {
    c.iter()
        .map(|(n, e)| format!("{n} {e}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn generate() -> Vec<Msg> {
    let mut rng = Rng(7);
    (1..=MESSAGES)
        .map(|id| {
            let from = [*rng.pick(&CONTACTS)];
            let to = [*rng.pick(&CONTACTS)];
            let cc: Vec<_> = (0..rng.below(2)).map(|_| *rng.pick(&CONTACTS)).collect();
            let mut locations = vec![(rng.below(7) as i64 + 1, rng.below(4) as u8)];
            if rng.below(4) == 0 {
                let other = rng.below(7) as i64 + 1;
                if other != locations[0].0 {
                    locations.push((other, rng.below(4) as u8));
                }
            }
            let labels = KEYWORD_LABELS
                .iter()
                .filter(|_| rng.below(4) == 0)
                .map(|(id, _)| *id)
                .collect();
            Msg {
                id,
                thread: rng.below(THREADS as u64) as i64 + 1,
                date: START + rng.below(60) as i64 * DAY + rng.below(DAY as u64) as i64,
                subject: rng.words(2),
                from: address_text(&from),
                to: address_text(&[to.as_slice(), cc.as_slice()].concat()),
                cc_json: address_json(&cc),
                body: {
                    let n = 5 + rng.below(4);
                    rng.words(n)
                },
                attachment: rng.below(5) == 0,
                tombstoned: rng.below(20) == 0,
                locations,
                labels,
            }
        })
        .collect()
}

static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.db");
    let messages = generate();
    let mut threads: Vec<(i64, i64)> = (1..=THREADS)
        .map(|t| {
            let last = messages
                .iter()
                .filter(|m| m.thread == t && !m.tombstoned)
                .map(|m| m.date)
                .max()
                .unwrap_or(0);
            (t, last)
        })
        .collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = CacheDb::open(&path).unwrap();
        let rows: Vec<_> = messages
            .iter()
            .map(|m| {
                (
                    m.id,
                    m.thread,
                    m.date,
                    m.subject.clone(),
                    m.from.clone(),
                    m.to.clone(),
                    m.cc_json.clone(),
                    m.body.clone(),
                    m.attachment,
                    m.tombstoned,
                    m.locations.clone(),
                    m.labels.clone(),
                )
            })
            .collect();
        let thread_rows = threads.clone();
        db.write(move |tx| {
            for (id, name, delim, special) in MAILBOXES {
                tx.execute(
                    "INSERT INTO mailbox (id, account_id, name, delimiter, special_use, uidvalidity) \
                     VALUES (?1, 1, ?2, ?3, ?4, 1)",
                    params![id, name, delim, special],
                )?;
            }
            for (id, name) in KEYWORD_LABELS {
                tx.execute(
                    "INSERT INTO label (id, account_id, keyword, label_key, kind) \
                     VALUES (?1, 1, ?2, ?3, 'keyword')",
                    params![id, name, LabelKey::of(name).as_str()],
                )?;
            }
            let (id, name, mailbox) = FOLDER_LABEL;
            tx.execute(
                "INSERT INTO label (id, account_id, label_key, kind, mailbox_id) \
                 VALUES (?1, 1, ?2, 'folder', ?3)",
                params![id, LabelKey::of(name).as_str(), mailbox],
            )?;
            for (id, last) in &thread_rows {
                tx.execute(
                    "INSERT INTO thread (id, account_id, last_date) VALUES (?1, 1, ?2)",
                    params![id, last],
                )?;
            }
            tx.execute(
                "INSERT INTO thread (id, account_id, last_date, merged_into) VALUES (?1, 1, ?2, 1)",
                params![THREADS + 1, START + 1000 * DAY],
            )?;
            for (id, thread, date, subject, from, to, cc, body, att, tomb, locs, labels) in &rows {
                tx.execute(
                    "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, \
                     cc_json, has_attachment, tombstoned_at) VALUES (?1, 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![id, thread, date, subject, cc, att, tomb.then_some(1)],
                )?;
                for (uid, (mailbox, flags)) in locs.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO location (mailbox_id, uid, message_id, flags) VALUES (?1, ?2, ?3, ?4)",
                        params![mailbox, id * 10 + uid as i64, id, flags],
                    )?;
                }
                for label in labels {
                    tx.execute(
                        "INSERT INTO message_label (message_id, label_id) VALUES (?1, ?2)",
                        params![id, label],
                    )?;
                }
                fts::index(
                    tx,
                    MessageRowId(*id),
                    &FtsDocument {
                        subject,
                        from,
                        to,
                        body,
                    },
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        db.close().await.unwrap();
    });
    threads.sort_by_key(|(id, last)| (std::cmp::Reverse(*last), *id));
    Fixture {
        _dir: dir,
        path,
        messages,
        threads,
    }
});

fn connect() -> Connection {
    Connection::open_with_flags(&FIXTURE.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn has_phrase(column: &str, text: &str, prefix: bool) -> bool {
    let want = tokens(text);
    if want.is_empty() {
        return true;
    }
    let have = tokens(column);
    have.windows(want.len()).any(|w| {
        w.iter().zip(&want).enumerate().all(|(i, (h, n))| {
            if prefix && i == want.len() - 1 {
                h.starts_with(n.as_str())
            } else {
                h == n
            }
        })
    })
}

fn mailboxes_named(name: &str) -> Vec<i64> {
    match name.to_lowercase().as_str() {
        "inbox" => vec![1],
        "sent" => vec![2],
        "trash" | "bin" => vec![3],
        "junk" | "spam" => vec![4],
        "archive" => vec![5],
        "projects/2026" => vec![6],
        "entwürfe" => vec![7],
        _ => vec![],
    }
}

fn eval(q: &Query, m: &Msg) -> bool {
    match q {
        Query::And(qs) => qs.iter().all(|q| eval(q, m)),
        Query::Or(qs) => qs.iter().any(|q| eval(q, m)),
        Query::Not(q) => !eval(q, m),
        Query::Term(t) => match t {
            Term::Text(t) => [&m.subject, &m.from, &m.to, &m.body]
                .iter()
                .any(|c| has_phrase(c, &t.value, t.prefix)),
            Term::Subject(t) => has_phrase(&m.subject, &t.value, t.prefix),
            Term::From(v) => has_phrase(&m.from, v, false),
            Term::To(v) => has_phrase(&m.to, v, false),
            Term::Cc(v) => {
                has_phrase(&m.to, v, false) && m.cc_json.to_lowercase().contains(&v.to_lowercase())
            }
            Term::Label(name) => {
                let key = LabelKey::of(name);
                KEYWORD_LABELS
                    .iter()
                    .any(|(id, n)| LabelKey::of(n) == key && m.labels.contains(id))
                    || (LabelKey::of(FOLDER_LABEL.1) == key
                        && m.locations.iter().any(|(mb, _)| *mb == FOLDER_LABEL.2))
            }
            Term::In(Location::Anywhere) => true,
            Term::In(Location::Named(name)) => {
                let ids = mailboxes_named(name);
                m.locations.iter().any(|(mb, _)| ids.contains(mb))
            }
            Term::Is(Status::Unread) => m.locations.iter().any(|(_, f)| f & 1 == 0),
            Term::Is(Status::Read) => m.locations.iter().all(|(_, f)| f & 1 != 0),
            Term::Is(Status::Starred) => m.locations.iter().any(|(_, f)| f & 2 != 0),
            Term::HasAttachment => m.attachment,
            Term::Before(d) => m.date < day_start(*d),
            Term::After(d) => m.date >= day_start(*d),
        },
    }
}

fn expected(q: &Query, limit: usize) -> Vec<ThreadId> {
    let exclude = !q.mentions_location();
    let hit: BTreeSet<i64> = FIXTURE
        .messages
        .iter()
        .filter(|m| !m.tombstoned)
        .filter(|m| {
            !exclude
                || m.locations
                    .iter()
                    .any(|(mb, _)| !TRASH_OR_JUNK.contains(mb))
        })
        .filter(|m| eval(q, m))
        .map(|m| m.thread)
        .collect();
    FIXTURE
        .threads
        .iter()
        .filter(|(id, _)| hit.contains(id))
        .take(limit)
        .map(|(id, _)| ThreadId(*id))
        .collect()
}

fn run(q: &str, t: Thresholds) -> (Vec<ThreadId>, Plan) {
    let query = parse(q).unwrap();
    let r = search_threads_with(&connect(), &query, 100_000, t).unwrap();
    assert_eq!(r.threads, expected(&query, 100_000), "{q} with {t:?}");
    (r.threads, r.strategy)
}

const NARROW: Thresholds = Thresholds {
    narrow_match: 0,
    narrow_filter: 100_000,
};
const BROAD: Thresholds = Thresholds {
    narrow_match: 0,
    narrow_filter: 0,
};
const DEFAULT: Thresholds = Thresholds {
    narrow_match: 5_000,
    narrow_filter: 20_000,
};

#[test]
fn strategies_by_selectivity() {
    assert_eq!(run("alpha", DEFAULT).1, Plan::MatchOnly);
    assert_eq!(run("is:starred", DEFAULT).1, Plan::FilterOnly);
    assert_eq!(run("alpha is:starred", DEFAULT).1, Plan::MatchFirst);
    assert_eq!(run("alpha is:starred", NARROW).1, Plan::FilterFirst);
    assert_eq!(run("alpha is:starred", BROAD).1, Plan::MatchFirst);
    assert_eq!(run("in:anywhere", DEFAULT).1, Plan::Everything);
    assert_eq!(run("in:nope", DEFAULT), (Vec::new(), Plan::Nothing));
    assert_eq!(run("-alpha", DEFAULT).1, Plan::FilterOnly);
    assert_eq!(run("\"!!\"", DEFAULT).1, Plan::Everything);
}

#[test]
fn phrases_prefixes_and_columns() {
    for q in [
        "\"quick brown\"",
        "\"brown quick\"",
        "subject:\"alpha beta\"",
        "inv*",
        "subject:bud*",
        "from:alice@example.com",
        "from:\"Bob Jones\"",
        "to:carol",
        "cc:dan",
        "\"org carol\"",
    ] {
        let (hits, _) = run(q, DEFAULT);
        assert!(!hits.is_empty(), "{q} matches something");
    }
}

#[test]
fn locations_and_default_exclusion() {
    let all = run("in:anywhere", DEFAULT).0;
    let default = run("-zzz", DEFAULT).0;
    let trash = run("in:trash", DEFAULT).0;
    assert!(default.len() < all.len());
    assert!(!trash.is_empty());
    run("in:spam OR in:bin", DEFAULT);
    run("in:Projects/2026", DEFAULT);
    run("in:entwürfe", DEFAULT);
    run("label:clients", DEFAULT);
    run("label:WORK -label:travel", DEFAULT);
    assert!(
        !all.contains(&ThreadId(THREADS + 1)),
        "merged thread hidden"
    );
}

#[test]
fn limit_keeps_newest() {
    let query = parse("alpha OR beta").unwrap();
    let r = search_threads_with(&connect(), &query, 5, DEFAULT).unwrap();
    assert_eq!(r.threads, expected(&query, 5));
}

fn date(day: i64) -> Date {
    let base = Date::from_calendar_date(2026, Month::January, 1).unwrap();
    base + time::Duration::days(day)
}

fn arb_term() -> impl Strategy<Value = Term> {
    let word = prop::sample::select(WORDS.to_vec()).prop_map(str::to_owned);
    let text =
        (word.clone(), prop::option::of(word.clone()), any::<bool>()).prop_map(|(a, b, prefix)| {
            let value = match b {
                Some(b) => format!("{a} {b}"),
                None => a,
            };
            let value = if prefix {
                value[..value.len() - 2].to_owned()
            } else {
                value
            };
            Text { value, prefix }
        });
    let who = prop::sample::select(vec![
        "alice",
        "Bob Jones",
        "carol@x.test",
        "y.test",
        "example",
        "nobody",
    ])
    .prop_map(str::to_owned);
    let mailbox = prop::sample::select(vec![
        "inbox",
        "Sent",
        "trash",
        "spam",
        "archive",
        "projects/2026",
        "Entwürfe",
        "nope",
    ])
    .prop_map(|s| Location::Named(s.to_owned()));
    let label = prop::sample::select(vec!["work", "Travel", "receipts", "clients", "none"])
        .prop_map(str::to_owned);
    prop_oneof![
        text.clone().prop_map(Term::Text),
        text.prop_map(Term::Subject),
        who.clone().prop_map(Term::From),
        who.clone().prop_map(Term::To),
        who.prop_map(Term::Cc),
        label.prop_map(Term::Label),
        mailbox.prop_map(Term::In),
        Just(Term::In(Location::Anywhere)),
        prop::sample::select(vec![Status::Unread, Status::Read, Status::Starred])
            .prop_map(Term::Is),
        Just(Term::HasAttachment),
        (0i64..60).prop_map(|d| Term::Before(date(d))),
        (0i64..60).prop_map(|d| Term::After(date(d))),
    ]
}

fn arb_query() -> impl Strategy<Value = Query> {
    arb_term()
        .prop_map(Query::Term)
        .prop_recursive(3, 12, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 2..4).prop_map(and_of),
                prop::collection::vec(inner.clone(), 2..3).prop_map(or_of),
                inner.prop_map(|q| Query::Not(Box::new(q))),
            ]
        })
}

fn arb_thresholds() -> impl Strategy<Value = Thresholds> {
    (
        prop::sample::select(vec![0usize, 10, 100, 5_000]),
        prop::sample::select(vec![0usize, 10, 100, 20_000]),
    )
        .prop_map(|(narrow_match, narrow_filter)| Thresholds {
            narrow_match,
            narrow_filter,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn planner_matches_brute_force(q in arb_query(), t in arb_thresholds()) {
        let r = search_threads_with(&connect(), &q, 100_000, t).unwrap();
        prop_assert_eq!(r.threads, expected(&q, 100_000), "{} ({:?})", q, r.strategy);
    }

    #[test]
    fn canonical_text_searches_the_same(q in arb_query()) {
        let reparsed = parse(&q.to_string()).unwrap();
        let a = search_threads_with(&connect(), &q, 100_000, DEFAULT).unwrap();
        let b = search_threads_with(&connect(), &reparsed, 100_000, DEFAULT).unwrap();
        prop_assert_eq!(a, b);
    }
}
