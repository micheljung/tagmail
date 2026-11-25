//! Write operations (plan §3.3, §3.4 Writes): what each op kind does to a
//! message's locations, projected optimistically over the server state
//! while the op is pending, and how the executor finds its mailboxes again.
//!
//! An [`Op`] holds only IMAP-stable references (email ids, keywords and a
//! [`MailboxRef`]), so the same projection works on the server's cache and
//! on a desktop replica.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use crate::email_id::EmailId;
use crate::error::{Error, Result};
use crate::flags::{Flags, Keyword, KeywordSet};
use crate::ids::{MailboxRef, SpecialUse};
use crate::label::{KeywordClass, LabelKey, classify_keyword};

/// Name of the archive folder created when a server has neither `\Archive`
/// nor `\All`.
pub const ARCHIVE_NAME: &str = "Archive";
const INBOX: &str = "INBOX";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    LabelAdd,
    LabelRemove,
    Archive,
    /// Back to INBOX.
    Unarchive,
    Move,
    MarkRead,
    MarkUnread,
    Star,
    Unstar,
    Trash,
    Spam,
    /// Permanently, from Trash or Junk.
    Delete,
}

impl OpKind {
    pub const ALL: [OpKind; 12] = [
        OpKind::LabelAdd,
        OpKind::LabelRemove,
        OpKind::Archive,
        OpKind::Unarchive,
        OpKind::Move,
        OpKind::MarkRead,
        OpKind::MarkUnread,
        OpKind::Star,
        OpKind::Unstar,
        OpKind::Trash,
        OpKind::Spam,
        OpKind::Delete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::LabelAdd => "label.add",
            OpKind::LabelRemove => "label.remove",
            OpKind::Archive => "archive",
            OpKind::Unarchive => "unarchive",
            OpKind::Move => "move",
            OpKind::MarkRead => "markRead",
            OpKind::MarkUnread => "markUnread",
            OpKind::Star => "star",
            OpKind::Unstar => "unstar",
            OpKind::Trash => "trash",
            OpKind::Spam => "spam",
            OpKind::Delete => "delete",
        }
    }

    pub fn needs_label(self) -> bool {
        matches!(self, OpKind::LabelAdd | OpKind::LabelRemove)
    }

    pub fn needs_mailbox(self) -> bool {
        self == OpKind::Move
    }

    /// Moves messages between mailboxes (or out of them) rather than
    /// storing flags.
    pub fn is_move(self) -> bool {
        matches!(
            self,
            OpKind::Archive
                | OpKind::Unarchive
                | OpKind::Move
                | OpKind::Trash
                | OpKind::Spam
                | OpKind::Delete
        )
    }
}

impl fmt::Display for OpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OpKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<OpKind> {
        OpKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| Error::UnknownOpKind(s.to_owned()))
    }
}

/// `pending_op.state`. Only pending and running ops are projected; a
/// failed op stays visible until it is retried or discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpState {
    Pending,
    Running,
    Done,
    Failed,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Pending => "pending",
            OpState::Running => "running",
            OpState::Done => "done",
            OpState::Failed => "failed",
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, OpState::Pending | OpState::Running)
    }
}

impl FromStr for OpState {
    type Err = Error;

    fn from_str(s: &str) -> Result<OpState> {
        [
            OpState::Pending,
            OpState::Running,
            OpState::Done,
            OpState::Failed,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
        .ok_or_else(|| Error::UnknownOpState(s.to_owned()))
    }
}

/// How an account archives (plan §3.3): MOVE to `\Archive` (or a created
/// `Archive` folder), or on Gmail-like servers with `\All` only leave
/// INBOX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveMode {
    MoveArchive,
    MoveAll,
}

