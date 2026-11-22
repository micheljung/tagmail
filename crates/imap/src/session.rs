//! The [`ImapSession`] trait and its tokio implementation on imap-next.

use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use imap_next::imap_types::IntoStatic;
use imap_next::imap_types::auth::AuthMechanism;
use imap_next::imap_types::command::{CommandBody, SelectParameter};
use imap_next::imap_types::core::{AString, Atom, Vec1};
use imap_next::imap_types::extensions::enable::CapabilityEnable;
use imap_next::imap_types::flag::{Flag as ImapFlag, FlagNameAttribute, FlagPerm};
use imap_next::imap_types::mailbox::Mailbox;
use imap_next::imap_types::response::{Code, Data, GreetingKind, StatusKind};
use imap_next::imap_types::search::SearchKey;
use imap_next::imap_types::secret::Secret;
use imap_next::imap_types::sequence::SequenceSet;
use imap_next::imap_types::status::{StatusDataItem, StatusDataItemName};
use rustls::ClientConfig;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::TlsConnector;

use crate::caps::{Capabilities, CapabilityOverride, FetchSpec};
use crate::conn::{Collected, Conn, Done, Io};
use crate::error::{Error, Result};
use crate::fetch;
use crate::qresync;
use crate::tls::{self, Security};
use crate::types::{
    AppendResult, BodyFetch, Changes, Flag, IdleResult, MailboxEvent, MailboxInfo, MailboxStatus,
    MessageMeta, ModSeq, MoveResult, SearchCriteria, SelectOptions, SelectedMailbox, StoreOp,
    StoreResult, Uid, UidSet, UidValidity,
};
use tagmail_core::SpecialUse;

/// The IMAP operations the sync engine needs. Implemented over imap-next by
/// [`TokioSession`]; tests can substitute scripted sessions.
///
/// Commands run one at a time. Changes the server reports outside a
/// command's own result (EXISTS, EXPUNGE, VANISHED, flag FETCHes) queue up
/// until [`ImapSession::take_events`].
pub trait ImapSession: Send {
    fn capabilities(&self) -> &Capabilities;

    /// `ENABLE QRESYNC` succeeded, so SELECT accepts QRESYNC parameters and
    /// expunges arrive as VANISHED.
    fn qresync_enabled(&self) -> bool;

    /// The session can still run commands.
    fn is_usable(&self) -> bool;

    fn selected(&self) -> Option<&str>;

    /// All mailboxes with their special-use attributes.
    fn list(&mut self) -> impl Future<Output = Result<Vec<MailboxInfo>>> + Send;

    fn create_mailbox(&mut self, name: &str) -> impl Future<Output = Result<()>> + Send;

    /// SELECT or EXAMINE. With QRESYNC parameters the result carries
    /// `VANISHED (EARLIER)` UIDs and the messages changed since the modseq.
    fn select(
        &mut self,
        name: &str,
        options: SelectOptions,
    ) -> impl Future<Output = Result<SelectedMailbox>> + Send;

    fn status(&mut self, name: &str) -> impl Future<Output = Result<MailboxStatus>> + Send;

    /// Plain `UID SEARCH` (never ESEARCH). `All` is split into UID ranges so
    /// large mailboxes stay under the response size limit.
    fn uid_search(
        &mut self,
        criteria: SearchCriteria,
    ) -> impl Future<Output = Result<UidSet>> + Send;

    /// `UID FETCH` with the attributes `spec` asks for that the server
    /// supports. A response over the size limit is dropped, so a requested
    /// UID may be missing from the result (as it is when expunged).
    fn fetch(
        &mut self,
        uids: &UidSet,
        spec: &FetchSpec,
    ) -> impl Future<Output = Result<Vec<MessageMeta>>> + Send;

    /// `UID FETCH ... (CHANGEDSINCE since [VANISHED])`; requires CONDSTORE.
    fn fetch_changed_since(
        &mut self,
        uids: &UidSet,
        since: ModSeq,
        spec: &FetchSpec,
    ) -> impl Future<Output = Result<Changes>> + Send;

    /// Streams `BODY.PEEK[section]` into `sink` in chunks, stopping after
    /// `max_bytes`. `section` is `""` (whole message), `HEADER`, `TEXT` or
    /// a part number such as `1.2`.
    fn fetch_body(
        &mut self,
        uid: Uid,
        section: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        max_bytes: u64,
    ) -> impl Future<Output = Result<BodyFetch>> + Send;

