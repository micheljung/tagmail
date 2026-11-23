//! An in-memory IMAP server behind [`ImapSession`] for engine tests. Tests
//! play the "second client" by changing [`FakeServer`] directly; sessions
//! see the changes like a real server's would, per capability set.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tagmail_core::SpecialUse;
use tagmail_imap::{
    Address, AppendResult, BodyFetch, BodySummary, Capabilities, Changes, Connector, Envelope,
    Error, FetchSpec, Flag, IdleResult, IdleStop, ImapSession, MailboxEvent, MailboxInfo,
    MailboxStatus, MessageMeta, ModSeq, MoveResult, PartInfo, Result, SearchCriteria,
    SelectOptions, SelectedMailbox, StoreOp, StoreResult, Uid, UidSet, UidValidity,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

use crate::supervisor::{AccountSettings, ConnectorFactory};

#[derive(Debug, Clone)]
pub struct FakeMsg {
    pub id: String,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: i64,
    pub refs: Vec<String>,
    pub body: String,
    pub flags: Vec<Flag>,
    /// The message as appended by a client; `raw()` synthesizes one
    /// otherwise.
    pub appended: Option<Vec<u8>>,
    modseq: u64,
}

impl FakeMsg {
    pub fn new(id: &str, subject: &str, from: &str, to: &str, date: i64) -> FakeMsg {
        FakeMsg {
            id: id.to_owned(),
            subject: subject.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            date,
            refs: Vec::new(),
            body: format!("Body of {id}"),
            flags: Vec::new(),
            appended: None,
            modseq: 0,
        }
    }

    /// A message a client APPENDed, parsed like a server would.
    pub fn parsed(raw: &[u8], flags: &[Flag]) -> FakeMsg {
        let m = mail_parser::MessageParser::new()
            .parse(raw)
            .expect("parsable message");
        let first = |list: Option<mail_parser::AddressList<'_>>| {
            list.and_then(|a| a.first())
                .and_then(|a| a.address())
                .unwrap_or_default()
                .to_owned()
        };
        let mut refs: Vec<String> = m
            .references()
            .map(|l| l.iter().map(str::to_owned).collect())
            .unwrap_or_default();
        if let Some(parent) = m.in_reply_to().and_then(|l| l.last())
            && !refs.iter().any(|r| r == parent)
        {
            refs.push(parent.to_owned());
        }
        FakeMsg {
            id: m.message_id().unwrap_or_default().to_owned(),
            subject: m.subject().unwrap_or_default().to_owned(),
            from: first(m.from()),
            to: first(m.to()),
            date: m.date().map_or(0, |d| d.to_timestamp()),
            refs,
            body: m.body_text(0).unwrap_or_default().into_owned(),
            flags: flags.to_vec(),
            appended: Some(raw.to_vec()),
            modseq: 0,
        }
    }

    pub fn refs(mut self, refs: &[&str]) -> FakeMsg {
        self.refs = refs.iter().map(|r| (*r).to_owned()).collect();
        self
    }

    pub fn flags(mut self, flags: &[Flag]) -> FakeMsg {
        self.flags = flags.to_vec();
        self
    }

    /// The whole message as `BODY[]` returns it.
    pub fn raw(&self) -> String {
        if let Some(raw) = &self.appended {
            return String::from_utf8_lossy(raw).into_owned();
        }
        format!(
            "Message-ID: <{}>\r\nSubject: {}\r\nFrom: {}\r\nTo: {}\r\n\r\n{}",
            self.id, self.subject, self.from, self.to, self.body
        )
    }

    fn address(email: &str) -> Address {
        let (mailbox, host) = email.split_once('@').unwrap_or((email, ""));
        Address {
            name: None,
            mailbox: Some(mailbox.as_bytes().to_vec()),
            host: Some(host.as_bytes().to_vec()),
        }
    }

    fn date_header(&self) -> String {
        mail_parser::DateTime::from_timestamp(self.date).to_rfc822()
    }

    fn part(&self) -> PartInfo {
        PartInfo {
            section: "1".into(),
            mime_type: "text/plain".into(),
            charset: Some("utf-8".into()),
            encoding: "7bit".into(),
            size: u32::try_from(self.body.len()).unwrap_or(u32::MAX),
            disposition: None,
            filename: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeMailbox {
    pub name: String,
    pub special_use: Option<SpecialUse>,
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub modseq: u64,
    pub nomodseq: bool,
    pub msgs: BTreeMap<u32, FakeMsg>,
    expunged: Vec<(u32, u64)>,
}

/// One FETCH the engine sent: mailbox, UIDs, whether it asked for envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchLog {
    pub mailbox: String,
    pub uids: Vec<u32>,
    pub full: bool,
}

pub struct FakeServer {
    pub caps: Vec<String>,
    pub mailboxes: Vec<FakeMailbox>,
    pub fetches: Vec<FetchLog>,
    /// Full-window FETCHes that still succeed before every further one
    /// fails like a dropped connection.
    pub fail_after_full_fetches: Option<usize>,
    pub reject_login: bool,
    /// APPENDs fail with NO.
    pub fail_appends: bool,
    /// STOREs fail with NO.
    pub fail_stores: bool,
    /// STOREs answer OK but keep no keywords, like Stalwart past its
    /// per-account keyword limit.
    pub drop_keywords: bool,
    /// SELECT reports whether keywords can be stored.
    pub keyword_capable: bool,
    /// UIDs another client changes just before the next STORE, so an
    /// `UNCHANGEDSINCE` guard reports them MODIFIED; consumed by that STORE.
    pub change_before_store: Vec<u32>,
    /// `UID SEARCH HEADER` finds nothing, like Stalwart with more than one
    /// message in the mailbox.
    pub header_search_empty: bool,
    /// Every STORE: mailbox and the UIDs asked for.
    pub stores: Vec<FetchLog>,
    /// Server annotations; `Some` when METADATA is advertised.
    pub metadata: BTreeMap<String, Vec<u8>>,
    /// Counts full-window FETCHes in flight, each held for the delay; one
    /// gauge may be shared by the servers of several accounts.
    pub full_fetch_gauge: Option<(Arc<Gauge>, Duration)>,
    /// The password a login must present; any when `None`.
    pub password: Option<String>,
    pub logins: usize,
    changed: Arc<Notify>,
}

pub type Shared = Arc<Mutex<FakeServer>>;

/// How many of something run at once, and the most ever.
#[derive(Debug, Default)]
pub struct Gauge {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Gauge {
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn enter(&self) -> GaugeGuard<'_> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        GaugeGuard(self)
    }
}

struct GaugeGuard<'a>(&'a Gauge);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn lock(server: &Shared) -> MutexGuard<'_, FakeServer> {
    server.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A server with INBOX, Sent and Archive and the given capabilities.
pub fn server(caps: &[&str]) -> Shared {
    let mailbox = |name: &str, special_use, uidvalidity| FakeMailbox {
        name: name.into(),
        special_use,
        uidvalidity,
        uidnext: 1,
        modseq: 1,
        nomodseq: false,
        msgs: BTreeMap::new(),
        expunged: Vec::new(),
    };
    Arc::new(Mutex::new(FakeServer {
        caps: caps.iter().map(|c| (*c).to_owned()).collect(),
        mailboxes: vec![
            mailbox("INBOX", None, 100),
            mailbox("Sent", Some(SpecialUse::Sent), 200),
            mailbox("Archive", Some(SpecialUse::Archive), 300),
        ],
        fetches: Vec::new(),
        fail_after_full_fetches: None,
        reject_login: false,
        fail_appends: false,
        fail_stores: false,
        drop_keywords: false,
        keyword_capable: true,
        change_before_store: Vec::new(),
        header_search_empty: false,
        stores: Vec::new(),
        metadata: BTreeMap::new(),
        full_fetch_gauge: None,
        password: None,
        logins: 0,
        changed: Arc::new(Notify::new()),
    }))
}

impl FakeServer {
    pub fn add_mailbox(&mut self, name: &str, special_use: Option<SpecialUse>, uidvalidity: u32) {
        self.mailboxes.push(FakeMailbox {
            name: name.into(),
            special_use,
            uidvalidity,
            uidnext: 1,
            modseq: 1,
            nomodseq: false,
            msgs: BTreeMap::new(),
            expunged: Vec::new(),
        });
    }

    pub fn mailbox(&mut self, name: &str) -> &mut FakeMailbox {
        self.mailboxes
            .iter_mut()
            .find(|m| m.name == name)
            .expect("fake mailbox")
    }

    fn bump(&mut self, name: &str) -> u64 {
        let mb = self.mailbox(name);
        mb.modseq += 1;
        mb.modseq
    }

    fn notify(&self, name: &str) {
        if name == "INBOX" {
            self.changed.notify_one();
        }
    }

    pub fn append(&mut self, name: &str, mut msg: FakeMsg) -> u32 {
        msg.modseq = self.bump(name);
        let mb = self.mailbox(name);
        let uid = mb.uidnext;
        mb.uidnext += 1;
        mb.msgs.insert(uid, msg);
        self.notify(name);
        uid
    }

    /// Copies `uid` of `from` into `to`, returning the new UID.
    pub fn copy_msg(&mut self, from: &str, uid: u32, to: &str) -> u32 {
        let msg = self.mailbox(from).msgs.get(&uid).expect("uid").clone();
        self.append(to, msg)
    }

    /// Flags of `uid` in `name` as IMAP strings, sorted.
    pub fn flags_of(&mut self, name: &str, uid: u32) -> Vec<String> {
        let mut out: Vec<String> = self.mailbox(name).msgs[&uid]
            .flags
            .iter()
            .map(|f| f.as_imap().to_owned())
            .collect();
        out.sort();
        out
    }

    /// UID of the message with Message-ID `id` in `name`.
    pub fn uid_of(&mut self, name: &str, id: &str) -> Option<u32> {
        self.mailbox(name)
            .msgs
            .iter()
            .find(|(_, m)| m.id == id)
            .map(|(u, _)| *u)
    }

    pub fn set_flags(&mut self, name: &str, uid: u32, flags: &[Flag]) {
        let modseq = self.bump(name);
        let msg = self.mailbox(name).msgs.get_mut(&uid).expect("uid");
        msg.flags = flags.to_vec();
        msg.modseq = modseq;
        self.notify(name);
    }

    pub fn expunge(&mut self, name: &str, uid: u32) -> FakeMsg {
        let modseq = self.bump(name);
        let mb = self.mailbox(name);
        let msg = mb.msgs.remove(&uid).expect("uid");
        mb.expunged.push((uid, modseq));
        self.notify(name);
        msg
    }

    pub fn move_msg(&mut self, from: &str, uid: u32, to: &str) -> u32 {
        let msg = self.expunge(from, uid);
        self.append(to, msg)
    }

    /// New UIDVALIDITY and UIDs, as after a server-side mailbox rebuild.
    pub fn renumber(&mut self, name: &str) {
        let modseq = self.bump(name);
        let mb = self.mailbox(name);
        mb.uidvalidity += 1;
        let msgs: Vec<FakeMsg> = std::mem::take(&mut mb.msgs).into_values().collect();
        mb.expunged.clear();
        mb.uidnext = 1;
        for mut m in msgs {
            m.modseq = modseq;
            let uid = mb.uidnext;
            mb.uidnext += 1;
            mb.msgs.insert(uid, m);
        }
    }

    /// UIDs fetched with envelopes from `mailbox`.
    pub fn full_fetched(&self, mailbox: &str) -> Vec<u32> {
        self.fetches
            .iter()
            .filter(|f| f.full && f.mailbox == mailbox)
            .flat_map(|f| f.uids.iter().copied())
            .collect()
    }
}

pub struct FakeSession {
    server: Shared,
    caps: Capabilities,
    selected: Option<String>,
    broken: bool,
    changed: Arc<Notify>,
}

fn closed() -> Error {
    Error::Closed { bye: None }
}

impl FakeSession {
    fn server(&self) -> MutexGuard<'_, FakeServer> {
        lock(&self.server)
    }

    fn selected_name(&self) -> Result<String> {
        self.selected.clone().ok_or(Error::NoMailboxSelected)
    }

    fn fetch_now(&mut self, uids: &UidSet, spec: &FetchSpec) -> Result<Vec<MessageMeta>> {
        let name = self.selected_name()?;

        let mut server = self.server();
        if spec.envelope {
            let full = server.fetches.iter().filter(|f| f.full).count();
            if server.fail_after_full_fetches.is_some_and(|n| full >= n) {
                drop(server);
                self.broken = true;
                return Err(closed());
            }
        }
        let mb = server.mailbox(&name).clone();
        let out: Vec<MessageMeta> = uids
            .iter()
            .filter_map(|u| mb.msgs.get(&u.0).map(|m| (u.0, m)))
            .map(|(uid, m)| self.meta(&mb, uid, m, spec))
            .collect();
        let fetched: Vec<u32> = out.iter().filter_map(|m| m.uid.map(|u| u.0)).collect();
        server.fetches.push(FetchLog {
            mailbox: name,
            uids: fetched,
            full: spec.envelope,
        });
        Ok(out)
    }

    fn modseq_for(&self, mb: &FakeMailbox) -> Option<ModSeq> {
        (self.caps.condstore() && !mb.nomodseq).then_some(ModSeq(mb.modseq))
    }

    fn meta(&self, mb: &FakeMailbox, uid: u32, m: &FakeMsg, spec: &FetchSpec) -> MessageMeta {
        let (full, snippet) = (spec.envelope, spec.snippet);
        let mut meta = MessageMeta {
            uid: Some(Uid(uid)),
            flags: Some(m.flags.clone()),
            modseq: self.modseq_for(mb).map(|_| ModSeq(m.modseq)),
            ..MessageMeta::default()
        };
        if spec.body_structure {
            meta.body = Some(BodySummary {
                parts: vec![m.part()],
            });
        }
        if !spec.header_fields.is_empty() {
            let mut lines = String::new();
            for field in &spec.header_fields {
                if field.eq_ignore_ascii_case("REFERENCES") {
                    let refs: Vec<String> = m.refs.iter().map(|r| format!("<{r}>")).collect();
                    lines.push_str(&format!("References: {}\r\n", refs.join(" ")));
                } else if field.eq_ignore_ascii_case("MESSAGE-ID") {
                    lines.push_str(&format!("Message-ID: <{}>\r\n", m.id));
                }
            }
            lines.push_str("\r\n");
            meta.header_fields = Some(lines.into_bytes());
        }
        if full {
            meta.internal_date = Some(m.date);
            meta.size = Some(u32::try_from(m.body.len() + 200).unwrap_or(u32::MAX));
            meta.envelope = Some(Envelope {
                date: Some(m.date_header().into_bytes()),
                subject: Some(m.subject.clone().into_bytes()),
                from: vec![FakeMsg::address(&m.from)],
                to: vec![FakeMsg::address(&m.to)],
                in_reply_to: m.refs.last().map(|r| format!("<{r}>").into_bytes()),
                message_id: Some(format!("<{}>", m.id).into_bytes()),
                ..Envelope::default()
            });
            meta.snippet = snippet.map(|n| {
                let n = usize::try_from(n).unwrap_or(usize::MAX).min(m.body.len());
                m.body.as_bytes()[..n].to_vec()
            });
            if self.caps.objectid() {
                meta.email_id = Some(format!("E{}", m.id.replace(['@', '.'], "_")));
            }
        }
        meta
    }
}

