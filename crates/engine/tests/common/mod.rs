//! Shared harness for the Dovecot-backed engine tests: an engine over
//! temporary databases whose sessions record full FETCHes, plus a second
//! client for external changes.
#![allow(dead_code)]

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tagmail_api_core::ChangeFeed;
use tagmail_core::AccountId;
use tagmail_engine::{
    AccountSettings, ConnectorFactory, Engine, EngineConfig, LettreSender, Stores,
};
use tagmail_imap::*;
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey, Vault};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;

pub const PASSWORD: &str = "engine";
/// AC 3/4: changes reach the cache this fast via IDLE or the next poll.
pub const LIVE_BUDGET: Duration = Duration::from_secs(10);
pub const POLL: Duration = Duration::from_secs(2);

pub fn addr() -> (String, u16) {
    addr_of(Server::Dovecot)
}

/// The test servers: `dovecot` and `dovecot-gmail` (a virtual `\All`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    Dovecot,
    Gmail,
}

/// `TAGMAIL_IMAP_ADDR` (default `127.0.0.1:10143`) or, for the Gmail-mode
/// server, `TAGMAIL_IMAP_GMAIL_ADDR` (default `127.0.0.1:10144`).
pub fn addr_of(server: Server) -> (String, u16) {
    let addr = match server {
        Server::Dovecot => {
            std::env::var("TAGMAIL_IMAP_ADDR").unwrap_or_else(|_| "127.0.0.1:10143".into())
        }
        Server::Gmail => {
            std::env::var("TAGMAIL_IMAP_GMAIL_ADDR").unwrap_or_else(|_| "127.0.0.1:10144".into())
        }
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"))
}

/// Mailpit's SMTP port (`TAGMAIL_SMTP_ADDR`).
pub fn smtp_addr() -> (String, u16) {
    let addr = std::env::var("TAGMAIL_SMTP_ADDR").unwrap_or_else(|_| "127.0.0.1:10025".into());
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"))
}

pub fn mailpit_addr() -> String {
    std::env::var("TAGMAIL_MAILPIT_API").unwrap_or_else(|_| "127.0.0.1:10825".into())
}

/// `GET path` on Mailpit's API; the response body.
pub async fn mailpit(path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(mailpit_addr())
        .await
        .expect("Mailpit API");
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: mailpit\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP response");
    let head = String::from_utf8_lossy(&response[..split]);
    assert!(
        head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"),
        "{head}"
    );
    response[split + 4..].to_vec()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Mailpit's id of the message with this Message-ID, if it arrived.
pub async fn mailpit_id(msgid: &str) -> Option<String> {
    let query = encode(&format!("message-id:\"{msgid}\""));
    let found: serde_json::Value =
        serde_json::from_slice(&mailpit(&format!("/api/v1/search?query={query}")).await).unwrap();
    found["messages"][0]["ID"].as_str().map(str::to_owned)
}

/// How the account of [`Fixture::start_sending`] is set up.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sending {
    pub server_saves_sent: Option<bool>,
}

pub fn server_config(
    user: &str,
    password: &str,
    capability_override: CapabilityOverride,
) -> ServerConfig {
    server_config_at(Server::Dovecot, user, password, capability_override)
}

pub fn server_config_at(
    server: Server,
    user: &str,
    password: &str,
    capability_override: CapabilityOverride,
) -> ServerConfig {
    let (host, port) = addr_of(server);
    let mut config = ServerConfig::new(host, port, Security::Plain, user, Password::new(password));
    config.capability_override = capability_override;
    config
}

/// What the engine fetched with envelopes, per mailbox.
#[derive(Default)]
pub struct FetchLog {
    pub full: Vec<(String, u32)>,
    /// Full fetches allowed before every further one fails like a dropped
    /// connection.
    fail_after: Option<usize>,
    full_fetches: usize,
}

pub type Log = Arc<Mutex<FetchLog>>;

pub fn log(l: &Log) -> std::sync::MutexGuard<'_, FetchLog> {
    l.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A Dovecot session that records full FETCHes.
pub struct Counted {
    inner: TokioSession,
    log: Log,
}

impl ImapSession for Counted {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    fn qresync_enabled(&self) -> bool {
        self.inner.qresync_enabled()
    }
    fn is_usable(&self) -> bool {
        self.inner.is_usable()
    }
    fn selected(&self) -> Option<&str> {
        self.inner.selected()
    }
    async fn list(&mut self) -> Result<Vec<MailboxInfo>> {
        self.inner.list().await
    }
    async fn create_mailbox(&mut self, name: &str) -> Result<()> {
        self.inner.create_mailbox(name).await
    }
    async fn select(&mut self, name: &str, options: SelectOptions) -> Result<SelectedMailbox> {
        self.inner.select(name, options).await
    }
    async fn status(&mut self, name: &str) -> Result<MailboxStatus> {
        self.inner.status(name).await
    }
    async fn uid_search(&mut self, criteria: SearchCriteria) -> Result<UidSet> {
        self.inner.uid_search(criteria).await
    }
    async fn fetch(&mut self, uids: &UidSet, spec: &FetchSpec) -> Result<Vec<MessageMeta>> {
        if spec.envelope {
            let mut l = log(&self.log);
            if l.fail_after.is_some_and(|n| l.full_fetches >= n) {
                return Err(Error::Closed { bye: None });
            }
            l.full_fetches += 1;
        }
        let out = self.inner.fetch(uids, spec).await?;
        if spec.envelope {
            let mailbox = self.inner.selected().unwrap_or_default().to_owned();
            let mut l = log(&self.log);
            for m in &out {
                if let Some(uid) = m.uid {
                    l.full.push((mailbox.clone(), uid.0));
                }
            }
        }
        Ok(out)
    }
    async fn fetch_changed_since(
        &mut self,
        uids: &UidSet,
        since: ModSeq,
        spec: &FetchSpec,
    ) -> Result<Changes> {
        self.inner.fetch_changed_since(uids, since, spec).await
    }
    async fn fetch_body(
        &mut self,
        uid: Uid,
        section: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        max_bytes: u64,
    ) -> Result<BodyFetch> {
        self.inner.fetch_body(uid, section, sink, max_bytes).await
    }
    async fn store_flags(
        &mut self,
        uids: &UidSet,
        op: StoreOp,
        flags: &[Flag],
        unchanged_since: Option<ModSeq>,
    ) -> Result<StoreResult> {
        self.inner
            .store_flags(uids, op, flags, unchanged_since)
            .await
    }
    async fn move_messages(&mut self, uids: &UidSet, destination: &str) -> Result<MoveResult> {
        self.inner.move_messages(uids, destination).await
    }
    async fn append(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> Result<AppendResult> {
        let appended = self.inner.append(mailbox, flags, message).await?;
        Ok(self.without_appenduid_unless_uidplus(appended))
    }
    async fn append_stream(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        size: u64,
        message: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    ) -> Result<AppendResult> {
        let appended = self
            .inner
            .append_stream(mailbox, flags, size, message)
            .await?;
        Ok(self.without_appenduid_unless_uidplus(appended))
    }
    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()> {
        self.inner.expunge_uids(uids).await
    }
    async fn copy_messages(&mut self, uids: &UidSet, destination: &str) -> Result<MoveResult> {
        self.inner.copy_messages(uids, destination).await
    }
    async fn get_metadata(&mut self, entry: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_metadata(entry).await
    }
    async fn set_metadata(&mut self, entry: &str, value: Option<&[u8]>) -> Result<()> {
        self.inner.set_metadata(entry, value).await
    }
    async fn idle(&mut self, wake: &Notify) -> Result<IdleResult> {
        self.inner.idle(wake).await
    }
    async fn noop(&mut self) -> Result<()> {
        self.inner.noop().await
    }
    fn take_events(&mut self) -> Vec<MailboxEvent> {
        self.inner.take_events()
    }
    async fn logout(&mut self) -> Result<()> {
        self.inner.logout().await
    }
}

impl Counted {
    /// Servers send `APPENDUID` whatever the capability override hides, so
    /// a hidden UIDPLUS drops it here.
    fn without_appenduid_unless_uidplus(&self, mut appended: AppendResult) -> AppendResult {
        if !self.inner.capabilities().uidplus() {
            appended.uid = None;
        }
        appended
    }
}

#[derive(Clone)]
pub struct DovecotFactory {
    capability_override: CapabilityOverride,
    log: Log,
    server: Server,
}

pub struct CountingConnector {
    config: ServerConfig,
    log: Log,
}

impl Connector for CountingConnector {
    type Session = Counted;

    async fn connect(&self) -> Result<Counted> {
        Ok(Counted {
            inner: TokioSession::connect(&self.config).await?,
            log: self.log.clone(),
        })
    }
}

impl ConnectorFactory for DovecotFactory {
    type Connector = CountingConnector;

    fn connector(&self, account: &AccountSettings) -> CountingConnector {
        CountingConnector {
            config: server_config_at(
                self.server,
                &account.username,
                account.password.expose(),
                self.capability_override.clone(),
            ),
            log: self.log.clone(),
        }
    }
}

/// Databases of one simulated server process; reopened across restarts.
pub struct Fixture {
    dir: TempDir,
    user: &'static str,
    password: &'static str,
    server: Server,
    /// Accounts 2, 3, … with [`PASSWORD`].
    more: Vec<&'static str>,
}

/// The sender address of a test user: `<user>@tagmail.test`.
pub fn email_of(user: &str) -> String {
    format!("{user}@tagmail.test")
}

pub struct Running {
    pub engine: Engine<DovecotFactory>,
    pub config: Arc<ConfigDb>,
    pub cache: Arc<CacheDb>,
    pub feed: Arc<ChangeFeed>,
    pub log: Log,
}

impl Fixture {
    /// A test-owned `engineN` user, emptied first.
    pub async fn new(user: &'static str) -> Fixture {
        Fixture::on(Server::Dovecot, user).await
    }

    /// A test-owned `engineN` user of `server`, emptied first.
    pub async fn on(server: Server, user: &'static str) -> Fixture {
        reset_user_at(server, user).await;
        Fixture {
            server,
            ..Fixture::existing(user, PASSWORD)
        }
    }

    /// A user whose mail is left as it is (the seeded `user1`).
    pub fn existing(user: &'static str, password: &'static str) -> Fixture {
        Fixture {
            dir: TempDir::new().unwrap(),
            user,
            password,
            server: Server::Dovecot,
            more: Vec::new(),
        }
    }

    /// Adds account `2`, `3`, … for the test-owned `engineN` user, emptied
    /// first.
    pub async fn with_account(mut self, user: &'static str) -> Fixture {
        reset_user_at(self.server, user).await;
        self.more.push(user);
        self
    }

    pub async fn start(
        &self,
        capability_override: CapabilityOverride,
        fail_after: Option<usize>,
    ) -> Running {
        self.start_with(capability_override, fail_after, None).await
    }

    /// Where the engine keeps queued messages.
    pub fn outbox_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("outbox")
    }

    /// An engine that submits to Mailpit without TLS.
    pub async fn start_sending(&self, sending: Sending) -> Running {
        self.start_with(no_override(), None, Some(sending)).await
    }

    async fn start_with(
        &self,
        capability_override: CapabilityOverride,
        fail_after: Option<usize>,
        sending: Option<Sending>,
    ) -> Running {
        let running = self
            .open_with(capability_override, fail_after, sending)
            .await;
        running.engine.start_all().await.unwrap();
        running
    }

    /// Deletes `cache.db` and the blobs, as an operator rebuilding the
    /// cache would (AC 6); `config.db` stays.
    pub fn wipe_cache(&self) {
        for f in ["cache.db", "cache.db-wal", "cache.db-shm"] {
            let _ = std::fs::remove_file(self.dir.path().join(f));
        }
        let _ = std::fs::remove_dir_all(self.dir.path().join("blobs"));
    }

    /// The engine over this fixture's databases without starting it.
    pub async fn open(
        &self,
        capability_override: CapabilityOverride,
        fail_after: Option<usize>,
    ) -> Running {
        self.open_with(capability_override, fail_after, None).await
    }

    async fn open_with(
        &self,
        capability_override: CapabilityOverride,
        fail_after: Option<usize>,
        sending: Option<Sending>,
    ) -> Running {
        let dir = self.dir.path().to_owned();
        let (user, password) = (self.user, self.password);
        let (config, cache) = tokio::task::spawn_blocking(move || {
            (
                ConfigDb::open(&dir.join("config.db")).unwrap(),
                CacheDb::open(&dir.join("cache.db")).unwrap(),
            )
        })
        .await
        .unwrap();
        let config = Arc::new(config);
        let cache = Arc::new(cache);
        let key_path = self.dir.path().join("master.key");
        let master = match std::fs::read(&key_path) {
            Ok(bytes) => MasterKey::from_bytes(zeroize::Zeroizing::new(bytes.try_into().unwrap())),
            Err(_) => {
                let key = MasterKey::generate();
                std::fs::write(&key_path, key.expose()).unwrap();
                key
            }
        };
        let vault = Arc::new(Vault::unlock(&config, &master).await.unwrap());
        let (smtp_host, smtp_port) = smtp_addr();
        let saves_sent = sending.and_then(|s| s.server_saves_sent);
        let users =
            std::iter::once((user, password)).chain(self.more.iter().map(|u| (*u, PASSWORD)));
        let mut accounts = Vec::new();
        for (id, (user, password)) in (1..).zip(users) {
            let sealed = vault.seal(AccountId(id), password).unwrap();
            accounts.push((id, user, sealed));
        }
        config
            .write(move |tx| {
                for (id, user, sealed) in &accounts {
                    tx.execute(
                        "INSERT OR IGNORE INTO account (id, name, imap_host, imap_port, imap_tls, \
                         smtp_host, smtp_port, smtp_tls, username, secret_ct, secret_nonce, \
                         server_saves_sent, email) \
                         VALUES (?7, ?1, 'dovecot', 10143, 'tls', ?4, ?5, 'starttls', ?1, ?2, ?3, ?6, ?8)",
                        params![
                            user,
                            sealed.ciphertext,
                            sealed.nonce,
                            smtp_host,
                            smtp_port,
                            saves_sent,
                            id,
                            email_of(user)
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO identity (account_id, address, is_default) \
                         SELECT ?1, ?2, 1 WHERE NOT EXISTS \
                         (SELECT 1 FROM identity WHERE account_id = ?1)",
                        params![id, email_of(user)],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let blobs = Arc::new(BlobStore::open(&self.dir.path().join("blobs")).unwrap());
        let feed = Arc::new(ChangeFeed::new(cache.head().await.unwrap()));
        let fetch_log = Log::default();
        log(&fetch_log).fail_after = fail_after;
        let stores = Stores {
            config: config.clone(),
            vault,
            cache: cache.clone(),
            blobs,
            feed: feed.clone(),
            uploads: self.dir.path().join("uploads"),
            outbox: self.dir.path().join("outbox"),
        };
        let engine = Engine::with_smtp(
            stores,
            DovecotFactory {
                capability_override,
                log: fetch_log.clone(),
                server: self.server,
            },
            EngineConfig {
                poll_interval: POLL,
                retry_min: Duration::from_millis(200),
                retry_max: Duration::from_secs(1),
                draft_debounce: Duration::ZERO,
                sent_probe: Duration::from_secs(3),
                outbox_retry_min: Duration::from_secs(1),
                outbox_retry_max: Duration::from_secs(2),
                ..EngineConfig::default()
            },
            Arc::new(LettreSender {
                insecure_plaintext: true,
            }),
        );
        Running {
            engine,
            config,
            cache,
            feed,
            log: fetch_log,
        }
    }
}

impl Running {
    pub async fn stop(self) {
        self.engine.shutdown().await;
        drop(self.engine);
        if let Ok(cache) = Arc::try_unwrap(self.cache) {
            cache.close().await.unwrap();
        }
        if let Ok(config) = Arc::try_unwrap(self.config) {
            config.close().await.unwrap();
        }
    }

    pub async fn query<T: Send + 'static>(
        &self,
        sql: &'static str,
        f: impl Fn(&tagmail_store::rusqlite::Row<'_>) -> tagmail_store::rusqlite::Result<T>
        + Send
        + 'static,
    ) -> Vec<T> {
        self.cache
            .read(move |c| {
                let mut stmt = c.prepare(sql)?;
                let rows = stmt
                    .query_map([], |r| f(r))?
                    .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    pub async fn all_live(&self) -> bool {
        let states = self
            .query("SELECT sync_state FROM mailbox", |r| r.get::<_, String>(0))
            .await;
        !states.is_empty() && states.iter().all(|s| s == "live")
    }

    /// (mailbox, message row id, flags, keywords) of every location of the
    /// message with this Message-ID.
    pub async fn locations(&self, msgid: &str) -> Vec<(String, i64, u8, String)> {
        let rows = self
            .query(
                "SELECT m.msgid_hdr, mb.name, m.id, l.flags, l.keywords FROM location l \
                 JOIN message m ON m.id = l.message_id JOIN mailbox mb ON mb.id = l.mailbox_id",
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                },
            )
            .await;
        rows.into_iter()
            .filter(|r| r.0.as_deref() == Some(msgid))
            .map(|r| (r.1, r.2, r.3, r.4))
            .collect()
    }

    pub async fn wait_live(&self) {
        eventually("initial sync", Duration::from_secs(60), || self.all_live()).await;
    }
}

pub async fn eventually<F, Fut>(what: &str, budget: Duration, mut check: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < budget, "{what}: not within {budget:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    start.elapsed()
}

pub fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

pub fn message(id: &str, subject: &str, refs: &[&str]) -> Vec<u8> {
    let mut m = format!(
        "From: Ann <ann@example.com>\r\nTo: me@example.com\r\nSubject: {subject}\r\n\
         Message-ID: <{id}>\r\nDate: Sun, 22 Nov 2026 10:00:00 +0000\r\n"
    );
    if let Some(last) = refs.last() {
        let all: Vec<String> = refs.iter().map(|r| format!("<{r}>")).collect();
        m.push_str(&format!(
            "In-Reply-To: <{last}>\r\nReferences: {}\r\n",
            all.join(" ")
        ));
    }
    m.push_str(&format!("\r\nHello from {id}, zebra{}\r\n", id.len()));
    m.into_bytes()
}

pub async fn second(user: &str) -> TokioSession {
    second_as(user, PASSWORD).await
}

pub async fn second_as(user: &str, password: &str) -> TokioSession {
    second_at(Server::Dovecot, user, password).await
}

pub async fn second_at(server: Server, user: &str, password: &str) -> TokioSession {
    TokioSession::connect(&server_config_at(
        server,
        user,
        password,
        CapabilityOverride::default(),
    ))
    .await
    .expect("second session")
}

/// A line-based IMAP client for what the session API does not offer.
pub struct Raw {
    stream: BufReader<TcpStream>,
    tag: u32,
}

impl Raw {
    pub async fn login(user: &str) -> Raw {
        Raw::login_at(Server::Dovecot, user).await
    }

    pub async fn login_at(server: Server, user: &str) -> Raw {
        let (host, port) = addr_of(server);
        let stream = TcpStream::connect((host.as_str(), port)).await.unwrap();
        let mut raw = Raw {
            stream: BufReader::new(stream),
            tag: 0,
        };
        let mut greeting = String::new();
        raw.stream.read_line(&mut greeting).await.unwrap();
        raw.cmd(&format!("LOGIN {user} {PASSWORD}")).await;
        raw
    }

    /// Sends one command and returns the untagged lines; panics unless OK.
    pub async fn cmd(&mut self, command: &str) -> Vec<String> {
        self.tag += 1;
        let tag = format!("r{}", self.tag);
        self.stream
            .get_mut()
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .await
            .unwrap();
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                self.stream.read_line(&mut line).await.unwrap() > 0,
                "closed"
            );
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                assert!(rest.starts_with("OK"), "{command}: {rest}");
                return lines;
            }
            lines.push(line.trim_end().to_owned());
        }
    }
}

pub const KEEP: &[&str] = &["INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"];

/// Deletes the user's own mailboxes, empties the special ones and drops
/// the label registry annotation.
pub async fn reset_user(user: &str) {
    reset_user_at(Server::Dovecot, user).await;
}

pub async fn reset_user_at(server: Server, user: &str) {
    let mut raw = Raw::login_at(server, user).await;
    let list = raw.cmd("LIST \"\" \"*\"").await;
    for line in list {
        let name = line
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned();
        if !KEEP.contains(&name.as_str()) && !name.is_empty() && !name.starts_with("[Gmail]") {
            raw.cmd(&format!("DELETE \"{name}\"")).await;
        }
    }
    for name in KEEP {
        raw.cmd(&format!("SELECT {name}")).await;
        raw.cmd("UID STORE 1:* +FLAGS.SILENT (\\Deleted)").await;
        raw.cmd("EXPUNGE").await;
    }
    raw.cmd("SETMETADATA \"\" (/private/vendor/tagmail/labels NIL)")
        .await;
    raw.cmd("LOGOUT").await;
}

pub fn no_override() -> CapabilityOverride {
    CapabilityOverride::default()
}