    /// `UID STORE <op>FLAGS.SILENT`, with `UNCHANGEDSINCE` under CONDSTORE.
    fn store_flags(
        &mut self,
        uids: &UidSet,
        op: StoreOp,
        flags: &[Flag],
        unchanged_since: Option<ModSeq>,
    ) -> impl Future<Output = Result<StoreResult>> + Send;

    /// `UID MOVE`, else COPY + `\Deleted` + `UID EXPUNGE` (UIDPLUS), else
    /// COPY + plain EXPUNGE when no other message is `\Deleted`.
    fn move_messages(
        &mut self,
        uids: &UidSet,
        destination: &str,
    ) -> impl Future<Output = Result<MoveResult>> + Send;

    fn append(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> impl Future<Output = Result<AppendResult>> + Send;

    /// IDLE until the server reports a change, the renewal timer fires
    /// (below 29 minutes, RFC 2177) or `wake` is notified.
    fn idle(&mut self, wake: &Notify) -> impl Future<Output = Result<IdleResult>> + Send;

    fn noop(&mut self) -> impl Future<Output = Result<()>> + Send;

    fn take_events(&mut self) -> Vec<MailboxEvent>;

    fn logout(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// A password that never shows up in `Debug` output or logs.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

/// Body chunk size for [`ImapSession::fetch_body`].
pub const BODY_CHUNK: u32 = 1024 * 1024;
/// Largest response accepted: one body chunk plus headroom, so an
/// accidental full-message fetch fails instead of buffering it.
pub const MAX_RESPONSE_SIZE: u32 = 2 * BODY_CHUNK + 64 * 1024;
/// RFC 2177 servers may log out after 30 minutes of IDLE.
pub const MAX_IDLE_RENEW: Duration = Duration::from_secs(29 * 60);
pub const DEFAULT_IDLE_RENEW: Duration = Duration::from_secs(25 * 60);
/// `UID SEARCH ALL` is split into UID ranges of this size.
pub const SEARCH_CHUNK: u32 = 100_000;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: Password,
    /// `None` uses [`tls::client_config`] with the Mozilla roots.
    pub tls: Option<Arc<ClientConfig>>,
    pub capability_override: CapabilityOverride,
    pub max_response_size: u32,
    pub body_chunk: u32,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    /// Clamped to [`MAX_IDLE_RENEW`].
    pub idle_renew: Duration,
}

impl ServerConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        security: Security,
        username: impl Into<String>,
        password: Password,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            security,
            username: username.into(),
            password,
            tls: None,
            capability_override: CapabilityOverride::default(),
            max_response_size: MAX_RESPONSE_SIZE,
            body_chunk: BODY_CHUNK,
            connect_timeout: Duration::from_secs(30),
            command_timeout: Duration::from_secs(120),
            idle_renew: DEFAULT_IDLE_RENEW,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Selected {
    pub name: String,
    pub uid_next: Option<Uid>,
    pub nomodseq: bool,
}

/// An authenticated IMAP connection.
pub struct TokioSession {
    pub(crate) conn: Conn,
    pub(crate) caps: Capabilities,
    pub(crate) qresync: bool,
    pub(crate) selected: Option<Selected>,
    pub(crate) events: Vec<MailboxEvent>,
    pub(crate) body_chunk: u32,
    pub(crate) idle_renew: Duration,
}

impl fmt::Debug for TokioSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokioSession")
            .field("caps", &self.caps)
            .field("qresync", &self.qresync)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl TokioSession {
    /// Connects, secures, authenticates and enables CONDSTORE/QRESYNC.
    pub async fn connect(config: &ServerConfig) -> Result<Self> {
        let addr = (config.host.as_str(), config.port);
        let tcp = tokio::time::timeout(config.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Timeout)??;
        tcp.set_nodelay(true)?;
        let tls = || match &config.tls {
            Some(c) => Ok(c.clone()),
            None => tls::client_config(None),
        };
        let (io, greeting_done): (Box<dyn Io>, bool) = match config.security {
            Security::Plain => (Box::new(tcp), false),
            Security::Tls => (Box::new(handshake(tcp, &config.host, tls()?).await?), false),
            Security::StartTls => {
                let tcp = tokio::time::timeout(config.command_timeout, starttls(tcp))
                    .await
                    .map_err(|_| Error::Timeout)??;
                (Box::new(handshake(tcp, &config.host, tls()?).await?), true)
            }
        };
        let fut = Self::start(io, greeting_done, config);
        tokio::time::timeout(config.connect_timeout + config.command_timeout, fut)
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Runs the protocol from the greeting on an established stream.
    pub(crate) async fn start(
        io: Box<dyn Io>,
        greeting_done: bool,
        config: &ServerConfig,
    ) -> Result<Self> {
        let mut conn = Conn::new(
            io,
            greeting_done,
            config.max_response_size,
            config.command_timeout,
        );
        let mut preauth = false;
        if !greeting_done {
            let greeting = conn.greeting().await?;
            match greeting.kind {
                GreetingKind::Ok => {}
                GreetingKind::PreAuth => preauth = true,
                GreetingKind::Bye => {
                    return Err(Error::Unavailable(greeting.text.as_ref().to_owned()));
                }
            }
        }
        let mut session = Self {
            conn,
            caps: Capabilities::default(),
            qresync: false,
            selected: None,
            events: Vec::new(),
            body_chunk: config.body_chunk,
            idle_renew: config.idle_renew.min(MAX_IDLE_RENEW),
        };
        session.refresh_capabilities(config).await?;
        if !preauth {
            if config.security == Security::Plain && session.caps.login_disabled() {
                return Err(Error::Unsupported("login over an unencrypted connection"));
            }
            session.authenticate(config).await?;
            session.refresh_capabilities(config).await?;
        }
        session.enable().await?;
        Ok(session)
    }

    async fn refresh_capabilities(&mut self, config: &ServerConfig) -> Result<()> {
        let (out, done) = self.conn.run(CommandBody::Capability).await?;
        check(done)?;
        let mut caps: Capabilities = out
            .data
            .iter()
            .filter_map(|d| match d {
                Data::Capability(c) => Some(c.as_ref().iter().map(|c| c.to_string())),
                _ => None,
            })
            .flatten()
            .collect();
        config.capability_override.apply(&mut caps);
        self.caps = caps;
        Ok(())
    }

    async fn authenticate(&mut self, config: &ServerConfig) -> Result<()> {
        let user = config.username.as_str();
        let pass = config.password.expose();
        let done = if self.caps.auth_plain() {
            let ir = format!("\0{user}\0{pass}").into_bytes();
            let sasl_ir = self.caps.sasl_ir();
            let body = CommandBody::Authenticate {
                mechanism: AuthMechanism::Plain,
                initial_response: sasl_ir.then(|| Secret::new(ir.clone().into())),
            };
            self.authenticate_exchange(body, (!sasl_ir).then_some(ir))
                .await?
        } else {
            let body = CommandBody::login(user.to_owned(), pass.to_owned())
                .map_err(|_| Error::InvalidArgument("username or password".into()))?;
            self.conn.run(body).await?.1
        };
        match done.kind {
            StatusKind::Ok => Ok(()),
            _ => {
                let code = done.code.as_ref().map(code_name);
                let text = done.text;
                match code.as_deref() {
                    Some("UNAVAILABLE" | "LIMIT" | "INUSE" | "SERVERBUG") => {
                        Err(Error::Unavailable(text))
                    }
                    _ => Err(Error::Auth(text)),
                }
            }
        }
    }

    async fn authenticate_exchange(
        &mut self,
        body: CommandBody<'static>,
        mut response: Option<Vec<u8>>,
    ) -> Result<Done> {
        use imap_next::client::Event;
        let tag = self.conn.enqueue(body);
        let mut out = Collected::default();
        loop {
            let event = self.conn.next_event().await?;
            if let crate::conn::ConnEvent::Next(Event::AuthenticateContinuationRequestReceived {
                ..
            }) = event
            {
                let data = response.take().unwrap_or_default();
                self.conn.set_authenticate_data(data)?;
                continue;
            }
            if let Some(done) = self.conn.absorb(event, tag.as_ref(), &mut out)? {
                return Ok(done);
            }
        }
    }

    async fn enable(&mut self) -> Result<()> {
        if !self.caps.enable() {
            return Ok(());
        }
        let wanted = if self.caps.qresync() {
            CapabilityEnable::try_from("QRESYNC")
        } else if self.caps.condstore() {
            Ok(CapabilityEnable::CondStore)
        } else {
            return Ok(());
        };
        let wanted = wanted.map_err(|e| Error::Protocol(e.to_string()))?;
        let (out, done) = self
            .conn
            .run(CommandBody::Enable {
                capabilities: Vec1::from(wanted),
            })
            .await?;
        check(done)?;
        self.qresync = out.data.iter().any(|d| match d {
            Data::Enabled { capabilities } => capabilities
                .iter()
                .any(|c| c.to_string().eq_ignore_ascii_case("QRESYNC")),
            _ => false,
        });
        Ok(())
    }

    /// Runs a command, queues unsolicited changes and fails on NO/BAD.
    pub(crate) async fn run(&mut self, body: CommandBody<'static>) -> Result<(Collected, Done)> {
        self.ensure_usable()?;
        let (out, done) = self.conn.run(body).await?;
        Ok((out, check_done(done)?))
    }

    pub(crate) async fn run_raw(&mut self, line: &str) -> Result<(Collected, Done)> {
        self.ensure_usable()?;
        let (out, done) = self.conn.run_raw(line).await?;
        Ok((out, check_done(done)?))
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.conn.broken {
            Err(self.conn.closed())
        } else {
            Ok(())
        }
    }

    pub(crate) fn selected_state(&self) -> Result<&Selected> {
        self.selected.as_ref().ok_or(Error::NoMailboxSelected)
    }

    /// Queues the changes in `out` except FETCHes for which `keep` holds,
    /// which are returned as the command's result.
    pub(crate) fn split_events(
        &mut self,
        out: Collected,
        keep: impl Fn(&MessageMeta) -> bool,
    ) -> (Vec<MessageMeta>, UidSet) {
        let mut results = Vec::new();
        let mut vanished = UidSet::new();
        let mut metas = Vec::new();
        for d in out.data {
            match d {
                Data::Fetch { seq, items } => {
                    metas.push(fetch::meta_from_items(seq.get(), items.as_ref()));
                }
                Data::Exists(n) => self.events.push(MailboxEvent::Exists(n)),
                Data::Expunge(seq) => self.events.push(MailboxEvent::Expunge(seq.get())),
                other => {
                    if let Some(v) = qresync::vanished(&other) {
                        if v.earlier {
                            vanished.extend(&v.uids);
                        } else {
                            self.events.push(MailboxEvent::Vanished(v));
                        }
                    }
                }
            }
        }
        metas.extend(out.raw_fetches.into_iter().map(|mut m| {
            m.degraded = true;
            m
        }));
        for meta in metas {
            if keep(&meta) {
                results.push(meta);
            } else {
                self.events.push(MailboxEvent::Fetch(Box::new(meta)));
            }
        }
        (results, vanished)
    }

    pub(crate) fn mailbox(name: &str) -> Result<Mailbox<'static>> {
        Mailbox::try_from(name.to_owned())
            .map_err(|e| Error::InvalidArgument(format!("mailbox {name:?}: {e}")))
    }

    async fn uid_search_typed(&mut self, keys: Vec<SearchKey<'static>>) -> Result<UidSet> {
        let charset = keys
            .iter()
            .any(|k| matches!(k, SearchKey::Header(_, v) if !v.as_ref().is_ascii()))
            .then(|| "UTF-8".try_into())
            .transpose()
            .map_err(|_| Error::Protocol("charset".into()))?;
        let keys =
            Vec1::try_from(keys).map_err(|_| Error::InvalidArgument("empty search".into()))?;
        let (out, _) = self.run(CommandBody::search(charset, keys, true)).await?;
        Ok(search_result(out, &mut self.events))
    }
}

fn search_result(out: Collected, events: &mut Vec<MailboxEvent>) -> UidSet {
    let mut set = UidSet::new();
    for d in out.data {
        match d {
            Data::Search(uids, ..) => {
                for uid in uids {
                    set.insert(Uid(uid.get()));
                }
            }
            Data::Exists(n) => events.push(MailboxEvent::Exists(n)),
            Data::Expunge(n) => events.push(MailboxEvent::Expunge(n.get())),
            _ => {}
        }
    }
    set
}

pub(crate) fn code_name(code: &Code<'_>) -> String {
    match code {
        Code::Other(other) => {
            let raw = String::from_utf8_lossy(other.inner());
            raw.split(' ')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase()
        }
        other => format!("{other:?}")
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase(),
    }
}

fn check(done: Done) -> Result<()> {
    check_done(done).map(|_| ())
}

fn check_done(done: Done) -> Result<Done> {
    match done.kind {
        StatusKind::Ok => Ok(done),
        StatusKind::No => Err(Error::No {
            code: done.code.as_ref().map(code_name),
            text: done.text,
        }),
        StatusKind::Bad => Err(Error::Bad(done.text)),
    }
}

async fn handshake(
    tcp: TcpStream,
    host: &str,
    config: Arc<ClientConfig>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| Error::InvalidServerName(host.to_owned()))?;
    Ok(TlsConnector::from(config).connect(name, tcp).await?)
}

/// Upgrades a plain connection. Line-based on purpose: any byte the server
/// sends after the tagged OK and before the handshake would be injected
/// plaintext, so it is rejected instead of buffered.
pub(crate) async fn starttls<S>(stream: S) -> Result<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    if !line.starts_with(b"* OK") && !line.starts_with(b"* ok") {
        return Err(Error::Unavailable(
            String::from_utf8_lossy(&line).trim().to_owned(),
        ));
    }
    reader.get_mut().write_all(b"S0 STARTTLS\r\n").await?;
    reader.get_mut().flush().await?;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(Error::Closed { bye: None });
        }
        if line.starts_with(b"* ") {
            continue;
        }
        let ok = line.len() > 5 && line[..5].eq_ignore_ascii_case(b"S0 OK");
        if !ok {
            return Err(Error::Unsupported("STARTTLS"));
        }
        if !reader.buffer().is_empty() {
            return Err(Error::Protocol("data after STARTTLS response".into()));
        }
        return Ok(reader.into_inner());
    }
}

