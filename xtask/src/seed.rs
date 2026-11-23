//! Deterministic Maildir++ generator for test and benchmark fixtures.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use time::format_description::well_known::Rfc2822;
use time::{OffsetDateTime, UtcOffset};

pub const MANIFEST: &str = "tagmail-seed.json";
pub const KEYWORDS: [&str; 4] = ["Work", "Travel", "Receipts", "$Forwarded"];

const ME_NAME: &str = "Tagmail Tester";
const ME_ADDR: &str = "user1@tagmail.test";
const START: i64 = 1_420_070_400;
const SPAN_SECS: i64 = 10 * 365 * 86_400;
const MIN_STEP_SECS: i64 = 60;
const CONTACTS: u64 = 500;
const REPLY_WINDOW: usize = 64;
const MAX_REFERENCES: usize = 20;
const LARGE_EVERY: u64 = 20_000;
const LARGE_MIN_COUNT: u64 = 100;
const LARGE_MIN_BYTES: u64 = 1 << 20;
const LARGE_MAX_BYTES: u64 = 4 << 20;
const MIN_TARGET_BYTES: u64 = 1_500;
const TEXT_WIDTH: usize = 72;
const QP_WIDTH: usize = 75;
const BASE64_WIDTH: usize = 76;
const ENCODED_WORD_CHUNK: usize = 30;
/// Subject of the `--huge-mb` message, for finding it through search.
pub const HUGE_SUBJECT: &str = "tagmail-bench-huge";
const HUGE_HTML_BYTES: usize = 2 << 20;
const BYTES_PER_MB: u64 = 1_000_000;
/// The huge message is the newest in INBOX.
const HUGE_AFTER_SECS: i64 = 86_400;

const SEEN: u8 = 1;
const ANSWERED: u8 = 2;
const FLAGGED: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mailbox {
    Inbox,
    Sent,
    Archive,
}

impl Mailbox {
    const ALL: [Mailbox; 3] = [Mailbox::Inbox, Mailbox::Sent, Mailbox::Archive];

    fn name(self) -> &'static str {
        match self {
            Mailbox::Inbox => "INBOX",
            Mailbox::Sent => "Sent",
            Mailbox::Archive => "Archive",
        }
    }

    fn dir(self, root: &Path) -> PathBuf {
        match self {
            Mailbox::Inbox => root.to_path_buf(),
            other => root.join(format!(".{}", other.name())),
        }
    }
}

pub struct Options {
    pub count: u64,
    pub avg_kb: u64,
    pub seed: u64,
    /// Appends one message of this many megabytes to INBOX.
    pub huge_mb: Option<u64>,
    pub out: PathBuf,
}

pub struct Summary {
    pub count: u64,
    pub bytes: u64,
}

struct Thread {
    seed: u64,
    contact: u64,
    archived: bool,
    latin1: bool,
    last_with_id: Option<usize>,
}

struct Planned {
    date: i64,
    tz_minutes: i32,
    thread: usize,
    parent: Option<usize>,
    has_id: bool,
    from_me: bool,
    mailbox: Mailbox,
    flags: u8,
    keywords: u8,
    target: u64,
    large: bool,
    content_seed: u64,
}

struct Plan {
    seed: u64,
    threads: Vec<Thread>,
    messages: Vec<Planned>,
}

/// SplitMix64: tiny, fast and stable across platforms and releases.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn percent(&mut self, p: u64) -> bool {
        self.below(100) < p
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn fill(&mut self, out: &mut Vec<u8>, len: usize) {
        while out.len() < len {
            let bytes = self.next_u64().to_le_bytes();
            let take = (len - out.len()).min(bytes.len());
            out.extend_from_slice(&bytes[..take]);
        }
    }
}

pub fn generate(opts: &Options) -> Result<Summary> {
    if opts.count == 0 {
        bail!("--count must be at least 1");
    }
    if opts.avg_kb == 0 {
        bail!("--avg-kb must be at least 1");
    }
    if opts.out.exists() && fs::read_dir(&opts.out)?.next().is_some() {
        bail!("{} exists and is not empty", opts.out.display());
    }
    for mailbox in Mailbox::ALL {
        let dir = mailbox.dir(&opts.out);
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("creating {}", dir.join(sub).display()))?;
        }
        if mailbox != Mailbox::Inbox {
            fs::write(dir.join("maildirfolder"), "")?;
        }
        let keywords: String = KEYWORDS
            .iter()
            .enumerate()
            .map(|(i, k)| format!("{i} {k}\n"))
            .collect();
        fs::write(dir.join("dovecot-keywords"), keywords)?;
    }

    let plan = plan(opts);
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = plan.messages.len().div_ceil(workers);
    let results: Vec<Result<u64>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..plan.messages.len())
            .step_by(chunk)
            .map(|start| {
                let plan = &plan;
                let out = &opts.out;
                scope.spawn(move || {
                    let end = (start + chunk).min(plan.messages.len());
                    (start..end).try_fold(0u64, |bytes, i| Ok(bytes + write_message(plan, i, out)?))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("seed worker panicked"))
            .collect()
    });
    let mut bytes = 0;
    for result in results {
        bytes += result?;
    }
    let huge = match opts.huge_mb {
        Some(mb) => Some(write_huge(opts.seed, mb, &opts.out)?),
        None => None,
    };
    bytes += huge.unwrap_or(0);

    fs::write(
        opts.out.join(MANIFEST),
        serde_json::to_vec_pretty(&manifest(opts, &plan, bytes, huge))?,
    )?;
    Ok(Summary {
        count: opts.count + u64::from(huge.is_some()),
        bytes,
    })
}