impl ArchiveMode {
    /// `account.archive_mode` if set, else `\All` decides.
    pub fn detect(configured: Option<ArchiveMode>, has_all: bool) -> ArchiveMode {
        configured.unwrap_or(if has_all {
            ArchiveMode::MoveAll
        } else {
            ArchiveMode::MoveArchive
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveMode::MoveArchive => "move_archive",
            ArchiveMode::MoveAll => "move_all",
        }
    }
}

impl FromStr for ArchiveMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<ArchiveMode> {
        match s {
            "move_archive" => Ok(ArchiveMode::MoveArchive),
            "move_all" => Ok(ArchiveMode::MoveAll),
            other => Err(Error::UnknownArchiveMode(other.to_owned())),
        }
    }
}

/// The label an op adds or removes: the keyword to STORE and the identity
/// every spelling of it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelArg {
    pub keyword: Keyword,
    pub key: LabelKey,
}

/// One write the client asked for, with the email ids it saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub id: String,
    pub kind: OpKind,
    pub email_ids: BTreeSet<EmailId>,
    pub label: Option<LabelArg>,
    /// Destination of [`OpKind::Move`].
    pub mailbox: Option<MailboxRef>,
}

/// What a mailbox is to the ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Inbox,
    Special(SpecialUse),
    Folder,
}

impl Role {
    pub fn of(name: &str, special_use: Option<SpecialUse>) -> Role {
        if name.eq_ignore_ascii_case("INBOX") {
            Role::Inbox
        } else if let Some(u) = special_use {
            Role::Special(u)
        } else {
            Role::Folder
        }
    }
}

/// A mailbox as the projection sees it.
#[derive(Debug, Clone, Copy)]
pub struct MailboxView<'a> {
    pub name: &'a str,
    pub role: Role,
    pub archive_mode: ArchiveMode,
    /// Whether this is the account's archive mailbox; with several
    /// candidates the caller picks one (see [`archive_mailbox`]).
    pub is_archive: bool,
}