impl ImapSession for TokioSession {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn qresync_enabled(&self) -> bool {
        self.qresync
    }

    fn is_usable(&self) -> bool {
        !self.conn.broken
    }

    fn selected(&self) -> Option<&str> {
        self.selected.as_ref().map(|s| s.name.as_str())
    }

    async fn list(&mut self) -> Result<Vec<MailboxInfo>> {
        let body =
            CommandBody::list("", "*").map_err(|_| Error::Protocol("LIST arguments".into()))?;
        let (out, _) = self.run(body).await?;
        let mut boxes = Vec::new();
        for d in &out.data {
            if let Data::List {
                items,
                delimiter,
                mailbox,
            } = d
            {
                let attributes: Vec<String> = items.iter().map(|a| a.to_string()).collect();
                let selectable = !items.iter().any(|a| {
                    matches!(a, FlagNameAttribute::Noselect)
                        || a.to_string().eq_ignore_ascii_case("\\NonExistent")
                });
                boxes.push(MailboxInfo {
                    name: fetch::mailbox_name(mailbox),
                    delimiter: delimiter.as_ref().map(|d| d.inner()),
                    special_use: attributes.iter().find_map(|a| a.parse::<SpecialUse>().ok()),
                    attributes,
                    selectable,
                });
            }
        }
        self.split_events(out, |_| false);
        Ok(boxes)
    }