fn manifest(opts: &Options, plan: &Plan, bytes: u64, huge: Option<u64>) -> serde_json::Value {
    let mailboxes: serde_json::Map<String, serde_json::Value> = Mailbox::ALL
        .iter()
        .map(|&mailbox| {
            let messages = plan.messages.iter().filter(|m| m.mailbox == mailbox);
            let count = |flag: u8| messages.clone().filter(|m| m.flags & flag != 0).count();
            let keywords: serde_json::Map<String, serde_json::Value> = KEYWORDS
                .iter()
                .enumerate()
                .map(|(bit, k)| {
                    let n = messages.clone().filter(|m| m.keywords & (1 << bit) != 0);
                    ((*k).to_owned(), json!(n.count()))
                })
                .collect();
            let extra = usize::from(huge.is_some() && mailbox == Mailbox::Inbox);
            let value = json!({
                "messages": messages.clone().count() + extra,
                "seen": count(SEEN),
                "answered": count(ANSWERED),
                "flagged": count(FLAGGED),
                "without_message_id": messages.clone().filter(|m| !m.has_id).count(),
                "keywords": keywords,
            });
            (mailbox.name().to_owned(), value)
        })
        .collect();
    json!({
        "seed": opts.seed,
        "count": opts.count,
        "avg_kb": opts.avg_kb,
        "bytes": bytes,
        "threads": plan.threads.len(),
        "mailboxes": mailboxes,
        "huge": huge.map(|bytes| json!({ "subject": HUGE_SUBJECT, "bytes": bytes })),
    })
}

fn plan(opts: &Options) -> Plan {
    let mut rng = Rng::new(opts.seed);
    let count = opts.count;
    let step = (SPAN_SECS / count as i64).max(MIN_STEP_SECS);

    let large = if count >= LARGE_MIN_COUNT {
        count / LARGE_EVERY + 1
    } else {
        0
    };
    let stride = count / large.max(1);
    let is_large = |i: u64| large > 0 && i % stride == stride / 2 && i / stride < large;
    let large_sizes: Vec<u64> = (0..large)
        .map(|_| rng.range(LARGE_MIN_BYTES, LARGE_MAX_BYTES))
        .collect();
    let budget = (count * opts.avg_kb * 1024).saturating_sub(large_sizes.iter().sum::<u64>());
    let mean = (budget / (count - large).max(1)).max(MIN_TARGET_BYTES);

    let mut threads: Vec<Thread> = Vec::new();
    let mut messages: Vec<Planned> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let date = START + i as i64 * step + rng.below(step as u64) as i64;
        let tz_minutes = *rng.pick(&[0, 60, 120, -300, -420, 330, 540, -180]);
        let content_seed = rng.next_u64();
        let open = threads.len().saturating_sub(REPLY_WINDOW)..threads.len();
        let reply_to = if !open.is_empty() && rng.percent(55) {
            let t = open.start + rng.below(open.len() as u64) as usize;
            threads[t].last_with_id.map(|parent| (t, parent))
        } else {
            None
        };
        let has_id = !rng.percent(2);
        let (thread, parent, from_me) = match reply_to {
            Some((t, parent)) => {
                let from_me = !messages[parent].from_me && rng.percent(60);
                (t, Some(parent), from_me)
            }
            None => {
                threads.push(Thread {
                    seed: rng.next_u64(),
                    contact: rng.below(CONTACTS),
                    archived: rng.percent(30),
                    latin1: rng.percent(12),
                    last_with_id: None,
                });
                (threads.len() - 1, None, rng.percent(20))
            }
        };
        let idx = messages.len();
        if has_id {
            threads[thread].last_with_id = Some(idx);
        }
        let mailbox = match (from_me, threads[thread].archived) {
            (true, _) => Mailbox::Sent,
            (false, true) => Mailbox::Archive,
            (false, false) => Mailbox::Inbox,
        };
        let mut flags = 0;
        if from_me || mailbox == Mailbox::Archive || rng.percent(80) {
            flags |= SEEN;
        }
        if rng.percent(3) {
            flags |= FLAGGED;
        }
        if let (true, Some(p)) = (from_me, parent) {
            messages[p].flags |= ANSWERED;
        }
        let mut keywords = 0;
        for (bit, pct) in [8, 3, 4, 1].into_iter().enumerate() {
            if rng.percent(pct) {
                keywords |= 1 << bit;
            }
        }
        let large = is_large(i);
        let target = if large {
            large_sizes[(i / stride) as usize]
        } else {
            let factor = (-(1.0 - rng.unit()).ln()).clamp(0.15, 6.0);
            (mean as f64 * factor) as u64
        };
        messages.push(Planned {
            date,
            tz_minutes,
            thread,
            parent,
            has_id,
            from_me,
            mailbox,
            flags,
            keywords,
            target,
            large,
            content_seed,
        });
    }
    Plan {
        seed: opts.seed,
        threads,
        messages,
    }
}

