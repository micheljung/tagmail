//! Shared harness for the Dovecot-backed engine tests: an engine over
//! temporary databases whose sessions record full FETCHes, plus a second
//! client for external changes.
#![allow(dead_code)]

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tagmail_api_core::ChangeFeed;
use tagmail_core::AccountId;
use tagmail_engine::{AccountSettings, ConnectorFactory, Engine, EngineConfig, Stores};
use tagmail_imap::*;
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey, Vault};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;

pub const PASSWORD: &str = "engine";
/// AC 3/4: changes reach the cache this fast via IDLE or the next poll.
pub const LIVE_BUDGET: Duration = Duration::from_secs(10);
pub const POLL: Duration = Duration::from_secs(2);

pub fn addr() -> (String, u16) {
    let addr = std::env::var("TAGMAIL_IMAP_ADDR").unwrap_or_else(|_| "127.0.0.1:10143".into());
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"))
}

pub fn server_config(
    user: &str,
    password: &str,
    capability_override: CapabilityOverride,
) -> ServerConfig {
    let (host, port) = addr();
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
        self.inner.append(mailbox, flags, message).await
    }
    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()> {
        self.inner.expunge_uids(uids).await
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

#[derive(Clone)]
pub struct DovecotFactory {
    capability_override: CapabilityOverride,
    log: Log,
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
            config: server_config(
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
}

pub struct Running {
    pub engine: Engine<DovecotFactory>,
    pub cache: Arc<CacheDb>,
    pub feed: Arc<ChangeFeed>,
    pub log: Log,
}

impl Fixture {
    /// A test-owned `engineN` user, emptied first.
    pub async fn new(user: &'static str) -> Fixture {
        reset_user(user).await;
        Fixture::existing(user, PASSWORD)
    }

    /// A user whose mail is left as it is (the seeded `user1`).
    pub fn existing(user: &'static str, password: &'static str) -> Fixture {
        Fixture {
            dir: TempDir::new().unwrap(),
            user,
            password,
        }
    }

    pub async fn start(
        &self,
        capability_override: CapabilityOverride,
        fail_after: Option<usize>,
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
        let sealed = vault.seal(AccountId(1), password).unwrap();
        config
            .write(move |tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO account (id, name, imap_host, imap_port, imap_tls, \
                     smtp_host, smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                     VALUES (1, 'it', 'dovecot', 10143, 'tls', 'mailpit', 10025, 'tls', ?1, ?2, ?3)",
                    params![user, sealed.ciphertext, sealed.nonce],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let blobs = Arc::new(BlobStore::open(&self.dir.path().join("blobs")).unwrap());
        let feed = Arc::new(ChangeFeed::new(cache.head().await.unwrap()));
        let fetch_log = Log::default();
        log(&fetch_log).fail_after = fail_after;
        let stores = Stores {
            config,
            vault,
            cache: cache.clone(),
            blobs,
            feed: feed.clone(),
        };
        let engine = Engine::new(
            stores,
            DovecotFactory {
                capability_override,
                log: fetch_log.clone(),
            },
            EngineConfig {
                poll_interval: POLL,
                retry_min: Duration::from_millis(200),
                retry_max: Duration::from_secs(1),
                ..EngineConfig::default()
            },
        );
        engine.start_all().await.unwrap();
        Running {
            engine,
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
    TokioSession::connect(&server_config(
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
        let (host, port) = addr();
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

/// Deletes the user's own mailboxes and empties the special ones.
pub async fn reset_user(user: &str) {
    let mut raw = Raw::login(user).await;
    let list = raw.cmd("LIST \"\" \"*\"").await;
    for line in list {
        let name = line
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned();
        if !KEEP.contains(&name.as_str()) && !name.is_empty() {
            raw.cmd(&format!("DELETE \"{name}\"")).await;
        }
    }
    for name in KEEP {
        raw.cmd(&format!("SELECT {name}")).await;
        raw.cmd("UID STORE 1:* +FLAGS.SILENT (\\Deleted)").await;
        raw.cmd("EXPUNGE").await;
    }
    raw.cmd("LOGOUT").await;
}

pub fn no_override() -> CapabilityOverride {
    CapabilityOverride::default()
}