    async fn create_mailbox(&mut self, name: &str) -> Result<()> {
        self.run(CommandBody::Create {
            mailbox: Self::mailbox(name)?,
        })
        .await?;
        Ok(())
    }

    async fn select(&mut self, name: &str, options: SelectOptions) -> Result<SelectedMailbox> {
        let mailbox = Self::mailbox(name)?;
        let mut parameters = Vec::new();
        match (&options.qresync, self.qresync) {
            (Some(q), true) => parameters.push(SelectParameter::QResync {
                uid_validity: NonZeroU32::new(q.uid_validity.0)
                    .ok_or_else(|| Error::InvalidArgument("UIDVALIDITY 0".into()))?,
                mod_sequence_value: std::num::NonZeroU64::new(q.modseq.0)
                    .ok_or_else(|| Error::InvalidArgument("modseq 0".into()))?,
                known_uids: q
                    .known_uids
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(fetch::sequence_set)
                    .transpose()?,
                seq_match_data: None,
            }),
            _ if self.caps.condstore() => parameters.push(SelectParameter::CondStore),
            _ => {}
        }
        let body = if options.read_only {
            CommandBody::Examine {
                mailbox,
                parameters,
            }
        } else {
            CommandBody::Select {
                mailbox,
                parameters,
            }
        };
        self.selected = None;
        self.events.clear();
        let (out, done) = self.run(body).await?;
        let mut selected = SelectedMailbox {
            name: name.to_owned(),
            read_only: options.read_only,
            uid_validity: UidValidity(0),
            uid_next: None,
            exists: 0,
            highest_modseq: None,
            nomodseq: false,
            flags: Vec::new(),
            permanent_flags: Vec::new(),
            keyword_capable: false,
            vanished: UidSet::new(),
            changed: Vec::new(),
        };
        let mut uid_validity = None;
        for code in out.codes().chain(done.code.as_ref()) {
            match code {
                Code::UidValidity(v) => uid_validity = Some(UidValidity(v.get())),
                Code::UidNext(n) => selected.uid_next = Some(Uid(n.get())),
                Code::HighestModSeq(m) => selected.highest_modseq = Some(ModSeq(m.get())),
                Code::NoModSeq => selected.nomodseq = true,
                Code::ReadOnly => selected.read_only = true,
                Code::PermanentFlags(flags) => {
                    selected.keyword_capable =
                        flags.iter().any(|f| matches!(f, FlagPerm::Asterisk));
                    selected.permanent_flags = flags
                        .iter()
                        .filter_map(|f| match f {
                            FlagPerm::Flag(f) => Flag::parse(&f.to_string()),
                            FlagPerm::Asterisk => None,
                        })
                        .collect();
                }
                _ => {}
            }
        }
        for d in &out.data {
            match d {
                Data::Exists(n) => selected.exists = *n,
                Data::Flags(flags) => {
                    selected.flags = flags
                        .iter()
                        .filter_map(|f| Flag::parse(&f.to_string()))
                        .collect();
                }
                _ => {}
            }
        }
        selected.uid_validity = uid_validity
            .ok_or_else(|| Error::Protocol(format!("SELECT {name} without UIDVALIDITY")))?;
        if selected.nomodseq {
            selected.highest_modseq = None;
        }
        let (changed, vanished) = self.split_events(out, |m| m.uid.is_some());
        // EXISTS during SELECT is the initial count, not a change.
        self.events
            .retain(|e| !matches!(e, MailboxEvent::Exists(_)));
        selected.changed = changed;
        selected.vanished = vanished;
        self.selected = Some(Selected {
            name: name.to_owned(),
            uid_next: selected.uid_next,
            nomodseq: selected.nomodseq,
        });
        Ok(selected)
    }