fn write_message(plan: &Plan, i: usize, root: &Path) -> Result<u64> {
    let message = &plan.messages[i];
    let bytes = render(plan, i);
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let mut flags = String::new();
    for (bit, letter) in [(FLAGGED, 'F'), (ANSWERED, 'R'), (SEEN, 'S')] {
        if message.flags & bit != 0 {
            flags.push(letter);
        }
    }
    for bit in 0..KEYWORDS.len() {
        if message.keywords & (1 << bit) != 0 {
            flags.push(char::from(b'a' + bit as u8));
        }
    }
    let name = format!(
        "{}.M{i}P{}.tagmail,S={},W={}:2,{flags}",
        message.date,
        plan.seed % 100_000,
        bytes.len(),
        bytes.len() + newlines,
    );
    let path = message.mailbox.dir(root).join("cur").join(name);
    fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    let mtime = UNIX_EPOCH + Duration::from_secs(message.date as u64);
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(mtime)?;
    Ok(bytes.len() as u64)
}

#[rustfmt::skip]
const FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Carol", "David", "Erin", "Frank", "Grace", "Heidi", "Ivan", "Judy", "Mallory",
    "Niaj", "Olivia", "Peggy", "Rupert", "Sybil", "Trent", "Victor", "Walter", "Zoë", "José",
    "Søren", "Łukasz", "Jürgen", "François", "Ångström", "Chloé", "Анна", "美咲",
];
#[rustfmt::skip]
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Miller", "Brown", "Garcia", "Müller", "Schäfer", "Dubois", "Lefèvre",
    "Nowak", "Rossi", "Tanaka", "Kowalski", "O'Brien", "Petrović", "Иванова", "Andersen",
];
#[rustfmt::skip]
const DOMAINS: &[&str] = &[
    "example.com", "example.org", "example.net", "mail.example.de", "corp.example.fr",
    "lists.example.org",
];
#[rustfmt::skip]
const SUBJECT_WORDS: &[&str] = &[
    "quarterly", "report", "meeting", "notes", "invoice", "trip", "plan", "review", "draft",
    "budget", "launch", "update", "question", "photos", "schedule", "contract", "dinner",
    "release", "agenda", "feedback", "proposal", "weekend", "project", "migration", "server",
];
#[rustfmt::skip]
const SUBJECT_EXTRAS_UTF8: &[&str] = &[
    "café ☕", "Zürich", "東京 出張", "Отчёт", "🎉 party", "naïve résumé", "Ελλάδα",
];
#[rustfmt::skip]
const SUBJECT_EXTRAS_LATIN1: &[&str] = &[
    "Rechnung März", "Grüße", "réunion", "Übersicht", "à bientôt",
];
#[rustfmt::skip]
const WORDS: &[&str] = &[
    "the", "of", "and", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on", "are",
    "as", "with", "his", "they", "at", "be", "this", "have", "from", "or", "one", "had", "by",
    "word", "but", "not", "what", "all", "were", "we", "when", "your", "can", "said", "there",
    "use", "an", "each", "which", "she", "do", "how", "their", "if", "will", "up", "other",
    "about", "out", "many", "then", "them", "these", "so", "some", "her", "would", "make", "like",
    "into", "time", "has", "look", "two", "more", "write", "go", "see", "number", "no", "way",
    "could", "people", "my", "than", "first", "water", "been", "call", "who", "now", "find",
    "long", "down", "day", "did", "get", "come", "made", "may", "part", "meeting", "server",
    "mailbox", "attached", "deadline", "budget", "review", "schedule", "invoice", "thanks",
];
#[rustfmt::skip]
const WORDS_UTF8: &[&str] = &[
    "café", "naïve", "Zürich", "日本語", "Москва", "😀", "→", "€", "Ελληνικά", "ﬁ", "“quoted”",
];
#[rustfmt::skip]
const WORDS_LATIN1: &[&str] = &[
    "für", "Grüße", "Straße", "schön", "Mädchen", "über", "Brötchen", "déjà", "garçon", "être",
    "très", "élève", "Noël", "año", "señor",
];
#[rustfmt::skip]
const MAILERS: &[&str] = &[
    "Thunderbird 128.4.0", "Apple Mail (2.3826)", "Microsoft Outlook 16.0",
    "Roundcube Webmail/1.6.9", "K-9 Mail for Android",
];

struct Contact {
    name: String,
    addr: String,
}

fn contact(n: u64) -> Contact {
    let first = FIRST_NAMES[(n % FIRST_NAMES.len() as u64) as usize];
    let last = LAST_NAMES[(n / FIRST_NAMES.len() as u64 % LAST_NAMES.len() as u64) as usize];
    let domain = DOMAINS[(n % DOMAINS.len() as u64) as usize];
    Contact {
        name: format!("{first} {last}"),
        addr: format!("contact{n}@{domain}"),
    }
}

fn me() -> Contact {
    Contact {
        name: ME_NAME.to_owned(),
        addr: ME_ADDR.to_owned(),
    }
}