impl ImapSession for FakeSession {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn qresync_enabled(&self) -> bool {
        self.caps.qresync()
    }

    fn is_usable(&self) -> bool {
        !self.broken
    }

    fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    async fn list(&mut self) -> Result<Vec<MailboxInfo>> {
        Ok(self
            .server()
            .mailboxes
            .iter()
            .map(|m| MailboxInfo {
                name: m.name.clone(),
                delimiter: Some('/'),
                attributes: Vec::new(),
                special_use: m.special_use,
                selectable: true,
            })
            .collect())
    }

    async fn create_mailbox(&mut self, name: &str) -> Result<()> {
        let mut server = self.server();
        if server.mailboxes.iter().any(|m| m.name == name) {
            return Err(Error::No {
                code: Some("ALREADYEXISTS".into()),
                text: name.into(),
            });
        }
        let uidvalidity = 1000 + u32::try_from(server.mailboxes.len()).unwrap_or(0);
        server.add_mailbox(name, None, uidvalidity);
        Ok(())
    }

    async fn select(&mut self, name: &str, options: SelectOptions) -> Result<SelectedMailbox> {
        let server = lock(&self.server);
        let mb = server
            .mailboxes
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| Error::No {
                code: Some("NONEXISTENT".into()),
                text: name.into(),
            })?;
        let mut out = SelectedMailbox {
            name: name.into(),
            read_only: options.read_only,
            uid_validity: UidValidity(mb.uidvalidity),
            uid_next: Some(Uid(mb.uidnext)),
            exists: u32::try_from(mb.msgs.len()).unwrap_or(u32::MAX),
            highest_modseq: self.modseq_for(mb),
            nomodseq: mb.nomodseq,
            flags: Vec::new(),
            permanent_flags: Vec::new(),
            keyword_capable: server.keyword_capable,
            vanished: UidSet::new(),
            changed: Vec::new(),
        };
        if let Some(q) = options.qresync.filter(|_| self.caps.qresync())
            && q.uid_validity.0 == mb.uidvalidity
        {
            out.vanished = mb
                .expunged
                .iter()
                .filter(|(_, m)| *m > q.modseq.0)
                .map(|(u, _)| Uid(*u))
                .collect();
            out.changed = mb
                .msgs
                .iter()
                .filter(|(_, m)| m.modseq > q.modseq.0)
                .map(|(uid, m)| self.meta(mb, *uid, m, &FetchSpec::flags()))
                .collect();
        }
        drop(server);
        self.selected = Some(name.into());
        Ok(out)
    }

    async fn status(&mut self, name: &str) -> Result<MailboxStatus> {
        let mut server = self.server();
        let mb = server.mailbox(name).clone();
        drop(server);
        Ok(MailboxStatus {
            messages: Some(u32::try_from(mb.msgs.len()).unwrap_or(u32::MAX)),
            uid_next: Some(Uid(mb.uidnext)),
            uid_validity: Some(UidValidity(mb.uidvalidity)),
            unseen: None,
            highest_modseq: self.modseq_for(&mb),
        })
    }

    async fn uid_search(&mut self, criteria: SearchCriteria) -> Result<UidSet> {
        let name = self.selected_name()?;
        let mut server = self.server();
        let header_search_empty = server.header_search_empty;
        let msgs = &server.mailbox(&name).msgs;
        let matches = |m: &FakeMsg| match &criteria {
            SearchCriteria::All => true,
            SearchCriteria::Deleted => m.flags.contains(&Flag::Deleted),
            SearchCriteria::Header { name, value } => {
                assert!(name.eq_ignore_ascii_case("Message-ID"), "header {name}");
                !header_search_empty && value.contains(&m.id)
            }
            SearchCriteria::EmailId(_) => panic!("EMAILID search"),
        };
        Ok(msgs
            .iter()
            .filter(|(_, m)| matches(m))
            .map(|(u, _)| Uid(*u))
            .collect())
    }

    async fn fetch(&mut self, uids: &UidSet, spec: &FetchSpec) -> Result<Vec<MessageMeta>> {
        let gauge = spec
            .envelope
            .then(|| self.server().full_fetch_gauge.clone())
            .flatten();
        let Some((gauge, delay)) = gauge else {
            return self.fetch_now(uids, spec);
        };
        let _busy = gauge.enter();
        tokio::time::sleep(delay).await;
        self.fetch_now(uids, spec)
    }

    async fn fetch_changed_since(
        &mut self,
        uids: &UidSet,
        since: ModSeq,
        spec: &FetchSpec,
    ) -> Result<Changes> {
        assert!(self.caps.condstore());
        let name = self.selected_name()?;
        let mut server = self.server();
        let mb = server.mailbox(&name).clone();
        drop(server);
        let messages = mb
            .msgs
            .iter()
            .filter(|(uid, m)| uids.contains(Uid(**uid)) && m.modseq > since.0)
            .map(|(uid, m)| self.meta(&mb, *uid, m, spec))
            .collect();
        let vanished = if self.caps.qresync() {
            mb.expunged
                .iter()
                .filter(|(u, m)| *m > since.0 && uids.contains(Uid(*u)))
                .map(|(u, _)| Uid(*u))
                .collect()
        } else {
            UidSet::new()
        };
        Ok(Changes { messages, vanished })
    }

    async fn fetch_body(
        &mut self,
        uid: Uid,
        section: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        max_bytes: u64,
    ) -> Result<BodyFetch> {
        let name = self.selected_name()?;
        let msg = {
            let mut server = self.server();
            let msg = server.mailbox(&name).msgs.get(&uid.0).cloned();
            msg.ok_or(Error::NotFound)?
        };
        let body = match section {
            "1" => msg.body,
            "" => msg.raw(),
            other => panic!("unexpected section {other:?}"),
        };
        let n = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(body.len());
        sink.write_all(&body.as_bytes()[..n]).await?;
        Ok(BodyFetch {
            bytes: n as u64,
            truncated: n < body.len(),
        })
    }

    async fn store_flags(
        &mut self,
        uids: &UidSet,
        op: StoreOp,
        flags: &[Flag],
        unchanged_since: Option<ModSeq>,
    ) -> Result<StoreResult> {
        let name = self.selected_name()?;
        let condstore = self.caps.condstore();
        let mut server = self.server();
        if server.fail_stores {
            return Err(Error::No {
                code: None,
                text: "store refused".into(),
            });
        }
        let targets: Vec<u32> = server
            .mailbox(&name)
            .msgs
            .keys()
            .copied()
            .filter(|u| uids.contains(Uid(*u)))
            .collect();
        server.stores.push(FetchLog {
            mailbox: name.clone(),
            uids: targets.clone(),
            full: false,
        });
        for uid in std::mem::take(&mut server.change_before_store) {
            let flags = server.mailbox(&name).msgs[&uid].flags.clone();
            server.set_flags(&name, uid, &flags);
        }
        let mut modified = UidSet::new();
        let mut touched = false;
        for uid in targets {
            let msg_modseq = server.mailbox(&name).msgs[&uid].modseq;
            if let Some(since) = unchanged_since.filter(|_| condstore)
                && msg_modseq > since.0
            {
                modified.insert(Uid(uid));
                continue;
            }
            let mut new = server.mailbox(&name).msgs[&uid].flags.clone();
            match op {
                StoreOp::Add => {
                    for f in flags {
                        let dropped = server.drop_keywords && matches!(f, Flag::Keyword(_));
                        if !dropped && !new.contains(f) {
                            new.push(f.clone());
                        }
                    }
                }
                StoreOp::Remove => new.retain(|f| !flags.contains(f)),
                StoreOp::Replace => new = flags.to_vec(),
            }
            if new != server.mailbox(&name).msgs[&uid].flags {
                let modseq = server.bump(&name);
                let msg = server.mailbox(&name).msgs.get_mut(&uid).expect("uid");
                msg.flags = new;
                msg.modseq = modseq;
                touched = true;
            }
        }
        if touched {
            server.notify(&name);
        }
        Ok(StoreResult {
            modified,
            updates: Vec::new(),
        })
    }

    async fn move_messages(&mut self, uids: &UidSet, destination: &str) -> Result<MoveResult> {
        let name = self.selected_name()?;
        let mut server = self.server();
        if !server.mailboxes.iter().any(|m| m.name == destination) {
            return Err(Error::No {
                code: Some("TRYCREATE".into()),
                text: destination.into(),
            });
        }
        let targets: Vec<u32> = server
            .mailbox(&name)
            .msgs
            .keys()
            .copied()
            .filter(|u| uids.contains(Uid(*u)))
            .collect();
        for uid in targets {
            server.move_msg(&name, uid, destination);
        }
        Ok(MoveResult::default())
    }

    async fn copy_messages(&mut self, uids: &UidSet, destination: &str) -> Result<MoveResult> {
        let name = self.selected_name()?;
        let mut server = self.server();
        if !server.mailboxes.iter().any(|m| m.name == destination) {
            return Err(Error::No {
                code: Some("TRYCREATE".into()),
                text: destination.into(),
            });
        }
        let targets: Vec<u32> = server
            .mailbox(&name)
            .msgs
            .keys()
            .copied()
            .filter(|u| uids.contains(Uid(*u)))
            .collect();
        for uid in targets {
            server.copy_msg(&name, uid, destination);
        }
        Ok(MoveResult::default())
    }

    async fn get_metadata(&mut self, entry: &str) -> Result<Option<Vec<u8>>> {
        if !self.caps.metadata() {
            return Err(Error::Unsupported("METADATA"));
        }
        Ok(self.server().metadata.get(entry).cloned())
    }

    async fn set_metadata(&mut self, entry: &str, value: Option<&[u8]>) -> Result<()> {
        if !self.caps.metadata() {
            return Err(Error::Unsupported("METADATA"));
        }
        let mut server = self.server();
        match value {
            Some(v) => server.metadata.insert(entry.to_owned(), v.to_vec()),
            None => server.metadata.remove(entry),
        };
        Ok(())
    }

    async fn append(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> Result<AppendResult> {
        let mut server = self.server();
        if server.fail_appends {
            return Err(Error::No {
                code: None,
                text: "append refused".into(),
            });
        }
        let uid = server.append(mailbox, FakeMsg::parsed(message, flags));
        let uid_validity = server.mailbox(mailbox).uidvalidity;
        Ok(AppendResult {
            uid: Some(Uid(uid)),
            uid_validity: Some(UidValidity(uid_validity)),
        })
    }

    async fn append_stream(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        size: u64,
        message: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> Result<AppendResult> {
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(
            &mut tokio::io::AsyncReadExt::take(message, size),
            &mut bytes,
        )
        .await?;
        self.append(mailbox, flags, &bytes).await
    }

    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()> {
        let name = self.selected_name()?;
        let mut server = self.server();
        let present: Vec<u32> = server
            .mailbox(&name)
            .msgs
            .keys()
            .copied()
            .filter(|u| uids.contains(Uid(*u)))
            .collect();
        for uid in present {
            server.expunge(&name, uid);
        }
        Ok(())
    }

    async fn idle(&mut self, wake: &Notify) -> Result<IdleResult> {
        let changed = self.changed.clone();
        let stop = tokio::select! {
            () = changed.notified() => IdleStop::Changed,
            () = wake.notified() => IdleStop::Woken,
        };
        Ok(IdleResult {
            stop,
            events: Vec::new(),
        })
    }

    async fn noop(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_events(&mut self) -> Vec<MailboxEvent> {
        Vec::new()
    }

    async fn logout(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Opens [`FakeSession`]s on one shared server.
#[derive(Clone)]
pub struct FakeConnector(pub Shared);

impl FakeConnector {
    pub fn session(&self) -> FakeSession {
        let server = lock(&self.0);
        FakeSession {
            caps: server.caps.iter().collect(),
            changed: server.changed.clone(),
            server: self.0.clone(),
            selected: None,
            broken: false,
        }
    }
}

impl Connector for FakeConnector {
    type Session = FakeSession;

    async fn connect(&self) -> Result<FakeSession> {
        {
            let mut server = lock(&self.0);
            server.logins += 1;
            if server.reject_login {
                return Err(Error::Auth("invalid credentials".into()));
            }
        }
        Ok(self.session())
    }
}

impl ConnectorFactory for FakeConnector {
    type Connector = FakeConnector;

    fn connector(&self, _account: &AccountSettings) -> FakeConnector {
        self.clone()
    }
}

/// One fake server per account, by IMAP username; a login must present
/// the server's [`FakeServer::password`] when it has one.
#[derive(Clone, Default)]
pub struct FakeAccounts(pub Arc<Mutex<BTreeMap<String, Shared>>>);

impl FakeAccounts {
    pub fn add(&self, username: &str, server: Shared) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(username.to_owned(), server);
    }
}

pub struct FakeLogin {
    connector: FakeConnector,
    password: String,
}

impl Connector for FakeLogin {
    type Session = FakeSession;

    async fn connect(&self) -> Result<FakeSession> {
        let expected = lock(&self.connector.0).password.clone();
        if expected.is_some_and(|p| p != self.password) {
            lock(&self.connector.0).logins += 1;
            return Err(Error::Auth("wrong password".into()));
        }
        self.connector.connect().await
    }
}

impl ConnectorFactory for FakeAccounts {
    type Connector = FakeLogin;

    fn connector(&self, account: &AccountSettings) -> FakeLogin {
        let server = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&account.username)
            .cloned()
            .expect("a fake server for the account");
        FakeLogin {
            connector: FakeConnector(server),
            password: account.password.expose().to_owned(),
        }
    }
}