    async fn status(&mut self, name: &str) -> Result<MailboxStatus> {
        let mut items = vec![
            StatusDataItemName::Messages,
            StatusDataItemName::UidNext,
            StatusDataItemName::UidValidity,
            StatusDataItemName::Unseen,
        ];
        if self.caps.condstore() {
            items.push(StatusDataItemName::HighestModSeq);
        }
        let (out, _) = self
            .run(CommandBody::Status {
                mailbox: Self::mailbox(name)?,
                item_names: items.into(),
            })
            .await?;
        let mut status = MailboxStatus::default();
        for d in &out.data {
            if let Data::Status { items, .. } = d {
                for item in items.iter() {
                    match item {
                        StatusDataItem::Messages(n) => status.messages = Some(*n),
                        StatusDataItem::UidNext(n) => status.uid_next = Some(Uid(n.get())),
                        StatusDataItem::UidValidity(n) => {
                            status.uid_validity = Some(UidValidity(n.get()));
                        }
                        StatusDataItem::Unseen(n) => status.unseen = Some(*n),
                        StatusDataItem::HighestModSeq(n) => {
                            status.highest_modseq = (*n > 0).then_some(ModSeq(*n));
                        }
                        _ => {}
                    }
                }
            }
        }
        self.split_events(out, |_| false);
        Ok(status)
    }