fn message_id(plan: &Plan, i: usize) -> String {
    let m = &plan.messages[i];
    let domain = if m.from_me {
        "tagmail.test"
    } else {
        DOMAINS[(plan.threads[m.thread].contact % DOMAINS.len() as u64) as usize]
    };
    format!(
        "<{i:x}.{:x}.{:08x}@{domain}>",
        plan.seed, m.content_seed as u32
    )
}

fn sender(plan: &Plan, i: usize) -> Contact {
    let m = &plan.messages[i];
    if m.from_me {
        me()
    } else {
        contact(plan.threads[m.thread].contact)
    }
}

fn subject(thread: &Thread) -> String {
    let mut rng = Rng::new(thread.seed);
    let mut words: Vec<&str> = (0..rng.range(2, 6))
        .map(|_| *rng.pick(SUBJECT_WORDS))
        .collect();
    if thread.latin1 {
        words.push(rng.pick(SUBJECT_EXTRAS_LATIN1));
    } else if rng.percent(25) {
        words.push(rng.pick(SUBJECT_EXTRAS_UTF8));
    }
    let mut s = words.join(" ");
    if let Some(first) = s.get(..1) {
        s.replace_range(..1, &first.to_uppercase());
    }
    if rng.percent(5) {
        s = format!("Fwd: {s}");
    }
    s
}

fn format_date(epoch: i64, tz_minutes: i32) -> String {
    let offset = UtcOffset::from_whole_seconds(tz_minutes * 60).expect("valid offset");
    OffsetDateTime::from_unix_timestamp(epoch)
        .expect("valid timestamp")
        .to_offset(offset)
        .format(&Rfc2822)
        .expect("RFC 2822 date")
}

/// RFC 2047 encoded words, folded so no header line exceeds 78 characters.
fn encode_words(s: &str, latin1: bool) -> String {
    if s.is_ascii() {
        return s.to_owned();
    }
    let latin1 = latin1 && s.chars().all(|c| (c as u32) <= 0xFF);
    let mut words = Vec::new();
    let mut chunk = String::new();
    let mut encoded = String::new();
    for c in s.chars() {
        if latin1 {
            let b = c as u32 as u8;
            match b {
                b' ' => encoded.push('_'),
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'-' => encoded.push(c),
                _ => write!(encoded, "={b:02X}").expect("write to string"),
            }
        } else {
            chunk.push(c);
        }
        if encoded.len() >= ENCODED_WORD_CHUNK || chunk.len() >= ENCODED_WORD_CHUNK {
            words.push(encoded_word(&mut chunk, &mut encoded, latin1));
        }
    }
    if !chunk.is_empty() || !encoded.is_empty() {
        words.push(encoded_word(&mut chunk, &mut encoded, latin1));
    }
    words.join("\n ")
}

fn encoded_word(chunk: &mut String, encoded: &mut String, latin1: bool) -> String {
    let word = if latin1 {
        format!("=?ISO-8859-1?Q?{encoded}?=")
    } else {
        format!("=?UTF-8?B?{}?=", BASE64.encode(chunk.as_bytes()))
    };
    chunk.clear();
    encoded.clear();
    word
}

fn mailbox_header(c: &Contact, latin1: bool) -> String {
    if c.name.is_ascii() {
        format!("\"{}\" <{}>", c.name.replace('"', ""), c.addr)
    } else {
        format!("{}\n <{}>", encode_words(&c.name, latin1), c.addr)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    English,
    Utf8,
    Latin1,
}

fn text(rng: &mut Rng, lang: Lang, target: usize) -> String {
    let mut out = String::with_capacity(target + TEXT_WIDTH);
    let mut line = 0;
    let mut sentence_start = true;
    while out.len() < target {
        let word = match lang {
            Lang::Utf8 if rng.percent(4) => rng.pick(WORDS_UTF8),
            Lang::Latin1 if rng.percent(10) => rng.pick(WORDS_LATIN1),
            _ => rng.pick(WORDS),
        };
        let width = word.chars().count();
        if line > 0 && line + 1 + width > TEXT_WIDTH {
            out.push('\n');
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        if sentence_start {
            let mut chars = word.chars();
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
                out.push_str(chars.as_str());
            }
        } else {
            out.push_str(word);
        }
        line += width;
        sentence_start = rng.percent(10);
        if sentence_start {
            out.push('.');
            line += 1;
            if rng.percent(20) {
                out.push_str("\n\n");
                line = 0;
            }
        }
    }
    out.push_str(".\n");
    out
}

fn quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 8);
    let mut col = 0;
    for (i, &b) in input.iter().enumerate() {
        if b == b'\n' {
            out.push(b'\n');
            col = 0;
            continue;
        }
        let at_eol = input.get(i + 1).is_none_or(|&next| next == b'\n');
        let literal = matches!(b, b'!'..=b'<' | b'>'..=b'~') || (b == b' ' && !at_eol);
        let width = if literal { 1 } else { 3 };
        if col + width > QP_WIDTH {
            out.extend_from_slice(b"=\n");
            col = 0;
        }
        if literal {
            out.push(b);
        } else {
            write!(out, "={b:02X}").expect("write to vec");
        }
        col += width;
    }
    out
}

fn base64_lines(input: &[u8]) -> Vec<u8> {
    let encoded = BASE64.encode(input);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / BASE64_WIDTH + 1);
    for line in encoded.as_bytes().chunks(BASE64_WIDTH) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