/// Picks the archive mailbox among `(name, special_use, delimiter)`
/// triples: `\All` in [`ArchiveMode::MoveAll`], else `\Archive`, else a
/// folder named [`ARCHIVE_NAME`] (see [`folder_named`]).
pub fn archive_mailbox<'a, I>(mode: ArchiveMode, mailboxes: I) -> Option<&'a str>
where
    I: IntoIterator<Item = (&'a str, Option<SpecialUse>, Option<char>)> + Clone,
{
    let by_use = |u: SpecialUse| {
        mailboxes
            .clone()
            .into_iter()
            .find(|(_, s, _)| *s == Some(u))
            .map(|(n, _, _)| n)
    };
    match mode {
        ArchiveMode::MoveAll => by_use(SpecialUse::All),
        ArchiveMode::MoveArchive => by_use(SpecialUse::Archive).or_else(|| {
            folder_named(
                &[ARCHIVE_NAME],
                mailboxes
                    .into_iter()
                    .filter(|(_, s, _)| s.is_none())
                    .map(|(n, _, d)| (n, d)),
            )
        }),
    }
}

/// The part of `name` below `INBOX`, for servers whose personal namespace
/// is `INBOX<delimiter>` (Dovecot's `INBOX.Trash`).
fn under_inbox(name: &str, delimiter: Option<char>) -> Option<&str> {
    let (parent, rest) = name.split_once(delimiter?)?;
    parent.eq_ignore_ascii_case(INBOX).then_some(rest)
}

/// Finds a folder named one of `leaves` (case-insensitive, in order of
/// preference) among `(name, delimiter)` pairs: at the top level, else
/// directly under `INBOX`. Deeper folders of that name are the user's own.
pub fn folder_named<'a, I>(leaves: &[&str], mailboxes: I) -> Option<&'a str>
where
    I: IntoIterator<Item = (&'a str, Option<char>)>,
{
    let mailboxes: Vec<_> = mailboxes.into_iter().collect();
    let pick = |hit: &dyn Fn(&str, Option<char>, &str) -> bool| {
        leaves.iter().find_map(|leaf| {
            mailboxes
                .iter()
                .find(|(n, d)| hit(n, *d, leaf))
                .map(|(n, _)| *n)
        })
    };
    pick(&|n, _, leaf| n.eq_ignore_ascii_case(leaf)).or_else(|| {
        pick(&|n, d, leaf| under_inbox(n, d).is_some_and(|l| l.eq_ignore_ascii_case(leaf)))
    })
}

/// The name to create the missing folder `leaf` under: in the server's
/// personal `namespace`; without one, below `INBOX` when most of the
/// account's other folders live there, else top-level.
pub fn new_folder_name<'a, I>(leaf: &str, namespace: Option<&str>, mailboxes: I) -> String
where
    I: IntoIterator<Item = (&'a str, Option<char>)>,
{
    if let Some(prefix) = namespace.filter(|p| !p.is_empty()) {
        return format!("{prefix}{leaf}");
    }
    let (mut nested, mut top) = (0usize, 0usize);
    let mut delimiter = None;
    for (name, d) in mailboxes {
        if name.eq_ignore_ascii_case(INBOX) {
            continue;
        }
        if under_inbox(name, d).is_some() {
            nested += 1;
            delimiter = d;
        } else {
            top += 1;
        }
    }
    match delimiter {
        Some(d) if nested > top => format!("{INBOX}{d}{leaf}"),
        _ => leaf.to_owned(),
    }
}

/// Where a moving op puts messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Inbox,
    Archive,
    Special(SpecialUse),
    Mailbox(MailboxRef),
}

impl Destination {
    pub fn matches(&self, mb: &MailboxView<'_>) -> bool {
        match self {
            Destination::Inbox => mb.role == Role::Inbox,
            Destination::Archive => mb.is_archive,
            Destination::Special(u) => mb.role == Role::Special(*u),
            Destination::Mailbox(r) => {
                if r.name.eq_ignore_ascii_case("INBOX") {
                    mb.role == Role::Inbox
                } else {
                    mb.name == r.name
                }
            }
        }
    }
}

impl Op {
    pub fn destination(&self) -> Option<Destination> {
        match self.kind {
            OpKind::Archive => Some(Destination::Archive),
            OpKind::Unarchive => Some(Destination::Inbox),
            OpKind::Move => self.mailbox.clone().map(Destination::Mailbox),
            OpKind::Trash => Some(Destination::Special(SpecialUse::Trash)),
            OpKind::Spam => Some(Destination::Special(SpecialUse::Junk)),
            _ => None,
        }
    }

    /// Whether this op takes the message out of `mb`: the locations the
    /// executor MOVEs (or expunges) and the projection hides.
    pub fn removes_from(&self, mb: &MailboxView<'_>) -> bool {
        if self.destination().is_some_and(|d| d.matches(mb)) {
            return false;
        }
        match self.kind {
            OpKind::Archive => mb.role == Role::Inbox,
            OpKind::Unarchive => mb.is_archive && mb.archive_mode == ArchiveMode::MoveArchive,
            OpKind::Move | OpKind::Trash | OpKind::Spam => is_filing(mb),
            OpKind::Delete => {
                matches!(mb.role, Role::Special(SpecialUse::Trash | SpecialUse::Junk))
            }
            _ => false,
        }
    }

    /// Whether the executor copies (rather than moves) from `mb`: Gmail-mode
    /// unarchive adds INBOX to a message that stays in `\All`.
    pub fn copies_from(&self, mb: &MailboxView<'_>) -> bool {
        self.kind == OpKind::Unarchive && mb.is_archive && mb.archive_mode == ArchiveMode::MoveAll
    }

    /// Applies a flag or keyword op to one location's state; moving ops
    /// leave flags alone.
    pub fn apply(&self, flags: &mut Flags, keywords: &mut KeywordSet) {
        match self.kind {
            OpKind::MarkRead => flags.insert(Flags::SEEN),
            OpKind::MarkUnread => flags.remove(Flags::SEEN),
            OpKind::Star => flags.insert(Flags::FLAGGED),
            OpKind::Unstar => flags.remove(Flags::FLAGGED),
            OpKind::LabelAdd => {
                if let Some(l) = &self.label
                    && !has_label(keywords, &l.key)
                {
                    keywords.insert(l.keyword.clone());
                }
            }
            OpKind::LabelRemove => {
                if let Some(l) = &self.label {
                    for k in label_spellings(keywords, &l.key) {
                        keywords.remove(&k);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Mailboxes a message is filed in: moving it elsewhere takes it out of
/// these, but never out of Sent, Drafts or virtual views.
fn is_filing(mb: &MailboxView<'_>) -> bool {
    match mb.role {
        Role::Inbox | Role::Folder => true,
        Role::Special(u) => matches!(
            u,
            SpecialUse::Archive | SpecialUse::Junk | SpecialUse::Trash
        ),
    }
}

fn has_label(keywords: &KeywordSet, key: &LabelKey) -> bool {
    keywords.iter().any(|k| is_label(k, key))
}

fn is_label(k: &Keyword, key: &LabelKey) -> bool {
    matches!(classify_keyword(k), KeywordClass::Label { key: ref found, .. } if found == key)
}

/// The keywords in `keywords` that spell the label `key` (our encoding in
/// any hex case, or another client's keyword with the same casefold).
pub fn label_spellings(keywords: &KeywordSet, key: &LabelKey) -> Vec<Keyword> {
    keywords
        .iter()
        .filter(|k| is_label(k, key))
        .cloned()
        .collect()
}

/// A location's state as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    pub flags: Flags,
    pub keywords: KeywordSet,
    /// A pending op takes the message out of this mailbox.
    pub hidden: bool,
}

/// The server state of one location with `ops` (oldest first) applied.
pub fn project<'o>(
    ops: impl IntoIterator<Item = &'o Op>,
    mb: &MailboxView<'_>,
    flags: Flags,
    keywords: KeywordSet,
) -> Projected {
    let mut out = Projected {
        flags,
        keywords,
        hidden: false,
    };
    for op in ops {
        op.apply(&mut out.flags, &mut out.keywords);
        if op.removes_from(mb) {
            out.hidden = true;
        }
    }
    out
}

/// The open ops of one account, oldest first: the pending overlay, rebuilt
/// from durable ops at startup and never stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overlay {
    ops: Vec<Op>,
}

impl Overlay {
    pub fn new(ops: Vec<Op>) -> Overlay {
        Overlay { ops }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Appends `op`, replacing an op with the same id in place.
    pub fn insert(&mut self, op: Op) {
        match self.ops.iter_mut().find(|o| o.id == op.id) {
            Some(existing) => *existing = op,
            None => self.ops.push(op),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Op> {
        let i = self.ops.iter().position(|o| o.id == id)?;
        Some(self.ops.remove(i))
    }

    /// The ops touching `email_id`, oldest first.
    pub fn ops_for<'a>(&'a self, email_id: &'a EmailId) -> impl Iterator<Item = &'a Op> + 'a {
        self.ops
            .iter()
            .filter(move |o| o.email_ids.contains(email_id))
    }

    pub fn touches(&self, email_id: &EmailId) -> bool {
        self.ops_for(email_id).next().is_some()
    }

    /// Every email id some op touches.
    pub fn email_ids(&self) -> BTreeSet<EmailId> {
        self.ops
            .iter()
            .flat_map(|o| o.email_ids.iter().cloned())
            .collect()
    }
}

/// A server mailbox the executor may pick for a [`MailboxRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub special_use: Option<SpecialUse>,
    /// Known from the cache or STATUS; `None` when unknown.
    pub uidvalidity: Option<u32>,
    pub delimiter: Option<char>,
}

/// Finds the mailbox a durable reference means today (plan §3.4): by
/// special-use (preferring the one whose name also matches), then by name,
/// then the only mailbox with the stored UIDVALIDITY (renamed by another
/// client). `None` means the op must fail visibly.
pub fn resolve_mailbox<'a>(r: &MailboxRef, candidates: &'a [Candidate]) -> Option<&'a Candidate> {
    let same_name = |c: &Candidate| {
        if r.name.eq_ignore_ascii_case("INBOX") {
            c.name.eq_ignore_ascii_case("INBOX")
        } else {
            c.name == r.name
        }
    };
    if let Some(u) = r.special_use {
        let by_use: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.special_use == Some(u))
            .collect();
        if let Some(c) = by_use
            .iter()
            .find(|c| same_name(c))
            .or_else(|| by_use.first())
        {
            return Some(c);
        }
    }
    if let Some(c) = candidates.iter().find(|c| same_name(c)) {
        return Some(c);
    }
    let mut by_validity = candidates
        .iter()
        .filter(|c| c.uidvalidity == Some(r.uidvalidity));
    match (by_validity.next(), by_validity.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// How the executor finds a message on the server when the cached UID
/// cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSearch {
    /// `UID SEARCH EMAILID <id>` (RFC 8474).
    EmailId(String),
    /// `UID SEARCH HEADER Message-ID <id>`, then recompute the email id of
    /// each candidate and keep the equal ones.
    MessageId(String),
    /// No Message-ID to search by: the message cannot be found again.
    Unsearchable,
}

/// The search for `email_id`, given the message's own Message-ID if known.
pub fn message_search(email_id: &EmailId, msgid: Option<&str>) -> MessageSearch {
    if let Some(object_id) = email_id.as_str().strip_prefix("o:") {
        return MessageSearch::EmailId(object_id.to_owned());
    }
    match msgid.filter(|m| !m.is_empty()) {
        Some(m) => MessageSearch::MessageId(m.to_owned()),
        None => MessageSearch::Unsearchable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{DEFAULT_KEYWORD_LIMIT, encode_label};

    fn kw(s: &str) -> Keyword {
        Keyword::new(s).unwrap()
    }

    fn keywords(list: &[&str]) -> KeywordSet {
        let mut set = KeywordSet::new();
        for k in list {
            set.insert(kw(k));
        }
        set
    }

    fn op(kind: OpKind) -> Op {
        Op {
            id: "op".into(),
            kind,
            email_ids: BTreeSet::new(),
            label: None,
            mailbox: None,
        }
    }

    fn label_op(kind: OpKind, name: &str) -> Op {
        Op {
            label: Some(LabelArg {
                keyword: encode_label(name, DEFAULT_KEYWORD_LIMIT).unwrap(),
                key: LabelKey::of(name),
            }),
            ..op(kind)
        }
    }

    fn view(name: &str, special_use: Option<SpecialUse>, mode: ArchiveMode) -> MailboxView<'_> {
        let role = Role::of(name, special_use);
        let is_archive = match mode {
            ArchiveMode::MoveAll => special_use == Some(SpecialUse::All),
            ArchiveMode::MoveArchive => special_use == Some(SpecialUse::Archive),
        };
        MailboxView {
            name,
            role,
            archive_mode: mode,
            is_archive,
        }
    }

    fn ids(list: &[&str]) -> BTreeSet<EmailId> {
        list.iter()
            .map(|s| EmailId::from_object_id(s).unwrap())
            .collect()
    }

    #[test]
    fn overlay_is_rebuilt_in_order_and_dropped_per_op() {
        let mut a = op(OpKind::MarkRead);
        a.id = "a".into();
        a.email_ids = ids(&["m1", "m2"]);
        let mut b = label_op(OpKind::LabelAdd, "x");
        b.id = "b".into();
        b.email_ids = ids(&["m2"]);
        let mut overlay = Overlay::new(vec![a.clone(), b.clone()]);
        let m2 = EmailId::from_object_id("m2").unwrap();
        let m3 = EmailId::from_object_id("m3").unwrap();
        let order: Vec<&str> = overlay.ops_for(&m2).map(|o| o.id.as_str()).collect();
        assert_eq!(order, ["a", "b"]);
        assert!(!overlay.touches(&m3));
        assert_eq!(overlay.email_ids(), ids(&["m1", "m2"]));

        overlay.insert(a.clone());
        assert_eq!(overlay.ops().len(), 2, "re-insert replaces");
        assert_eq!(overlay.remove("a").unwrap().id, "a");
        assert!(overlay.remove("a").is_none());
        assert_eq!(overlay.email_ids(), ids(&["m2"]));
    }

    #[test]
    fn kinds_and_states_round_trip() {
        for k in OpKind::ALL {
            assert_eq!(k.as_str().parse::<OpKind>().unwrap(), k);
        }
        assert!("label.rename".parse::<OpKind>().is_err());
        for s in ["pending", "running", "done", "failed"] {
            assert_eq!(s.parse::<OpState>().unwrap().as_str(), s);
        }
        assert_eq!(
            "move_all".parse::<ArchiveMode>().unwrap(),
            ArchiveMode::MoveAll
        );
    }

    #[test]
    fn overlay_projects_flags_and_labels_in_order() {
        let inbox = view("INBOX", None, ArchiveMode::MoveArchive);
        let ops = [
            op(OpKind::MarkRead),
            label_op(OpKind::LabelAdd, "Work Stuff"),
            op(OpKind::Star),
            op(OpKind::Unstar),
        ];
        let p = project(&ops, &inbox, Flags::empty(), keywords(&["$Forwarded"]));
        assert_eq!(p.flags, Flags::SEEN);
        assert_eq!(p.keywords, keywords(&["$Forwarded", "$label_Work=20Stuff"]));
        assert!(!p.hidden);

        let read_then_unread = [op(OpKind::MarkRead), op(OpKind::MarkUnread)];
        let p = project(&read_then_unread, &inbox, Flags::SEEN, KeywordSet::new());
        assert_eq!(p.flags, Flags::empty());
    }

    #[test]
    fn label_ops_match_every_spelling_of_the_label() {
        let inbox = view("INBOX", None, ArchiveMode::MoveArchive);
        let have = keywords(&["$label_work=20stuff", "Other", "$label_Keep"]);
        let add = [label_op(OpKind::LabelAdd, "Work Stuff")];
        assert_eq!(
            project(&add, &inbox, Flags::empty(), have.clone()).keywords,
            have,
            "already labelled in another hex case"
        );
        let remove = [label_op(OpKind::LabelRemove, "work stuff")];
        assert_eq!(
            project(&remove, &inbox, Flags::empty(), have).keywords,
            keywords(&["Other", "$label_Keep"])
        );
        let foreign = [label_op(OpKind::LabelRemove, "other")];
        assert_eq!(
            project(&foreign, &inbox, Flags::empty(), keywords(&["Other"])).keywords,
            KeywordSet::new()
        );
    }

    #[test]
    fn divergent_copies_project_independently() {
        let mode = ArchiveMode::MoveArchive;
        let inbox = view("INBOX", None, mode);
        let archive = view("Archive", Some(SpecialUse::Archive), mode);
        let ops = [label_op(OpKind::LabelRemove, "x"), op(OpKind::MarkRead)];
        let a = project(&ops, &inbox, Flags::empty(), keywords(&["$label_x"]));
        let b = project(&ops, &archive, Flags::SEEN, KeywordSet::new());
        assert!(a.keywords.is_empty() && b.keywords.is_empty());
        assert_eq!((a.flags, b.flags), (Flags::SEEN, Flags::SEEN));
    }

    #[test]
    fn moving_ops_hide_their_sources() {
        let mode = ArchiveMode::MoveArchive;
        let inbox = view("INBOX", None, mode);
        let archive = view("Archive", Some(SpecialUse::Archive), mode);
        let sent = view("Sent", Some(SpecialUse::Sent), mode);
        let trash = view("Trash", Some(SpecialUse::Trash), mode);
        let folder = view("Projects", None, mode);
        let hidden = |o: &Op, mb: &MailboxView<'_>| {
            project([o], mb, Flags::empty(), KeywordSet::new()).hidden
        };

        let archive_op = op(OpKind::Archive);
        assert!(hidden(&archive_op, &inbox));
        assert!(!hidden(&archive_op, &folder) && !hidden(&archive_op, &archive));

        let unarchive = op(OpKind::Unarchive);
        assert!(hidden(&unarchive, &archive) && !hidden(&unarchive, &inbox));

        let trash_op = op(OpKind::Trash);
        for mb in [&inbox, &archive, &folder] {
            assert!(hidden(&trash_op, mb), "{}", mb.name);
        }
        assert!(!hidden(&trash_op, &sent) && !hidden(&trash_op, &trash));

        let delete = op(OpKind::Delete);
        assert!(hidden(&delete, &trash) && !hidden(&delete, &inbox));

        let mut move_op = op(OpKind::Move);
        move_op.mailbox = Some(MailboxRef {
            name: "Projects".into(),
            special_use: None,
            uidvalidity: 7,
        });
        assert!(hidden(&move_op, &inbox) && !hidden(&move_op, &folder));
        assert!(!hidden(&move_op, &sent));
    }

    #[test]
    fn gmail_mode_archives_by_leaving_inbox_and_unarchives_by_copy() {
        let mode = ArchiveMode::MoveAll;
        let inbox = view("INBOX", None, mode);
        let all = view("[Gmail]/All", Some(SpecialUse::All), mode);
        let archive = op(OpKind::Archive);
        assert!(archive.removes_from(&inbox) && !archive.removes_from(&all));
        let unarchive = op(OpKind::Unarchive);
        assert!(!unarchive.removes_from(&all) && unarchive.copies_from(&all));
        assert!(
            !op(OpKind::Trash).removes_from(&all),
            "virtual \\All is never a source"
        );
    }

    #[test]
    fn archive_mailbox_follows_mode() {
        let boxes = [
            ("INBOX", None, Some('/')),
            ("Archive", None, Some('/')),
            ("Old", Some(SpecialUse::Archive), Some('/')),
            ("[Gmail]/All", Some(SpecialUse::All), Some('/')),
        ];
        assert_eq!(
            archive_mailbox(ArchiveMode::MoveArchive, boxes.iter().copied()),
            Some("Old")
        );
        assert_eq!(
            archive_mailbox(ArchiveMode::MoveAll, boxes.iter().copied()),
            Some("[Gmail]/All")
        );
        assert_eq!(
            archive_mailbox(ArchiveMode::MoveArchive, boxes[..2].iter().copied()),
            Some("Archive")
        );
        assert_eq!(
            archive_mailbox(ArchiveMode::MoveAll, boxes[..3].iter().copied()),
            None
        );
        assert_eq!(ArchiveMode::detect(None, true), ArchiveMode::MoveAll);
        assert_eq!(ArchiveMode::detect(None, false), ArchiveMode::MoveArchive);
        assert_eq!(
            ArchiveMode::detect(Some(ArchiveMode::MoveArchive), true),
            ArchiveMode::MoveArchive
        );
    }

    #[test]
    fn archive_mailbox_finds_the_folder_under_the_inbox_namespace() {
        let boxes = [
            ("INBOX", None, Some('.')),
            ("INBOX.Projects.Archive", None, Some('.')),
            ("INBOX.archive", None, Some('.')),
        ];
        assert_eq!(
            archive_mailbox(ArchiveMode::MoveArchive, boxes.iter().copied()),
            Some("INBOX.archive")
        );
    }

    #[test]
    fn folder_named_prefers_top_level_then_inbox_children() {
        let names = ["Trash", "Deleted Items"];
        let find =
            |boxes: &[(&'static str, Option<char>)]| folder_named(&names, boxes.iter().copied());
        assert_eq!(
            find(&[("INBOX.Trash", Some('.')), ("trash", Some('.'))]),
            Some("trash")
        );
        assert_eq!(
            find(&[("INBOX", Some('.')), ("INBOX.Deleted Items", Some('.'))]),
            Some("INBOX.Deleted Items")
        );
        assert_eq!(find(&[("Projects.Trash", Some('.'))]), None);
        assert_eq!(find(&[("INBOX.Projects.Trash", Some('.'))]), None);
        assert_eq!(find(&[("INBOX.Trash", None)]), None);
        assert_eq!(find(&[("INBOX/Trash", Some('.'))]), None);
    }

    #[test]
    fn new_folders_follow_the_inbox_namespace() {
        let dovecot = [
            ("INBOX", Some('.')),
            ("INBOX.Sent", Some('.')),
            ("INBOX.Drafts", Some('.')),
        ];
        assert_eq!(new_folder_name("Trash", None, dovecot), "INBOX.Trash");
        let flat = [
            ("INBOX", Some('/')),
            ("Sent", Some('/')),
            ("INBOX/Receipts", Some('/')),
            ("Drafts", Some('/')),
        ];
        assert_eq!(new_folder_name("Trash", None, flat), "Trash");
        assert_eq!(
            new_folder_name("Junk", None, [("INBOX", Some('/'))]),
            "Junk"
        );
    }

    #[test]
    fn new_folders_go_into_the_personal_namespace() {
        let inbox_only = [("INBOX", Some('.'))];
        assert_eq!(
            new_folder_name("Trash", Some("INBOX."), inbox_only),
            "INBOX.Trash"
        );
        let flat = [("INBOX", Some('/')), ("Sent", Some('/'))];
        assert_eq!(new_folder_name("Trash", Some(""), flat), "Trash");
    }

    fn cand(name: &str, special_use: Option<SpecialUse>, uidvalidity: Option<u32>) -> Candidate {
        Candidate {
            name: name.into(),
            special_use,
            uidvalidity,
            delimiter: Some('/'),
        }
    }

    #[test]
    fn mailbox_resolution_order() {
        let r = MailboxRef {
            name: "Bin".into(),
            special_use: Some(SpecialUse::Trash),
            uidvalidity: 5,
        };
        let both = [
            cand("Deleted", Some(SpecialUse::Trash), Some(1)),
            cand("Bin", Some(SpecialUse::Trash), Some(2)),
        ];
        assert_eq!(resolve_mailbox(&r, &both).unwrap().name, "Bin");
        let renamed = [
            cand("Bin", None, Some(9)),
            cand("Deleted", Some(SpecialUse::Trash), Some(1)),
        ];
        assert_eq!(
            resolve_mailbox(&r, &renamed).unwrap().name,
            "Deleted",
            "special-use beats name"
        );

        let folder = MailboxRef {
            name: "Projects".into(),
            special_use: None,
            uidvalidity: 42,
        };
        let by_name = [cand("Projects", None, Some(1)), cand("P2", None, Some(42))];
        assert_eq!(resolve_mailbox(&folder, &by_name).unwrap().name, "Projects");
        let moved = [cand("Work/Projects", None, Some(42)), cand("x", None, None)];
        assert_eq!(
            resolve_mailbox(&folder, &moved).unwrap().name,
            "Work/Projects",
            "unique uidvalidity finds a renamed folder"
        );
        let ambiguous = [cand("a", None, Some(42)), cand("b", None, Some(42))];
        assert_eq!(resolve_mailbox(&folder, &ambiguous), None);
        assert_eq!(resolve_mailbox(&folder, &[cand("a", None, Some(1))]), None);

        let inbox = MailboxRef {
            name: "INBOX".into(),
            special_use: None,
            uidvalidity: 1,
        };
        assert_eq!(
            resolve_mailbox(&inbox, &[cand("Inbox", None, Some(3))])
                .unwrap()
                .name,
            "Inbox"
        );
    }

    #[test]
    fn message_search_prefers_email_id() {
        let object = EmailId::from_object_id("M123").unwrap();
        assert_eq!(
            message_search(&object, Some("a@b")),
            MessageSearch::EmailId("M123".into())
        );
        let hash = EmailId::parse(&format!("h:{}", "0".repeat(32))).unwrap();
        assert_eq!(
            message_search(&hash, Some("a@b")),
            MessageSearch::MessageId("a@b".into())
        );
        assert_eq!(message_search(&hash, None), MessageSearch::Unsearchable);
        let gmail = EmailId::from_gmail_msgid(7);
        assert_eq!(
            message_search(&gmail, Some("g@x")),
            MessageSearch::MessageId("g@x".into())
        );
    }
}