    async fn uid_search(&mut self, criteria: SearchCriteria) -> Result<UidSet> {
        match criteria {
            SearchCriteria::All => {
                let uid_next = self.selected_state()?.uid_next.map_or(0, |u| u.0);
                if uid_next <= SEARCH_CHUNK {
                    return self.uid_search_typed(vec![SearchKey::All]).await;
                }
                let mut all = UidSet::new();
                let mut lo = 1u32;
                while lo < uid_next {
                    let hi = lo.saturating_add(SEARCH_CHUNK - 1);
                    let range = if hi >= uid_next - 1 {
                        format!("{lo}:*")
                    } else {
                        format!("{lo}:{hi}")
                    };
                    let set = SequenceSet::try_from(range.as_str())
                        .map_err(|e| Error::Protocol(e.to_string()))?;
                    let part = self.uid_search_typed(vec![SearchKey::Uid(set)]).await?;
                    all.extend(&part);
                    lo = hi.saturating_add(1);
                }
                Ok(all)
            }
            SearchCriteria::Deleted => self.uid_search_typed(vec![SearchKey::Deleted]).await,
            SearchCriteria::Header { name, value } => {
                let astring = |s: String| {
                    AString::try_from(s)
                        .map(IntoStatic::into_static)
                        .map_err(|e| Error::InvalidArgument(e.to_string()))
                };
                let key = SearchKey::Header(astring(name)?, astring(value)?);
                self.uid_search_typed(vec![key]).await
            }
            SearchCriteria::EmailId(id) => {
                if !self.caps.objectid() {
                    return Err(Error::Unsupported("OBJECTID"));
                }
                if id.is_empty()
                    || !id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                {
                    return Err(Error::InvalidArgument(format!("EMAILID {id:?}")));
                }
                let (out, _) = self.run_raw(&format!("UID SEARCH EMAILID {id}")).await?;
                Ok(search_result(out, &mut self.events))
            }
        }
    }