fn to_latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(c as u32).unwrap_or(b'?'))
        .collect()
}

#[derive(Clone, Copy)]
enum Encoding {
    EightBit,
    QuotedPrintable,
    Base64,
}

impl Encoding {
    fn header(self) -> &'static str {
        match self {
            Encoding::EightBit => "8bit",
            Encoding::QuotedPrintable => "quoted-printable",
            Encoding::Base64 => "base64",
        }
    }

    fn apply(self, raw: &[u8]) -> Vec<u8> {
        match self {
            Encoding::EightBit => raw.to_vec(),
            Encoding::QuotedPrintable => quoted_printable(raw),
            Encoding::Base64 => base64_lines(raw),
        }
    }

    fn overhead(self) -> f64 {
        match self {
            Encoding::EightBit => 1.0,
            Encoding::QuotedPrintable => 1.1,
            Encoding::Base64 => 1.37,
        }
    }
}

struct Part {
    headers: String,
    body: Vec<u8>,
}

fn text_part(rng: &mut Rng, subtype: &str, lang: Lang, budget: usize, prefix: &str) -> Part {
    let encoding = *rng.pick(&[
        Encoding::EightBit,
        Encoding::EightBit,
        Encoding::QuotedPrintable,
        Encoding::Base64,
    ]);
    let raw_target = (budget as f64 / encoding.overhead()) as usize;
    let mut body = text(rng, lang, raw_target.saturating_sub(prefix.len()));
    if subtype == "html" {
        let paragraphs: Vec<String> = body
            .split("\n\n")
            .map(|p| format!("<p>{}</p>", p.replace('&', "&amp;")))
            .collect();
        body = format!(
            "<!DOCTYPE html>\n<html><head><meta charset=\"{}\"></head><body>\n{}\n</body></html>\n",
            if lang == Lang::Latin1 {
                "iso-8859-1"
            } else {
                "utf-8"
            },
            paragraphs.join("\n")
        );
    }
    let body = format!("{prefix}{body}");
    let (charset, raw) = if lang == Lang::Latin1 {
        ("iso-8859-1", to_latin1(&body))
    } else {
        ("utf-8", body.into_bytes())
    };
    Part {
        headers: format!(
            "Content-Type: text/{subtype}; charset={charset}\nContent-Transfer-Encoding: {}\n",
            encoding.header()
        ),
        body: encoding.apply(&raw),
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._".contains(&b) {
            out.push(b as char);
        } else {
            write!(out, "%{b:02X}").expect("write to string");
        }
    }
    out
}