    async fn fetch(&mut self, uids: &UidSet, spec: &FetchSpec) -> Result<Vec<MessageMeta>> {
        self.fetch_impl(uids, spec, None).await.map(|c| c.messages)
    }

    async fn fetch_changed_since(
        &mut self,
        uids: &UidSet,
        since: ModSeq,
        spec: &FetchSpec,
    ) -> Result<Changes> {
        if !self.caps.condstore() {
            return Err(Error::Unsupported("CONDSTORE"));
        }
        if self.selected_state()?.nomodseq {
            return Err(Error::Unsupported("CHANGEDSINCE on a NOMODSEQ mailbox"));
        }
        self.fetch_impl(uids, spec, Some(since)).await
    }

    async fn fetch_body(
        &mut self,
        uid: Uid,
        section: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        max_bytes: u64,
    ) -> Result<BodyFetch> {
        self.fetch_body_impl(uid, section, sink, max_bytes).await
    }

    async fn store_flags(
        &mut self,
        uids: &UidSet,
        op: StoreOp,
        flags: &[Flag],
        unchanged_since: Option<ModSeq>,
    ) -> Result<StoreResult> {
        self.store_impl(uids, op, flags, unchanged_since).await
    }

    async fn move_messages(&mut self, uids: &UidSet, destination: &str) -> Result<MoveResult> {
        self.move_impl(uids, destination).await
    }

    async fn append(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> Result<AppendResult> {
        self.append_impl(mailbox, flags, message).await
    }

    async fn idle(&mut self, wake: &Notify) -> Result<IdleResult> {
        self.idle_impl(wake).await
    }

    async fn noop(&mut self) -> Result<()> {
        let (out, _) = self.run(CommandBody::Noop).await?;
        self.split_events(out, |_| false);
        Ok(())
    }

    fn take_events(&mut self) -> Vec<MailboxEvent> {
        std::mem::take(&mut self.events)
    }

    async fn logout(&mut self) -> Result<()> {
        if self.conn.broken {
            return Ok(());
        }
        let result = self.conn.run(CommandBody::Logout).await.map(|_| ());
        self.conn.broken = true;
        match result {
            Err(Error::Closed { .. }) => Ok(()),
            other => other,
        }
    }
}

pub(crate) fn imap_flags(flags: &[Flag]) -> Result<Vec<ImapFlag<'static>>> {
    flags
        .iter()
        .map(|f| match f {
            Flag::Seen => Ok(ImapFlag::Seen),
            Flag::Answered => Ok(ImapFlag::Answered),
            Flag::Flagged => Ok(ImapFlag::Flagged),
            Flag::Deleted => Ok(ImapFlag::Deleted),
            Flag::Draft => Ok(ImapFlag::Draft),
            Flag::Keyword(k) => Atom::try_from(k.clone())
                .map(ImapFlag::Keyword)
                .map_err(|e| Error::InvalidArgument(format!("keyword {k:?}: {e}"))),
            Flag::Extension(x) => Atom::try_from(x.trim_start_matches('\\').to_owned())
                .map(ImapFlag::system)
                .map_err(|e| Error::InvalidArgument(format!("flag {x:?}: {e}"))),
        })
        .collect()
}