fn attachment(rng: &mut Rng, raw_len: usize) -> Part {
    const KINDS: &[(&str, &str, &[u8])] = &[
        ("application/pdf", "pdf", b"%PDF-1.7\n"),
        ("image/jpeg", "jpg", b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00"),
        ("image/png", "png", b"\x89PNG\r\n\x1A\n"),
        ("application/zip", "zip", b"PK\x03\x04"),
    ];
    const STEMS: &[&str] = &["report", "invoice", "photo", "scan", "slides", "backup"];
    const STEMS_UTF8: &[&str] = &["Rechnung-März", "résumé", "фото", "报告"];
    let (mime, ext, magic) = *rng.pick(KINDS);
    let mut data = magic.to_vec();
    rng.fill(&mut data, raw_len.max(magic.len()));
    let disposition = if rng.percent(15) {
        let name = format!("{}.{ext}", rng.pick(STEMS_UTF8));
        format!("attachment;\n filename*=UTF-8''{}", percent_encode(&name))
    } else {
        format!(
            "attachment; filename=\"{}-{}.{ext}\"",
            rng.pick(STEMS),
            rng.below(1000)
        )
    };
    Part {
        headers: format!(
            "Content-Type: {mime}\nContent-Transfer-Encoding: base64\nContent-Disposition: {disposition}\n"
        ),
        body: base64_lines(&data),
    }
}

fn multipart(subtype: &str, boundary: &str, parts: Vec<Part>) -> Part {
    let mut body = b"This is a multi-part message in MIME format.\n".to_vec();
    for part in parts {
        write!(body, "\n--{boundary}\n{}\n", part.headers).expect("write to vec");
        body.extend_from_slice(&part.body);
    }
    write!(body, "\n--{boundary}--\n").expect("write to vec");
    Part {
        headers: format!("Content-Type: multipart/{subtype};\n boundary=\"{boundary}\"\n"),
        body,
    }
}

fn render(plan: &Plan, i: usize) -> Vec<u8> {
    let m = &plan.messages[i];
    let thread = &plan.threads[m.thread];
    let mut rng = Rng::new(m.content_seed);
    let from = sender(plan, i);
    let to = if m.from_me {
        contact(thread.contact)
    } else {
        me()
    };
    let latin1 = thread.latin1;
    let date = format_date(m.date, m.tz_minutes);

    let mut h = String::new();
    if !m.from_me {
        writeln!(h, "Return-Path: <{}>", from.addr).expect("write to string");
        writeln!(
            h,
            "Received: from mx.{}\n by mx.tagmail.test with ESMTPS id {:x};\n {date}",
            from.addr.split('@').nth(1).unwrap_or("example.com"),
            m.content_seed
        )
        .expect("write to string");
    }
    writeln!(h, "Date: {date}").expect("write to string");
    writeln!(h, "From: {}", mailbox_header(&from, latin1)).expect("write to string");
    writeln!(h, "To: {}", mailbox_header(&to, latin1)).expect("write to string");
    if rng.percent(20) {
        let cc = contact(rng.below(CONTACTS));
        writeln!(h, "Cc: {}", mailbox_header(&cc, latin1)).expect("write to string");
    }
    let mut subj = subject(thread);
    if m.parent.is_some() {
        subj = format!("{}{subj}", if latin1 { "AW: " } else { "Re: " });
    }
    writeln!(h, "Subject: {}", encode_words(&subj, latin1)).expect("write to string");
    if m.has_id {
        writeln!(h, "Message-ID: {}", message_id(plan, i)).expect("write to string");
    }
    if let Some(parent) = m.parent {
        let mut ancestors = Vec::new();
        let mut cursor = Some(parent);
        while let Some(p) = cursor {
            if ancestors.len() == MAX_REFERENCES {
                break;
            }
            ancestors.push(message_id(plan, p));
            cursor = plan.messages[p].parent;
        }
        writeln!(h, "In-Reply-To: {}", ancestors[0]).expect("write to string");
        if !rng.percent(5) {
            ancestors.reverse();
            writeln!(h, "References: {}", ancestors.join("\n ")).expect("write to string");
        }
    }
    writeln!(h, "X-Mailer: {}", rng.pick(MAILERS)).expect("write to string");
    h.push_str("MIME-Version: 1.0\n");

    let lang = if latin1 {
        Lang::Latin1
    } else if rng.percent(30) {
        Lang::Utf8
    } else {
        Lang::English
    };
    let prefix = match m.parent {
        Some(p) => {
            let parent = sender(plan, p);
            format!(
                "On {}, {} wrote:\n> {}\n\n",
                format_date(plan.messages[p].date, plan.messages[p].tz_minutes),
                parent.name,
                text(&mut rng, lang, TEXT_WIDTH)
                    .trim_end()
                    .replace('\n', "\n> ")
            )
        }
        None => String::new(),
    };
    let budget = (m.target as usize).saturating_sub(h.len() + 200);
    let boundary = |n: u32| format!("=_tagmail_{i:x}_{n}");
    let kind = if m.large { 99 } else { rng.below(100) };
    let body = if kind < 35 {
        text_part(&mut rng, "plain", lang, budget, &prefix)
    } else if kind < 80 {
        let plain = text_part(&mut rng, "plain", lang, budget * 2 / 5, &prefix);
        let html = text_part(&mut rng, "html", lang, budget * 3 / 5, "");
        multipart("alternative", &boundary(1), vec![plain, html])
    } else {
        let text_budget = if m.large { 2_000 } else { budget * 2 / 5 };
        let main = if rng.percent(50) {
            text_part(&mut rng, "plain", lang, text_budget, &prefix)
        } else {
            let plain = text_part(&mut rng, "plain", lang, text_budget / 2, &prefix);
            let html = text_part(&mut rng, "html", lang, text_budget / 2, "");
            multipart("alternative", &boundary(2), vec![plain, html])
        };
        let files = if m.large { 1 } else { rng.range(1, 3) as usize };
        let raw_total = budget.saturating_sub(text_budget) * 3 / 4;
        let mut parts = vec![main];
        parts.extend((0..files).map(|_| attachment(&mut rng, raw_total / files)));
        multipart("mixed", &boundary(3), parts)
    };
    h.push_str(&body.headers);
    h.push('\n');
    let mut out = h.into_bytes();
    out.extend_from_slice(&body.body);
    out
}

/// An unread HTML newsletter with one attachment, `mb` megabytes in all,
/// newer than every planned message; returns its size.
fn write_huge(seed: u64, mb: u64, root: &Path) -> Result<u64> {
    let mut rng = Rng::new(seed ^ HUGE_SUBJECT.len() as u64);
    let date = START + SPAN_SECS + HUGE_AFTER_SECS;
    let from = contact(rng.below(CONTACTS));
    let mut h = String::new();
    writeln!(h, "Date: {}", format_date(date, 0)).expect("write to string");
    writeln!(h, "From: {}", mailbox_header(&from, false)).expect("write to string");
    writeln!(h, "To: {}", mailbox_header(&me(), false)).expect("write to string");
    writeln!(h, "Subject: {HUGE_SUBJECT}").expect("write to string");
    writeln!(h, "Message-ID: <huge.{seed:x}@tagmail.test>").expect("write to string");
    h.push_str("MIME-Version: 1.0\n");
    let target = (mb * BYTES_PER_MB) as usize;
    let html = text_part(&mut rng, "html", Lang::English, HUGE_HTML_BYTES, "");
    let encoded_len = target.saturating_sub(h.len() + html.body.len());
    let raw_len = encoded_len * 3 / 4 * BASE64_WIDTH / (BASE64_WIDTH + 1);
    let file = attachment(&mut rng, raw_len);
    let body = multipart(
        "mixed",
        &format!("=_tagmail_huge_{seed:x}"),
        vec![html, file],
    );
    h.push_str(&body.headers);
    h.push('\n');
    let mut bytes = h.into_bytes();
    bytes.extend_from_slice(&body.body);
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let name = format!(
        "{date}.Mhuge.tagmail,S={},W={}:2,",
        bytes.len(),
        bytes.len() + newlines
    );
    let path = Mailbox::Inbox.dir(root).join("cur").join(name);
    fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(date as u64))?;
    Ok(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;
    use std::collections::{BTreeMap, HashSet};

    fn seed_into(dir: &Path, count: u64, avg_kb: u64, seed: u64) -> Summary {
        generate(&Options {
            count,
            avg_kb,
            seed,
            huge_mb: None,
            out: dir.to_path_buf(),
        })
        .expect("seed")
    }

    /// Contents of every file, plus the mtime of messages (INTERNALDATE).
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, Option<std::time::SystemTime>)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    let is_message = rel.parent().is_some_and(|d| d.ends_with("cur"));
                    let mtime =
                        is_message.then(|| fs::metadata(&path).unwrap().modified().unwrap());
                    out.insert(rel, (fs::read(&path).unwrap(), mtime));
                }
            }
        }
        out
    }

    fn messages(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        snapshot(root)
            .into_iter()
            .filter(|(_, (_, mtime))| mtime.is_some())
            .map(|(p, (bytes, _))| (p, bytes))
            .collect()
    }

    #[test]
    fn same_seed_produces_identical_maildir() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        seed_into(a.path(), 500, 4, 7);
        seed_into(b.path(), 500, 4, 7);
        seed_into(c.path(), 500, 4, 8);
        let (sa, sb, sc) = (snapshot(a.path()), snapshot(b.path()), snapshot(c.path()));
        assert_eq!(sa.len(), 500 + 3 + 2 + 1);
        assert!(sa == sb, "same seed must produce byte-identical output");
        assert!(sa != sc, "different seeds must differ");
    }

    #[test]
    fn refuses_non_empty_output() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x"), "").unwrap();
        let opts = Options {
            count: 1,
            avg_kb: 1,
            seed: 1,
            huge_mb: None,
            out: dir.path().to_path_buf(),
        };
        assert!(generate(&opts).is_err());
    }

    #[test]
    fn messages_are_valid_rfc5322_with_realistic_variety() {
        let dir = tempfile::tempdir().unwrap();
        let summary = seed_into(dir.path(), 3_000, 8, 42);
        let files = messages(dir.path());
        assert_eq!(files.len(), 3_000);

        let parser = MessageParser::default();
        let mut ids = HashSet::new();
        let mut referenced = Vec::new();
        let (mut without_id, mut replies, mut large, mut attachments, mut html) = (0, 0, 0, 0, 0);
        let mut charsets = HashSet::new();
        let mut encodings = HashSet::new();
        let mut mailboxes = BTreeMap::<String, usize>::new();
        let mut keywords = 0;
        for (path, bytes) in &files {
            let name = path.file_name().unwrap().to_str().unwrap();
            let size: usize = name
                .split(",S=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let vsize: usize = name
                .split(",W=")
                .nth(1)
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(size, bytes.len(), "{name}: S= must equal file size");
            let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
            assert_eq!(
                vsize,
                bytes.len() + newlines,
                "{name}: W= must equal CRLF size"
            );
            let flags = name.rsplit(":2,").next().unwrap();
            keywords += usize::from(flags.chars().any(|c| c.is_ascii_lowercase()));
            let mailbox = path
                .components()
                .next()
                .unwrap()
                .as_os_str()
                .to_str()
                .unwrap();
            *mailboxes.entry(mailbox.to_owned()).or_default() += 1;

            let header_end = bytes.windows(2).position(|w| w == b"\n\n").unwrap();
            for line in bytes[..header_end].split(|&b| b == b'\n') {
                assert!(line.is_ascii(), "{name}: headers must be 7-bit");
                assert!(line.len() <= 78, "{name}: header line too long: {line:?}");
            }
            for line in bytes.split(|&b| b == b'\n') {
                assert!(line.len() <= 998, "{name}: line exceeds RFC 5322 limit");
            }

            let msg = parser
                .parse(bytes.as_slice())
                .unwrap_or_else(|| panic!("{name}: unparseable"));
            assert!(
                msg.from().is_some() && msg.to().is_some(),
                "{name}: missing From/To"
            );
            assert!(msg.date().is_some(), "{name}: missing or invalid Date");
            let subject = msg
                .subject()
                .unwrap_or_else(|| panic!("{name}: missing Subject"));
            assert!(
                !subject.contains("=?"),
                "{name}: undecoded encoded word in {subject}"
            );
            match msg.message_id() {
                Some(id) => assert!(ids.insert(id.to_owned()), "{name}: duplicate Message-ID"),
                None => without_id += 1,
            }
            let parent = msg.in_reply_to().and_then(|ids| ids.first());
            if let Some(parent) = parent {
                replies += 1;
                referenced.push(parent.to_owned());
            }
            if let Some(refs) = msg.references() {
                assert_eq!(
                    refs.last(),
                    parent,
                    "{name}: References must end with In-Reply-To"
                );
            }
            for part in msg.parts() {
                if let Some(cs) = part.content_type().and_then(|ct| ct.attribute("charset")) {
                    charsets.insert(cs.to_ascii_lowercase());
                }
                if let Some(cte) = part.content_transfer_encoding() {
                    encodings.insert(cte.to_ascii_lowercase());
                }
            }
            for text in (0..msg.text_body().len()).filter_map(|n| msg.body_text(n)) {
                assert!(
                    !text.contains('\u{FFFD}'),
                    "{name}: text body failed to decode"
                );
            }
            html += usize::from(
                msg.html_body().len() > 0 && msg.body_html(0).is_some_and(|b| b.contains("<p>")),
            );
            attachments += usize::from(msg.attachments().len() > 0);
            large += usize::from(bytes.len() >= LARGE_MIN_BYTES as usize);
        }

        for parent in &referenced {
            assert!(
                ids.contains(parent),
                "In-Reply-To {parent} points to an unknown message"
            );
        }
        assert!(without_id > 0, "some messages must lack a Message-ID");
        assert!(replies > 1_000, "expected thread replies, got {replies}");
        assert!(html > 1_000, "expected HTML alternatives, got {html}");
        assert!(attachments > 100, "expected attachments, got {attachments}");
        assert_eq!(large, 1, "expected one large message for 3k messages");
        assert!(keywords > 100, "expected keywords, got {keywords}");
        for cs in ["utf-8", "iso-8859-1"] {
            assert!(charsets.contains(cs), "missing charset {cs}: {charsets:?}");
        }
        for cte in ["8bit", "quoted-printable", "base64"] {
            assert!(
                encodings.contains(cte),
                "missing encoding {cte}: {encodings:?}"
            );
        }
        for mailbox in ["cur", ".Sent", ".Archive"] {
            assert!(
                mailboxes.get(mailbox).is_some_and(|&n| n > 300),
                "{mailboxes:?}"
            );
        }

        let avg = summary.bytes as f64 / 3_000.0 / 1024.0;
        assert!(
            (6.8..=9.2).contains(&avg),
            "average size {avg:.2} KB not near 8 KB"
        );
    }

    #[test]
    fn manifest_matches_files() {
        let dir = tempfile::tempdir().unwrap();
        seed_into(dir.path(), 800, 2, 3);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(MANIFEST)).unwrap()).unwrap();
        for (mailbox, sub) in [
            ("INBOX", "cur"),
            ("Sent", ".Sent/cur"),
            ("Archive", ".Archive/cur"),
        ] {
            let names: Vec<String> = fs::read_dir(dir.path().join(sub))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            let entry = &manifest["mailboxes"][mailbox];
            assert_eq!(entry["messages"], names.len());
            let flagged = |c: char| {
                names
                    .iter()
                    .filter(|n| n.rsplit(":2,").next().unwrap().contains(c))
                    .count()
            };
            assert_eq!(entry["seen"], flagged('S'));
            assert_eq!(entry["answered"], flagged('R'));
            assert_eq!(entry["flagged"], flagged('F'));
            assert_eq!(entry["keywords"]["Work"], flagged('a'));
            assert_eq!(entry["keywords"]["$Forwarded"], flagged('d'));
        }
    }

    #[test]
    fn huge_message_is_the_newest_unread_inbox_message() {
        const MB: u64 = 3;
        let dir = tempfile::tempdir().unwrap();
        let summary = generate(&Options {
            count: 200,
            avg_kb: 2,
            seed: 5,
            huge_mb: Some(MB),
            out: dir.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(summary.count, 201);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(MANIFEST)).unwrap()).unwrap();
        let inbox: Vec<_> = fs::read_dir(dir.path().join("cur"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(manifest["mailboxes"]["INBOX"]["messages"], inbox.len());
        let newest = inbox
            .iter()
            .max_by_key(|p| fs::metadata(p).unwrap().modified().unwrap())
            .unwrap();
        assert!(newest.to_string_lossy().ends_with(":2,"), "unread");
        let raw = fs::read(newest).unwrap();
        assert_eq!(manifest["huge"]["bytes"], raw.len());
        let target = (MB * BYTES_PER_MB) as usize;
        assert!(raw.len().abs_diff(target) < target / 100, "{}", raw.len());
        let parsed = MessageParser::default().parse(raw.as_slice()).unwrap();
        assert_eq!(parsed.subject(), Some(HUGE_SUBJECT));
        assert!(parsed.body_html(0).is_some());
        assert_eq!(parsed.attachments().count(), 1);
    }

    #[test]
    fn quoted_printable_round_trips_through_parser() {
        let raw = "Grüße aus Zürich = 100 % \nline with trailing space \n".repeat(5);
        let encoded = quoted_printable(&to_latin1(&raw));
        assert!(encoded.split(|&b| b == b'\n').all(|l| l.len() <= 76));
        let mut msg = b"Subject: qp\nContent-Type: text/plain; charset=iso-8859-1\nContent-Transfer-Encoding: quoted-printable\n\n".to_vec();
        msg.extend_from_slice(&encoded);
        let parsed = MessageParser::default().parse(msg.as_slice()).unwrap();
        assert_eq!(parsed.body_text(0).unwrap(), raw);
    }
}
