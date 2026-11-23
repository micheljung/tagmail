//! A large message never sits in memory: a draft with 30 MB of attachments
//! is saved to Drafts, sent and copied to Sent while the process's peak
//! RSS grows by far less than the message. Linux only (VmHWM, reset via
//! `/proc/self/clear_refs`); in its own test binary so no other test
//! shares the process.
#![cfg(target_os = "linux")]

use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tagmail_api_core::compose::{Compose, DraftInput, NewUpload};
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::{BoxFuture, ChangeFeed};
use tagmail_core::{AccountId, SpecialUse};
use tagmail_engine::{
    AccountSettings, ConnectorFactory, Engine, EngineConfig, SmtpError, SmtpSender, SmtpServer,
    Stores,
};
use tagmail_imap::*;
use tagmail_store::rusqlite::params;
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey, Vault};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::Notify;
use tokio::time::Instant;

/// Two attachments of this size, 30 MB in all.
const ATTACHMENT: usize = 15 * 1024 * 1024;
/// Peak RSS growth allowed while the message is built, saved and sent.
const BUDGET: u64 = 12 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Bytes APPENDed per mailbox; message bodies are read and dropped.
#[derive(Default)]
struct Sink {
    appended: Mutex<Vec<(String, u64)>>,
}

struct SinkSession {
    caps: Capabilities,
    sink: Arc<Sink>,
    selected: Option<String>,
}

const MAILBOXES: [(&str, Option<SpecialUse>); 3] = [
    ("INBOX", None),
    ("Sent", Some(SpecialUse::Sent)),
    ("Drafts", Some(SpecialUse::Drafts)),
];

fn unsupported<T>(what: &'static str) -> Result<T> {
    Err(Error::Unsupported(what))
}

impl ImapSession for SinkSession {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn qresync_enabled(&self) -> bool {
        false
    }
    fn is_usable(&self) -> bool {
        true
    }
    fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    async fn list(&mut self) -> Result<Vec<MailboxInfo>> {
        Ok(MAILBOXES
            .iter()
            .map(|(name, special_use)| MailboxInfo {
                name: (*name).to_owned(),
                delimiter: Some('/'),
                attributes: Vec::new(),
                special_use: *special_use,
                selectable: true,
            })
            .collect())
    }
    async fn create_mailbox(&mut self, _: &str) -> Result<()> {
        unsupported("CREATE")
    }
    async fn select(&mut self, name: &str, options: SelectOptions) -> Result<SelectedMailbox> {
        self.selected = Some(name.to_owned());
        Ok(SelectedMailbox {
            name: name.to_owned(),
            read_only: options.read_only,
            uid_validity: UidValidity(1),
            uid_next: Some(Uid(1)),
            exists: 0,
            highest_modseq: None,
            nomodseq: false,
            flags: Vec::new(),
            permanent_flags: Vec::new(),
            keyword_capable: true,
            vanished: UidSet::new(),
            changed: Vec::new(),
        })
    }
    async fn status(&mut self, _: &str) -> Result<MailboxStatus> {
        Ok(MailboxStatus {
            messages: Some(0),
            uid_next: Some(Uid(1)),
            uid_validity: Some(UidValidity(1)),
            unseen: Some(0),
            highest_modseq: None,
        })
    }
    async fn uid_search(&mut self, _: SearchCriteria) -> Result<UidSet> {
        Ok(UidSet::new())
    }
    async fn fetch(&mut self, _: &UidSet, _: &FetchSpec) -> Result<Vec<MessageMeta>> {
        Ok(Vec::new())
    }
    async fn fetch_changed_since(
        &mut self,
        _: &UidSet,
        _: ModSeq,
        _: &FetchSpec,
    ) -> Result<Changes> {
        unsupported("CONDSTORE")
    }
    async fn fetch_body(
        &mut self,
        _: Uid,
        _: &str,
        _: &mut (dyn AsyncWrite + Unpin + Send),
        _: u64,
    ) -> Result<BodyFetch> {
        Err(Error::NotFound)
    }
    async fn store_flags(
        &mut self,
        _: &UidSet,
        _: StoreOp,
        _: &[Flag],
        _: Option<ModSeq>,
    ) -> Result<StoreResult> {
        Ok(StoreResult::default())
    }
    async fn move_messages(&mut self, _: &UidSet, _: &str) -> Result<MoveResult> {
        unsupported("MOVE")
    }
    async fn copy_messages(&mut self, _: &UidSet, _: &str) -> Result<MoveResult> {
        unsupported("COPY")
    }
    async fn append(&mut self, _: &str, _: &[Flag], _: &[u8]) -> Result<AppendResult> {
        unsupported("in-memory APPEND")
    }
    async fn append_stream(
        &mut self,
        mailbox: &str,
        _: &[Flag],
        size: u64,
        message: &mut (dyn AsyncRead + Unpin + Send),
    ) -> Result<AppendResult> {
        let mut buf = vec![0u8; CHUNK];
        let mut read = 0u64;
        while read < size {
            let n = message.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            read += n as u64;
        }
        self.sink
            .appended
            .lock()
            .unwrap()
            .push((mailbox.to_owned(), read));
        Ok(AppendResult {
            uid: Some(Uid(1)),
            uid_validity: Some(UidValidity(1)),
        })
    }
    async fn get_metadata(&mut self, _: &str) -> Result<Option<Vec<u8>>> {
        unsupported("METADATA")
    }
    async fn set_metadata(&mut self, _: &str, _: Option<&[u8]>) -> Result<()> {
        unsupported("METADATA")
    }
    async fn expunge_uids(&mut self, _: &UidSet) -> Result<()> {
        Ok(())
    }
    async fn idle(&mut self, wake: &Notify) -> Result<IdleResult> {
        wake.notified().await;
        Ok(IdleResult {
            stop: IdleStop::Woken,
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

#[derive(Clone)]
struct SinkFactory(Arc<Sink>);

impl Connector for SinkFactory {
    type Session = SinkSession;

    async fn connect(&self) -> Result<SinkSession> {
        Ok(SinkSession {
            caps: ["IMAP4rev1", "UIDPLUS", "IDLE"].into_iter().collect(),
            sink: self.0.clone(),
            selected: None,
        })
    }
}

impl ConnectorFactory for SinkFactory {
    type Connector = SinkFactory;

    fn connector(&self, _: &AccountSettings) -> SinkFactory {
        self.clone()
    }
}

/// Reads each message from its file in chunks and drops it.
#[derive(Default)]
struct SinkSmtp {
    bytes: Arc<AtomicU64>,
}

impl SmtpSender for SinkSmtp {
    fn send(
        &self,
        _: SmtpServer,
        _: String,
        _: Vec<String>,
        message: PathBuf,
    ) -> BoxFuture<std::result::Result<(), SmtpError>> {
        let bytes = self.bytes.clone();
        Box::pin(async move {
            let mut file = tokio::fs::File::open(message).await.unwrap();
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = file.read(&mut buf).await.unwrap();
                if n == 0 {
                    return Ok(());
                }
                bytes.fetch_add(n as u64, Ordering::SeqCst);
            }
        })
    }
}

fn status_kb(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or_else(|| panic!("{field} in /proc/self/status"))
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_30_mb_message_streams_through_draft_send_and_sent() {
    let dir = TempDir::new().unwrap();
    let config = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let sealed = vault.seal(AccountId(1), "secret").unwrap();
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, server_saves_sent) \
                 VALUES (1, 'a', 'imap', 993, 'tls', 'smtp', 465, 'tls', 'me@a.example', \
                 ?1, ?2, 0)",
                params![sealed.ciphertext, sealed.nonce],
            )?;
            tx.execute(
                "INSERT INTO identity (account_id, address, is_default) \
                 VALUES (1, 'me@a.example', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let cache = Arc::new(CacheDb::open(&dir.path().join("cache.db")).unwrap());
    let feed = Arc::new(ChangeFeed::new(cache.head().await.unwrap()));
    let stores = Stores {
        config: config.clone(),
        vault: Arc::new(vault),
        cache,
        blobs: Arc::new(BlobStore::open(&dir.path().join("blobs")).unwrap()),
        feed,
        uploads: dir.path().join("uploads"),
        outbox: dir.path().join("outbox"),
    };
    let sink = Arc::new(Sink::default());
    let smtp = SinkSmtp::default();
    let sent_bytes = smtp.bytes.clone();
    let engine = Engine::with_smtp(
        stores,
        SinkFactory(sink.clone()),
        EngineConfig {
            draft_debounce: Duration::ZERO,
            poll_interval: Duration::from_secs(3600),
            ..EngineConfig::default()
        },
        Arc::new(smtp),
    );
    engine.start_all().await.unwrap();
    let identity = engine.identities().await.unwrap()[0].id;

    let chunk: Vec<u8> = (0..CHUNK).map(|i| (i * 31 % 251) as u8).collect();
    let files: Vec<PathBuf> = (0..2)
        .map(|i| {
            let file = engine.upload_dir().join(format!("big{i}"));
            let mut out = std::fs::File::create(&file).unwrap();
            for _ in 0..ATTACHMENT / CHUNK {
                out.write_all(&chunk).unwrap();
            }
            file
        })
        .collect();
    std::fs::write("/proc/self/clear_refs", "5").expect("reset the peak RSS");
    let before = status_kb("VmRSS:");

    let mut ids = Vec::new();
    for (i, file) in files.into_iter().enumerate() {
        let upload = engine
            .add_upload(NewUpload {
                name: format!("big{i}.bin"),
                content_type: "application/octet-stream".to_owned(),
                file,
                size: ATTACHMENT as u64,
            })
            .await
            .unwrap();
        ids.push(upload.id.parse().unwrap());
    }
    let draft = engine
        .create_draft(DraftInput {
            identity_id: identity,
            to: vec![AddressDto {
                name: None,
                email: "ann@x.example".to_owned(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "Big".to_owned(),
            text: Some("See attachment.".to_owned()),
            html: None,
            attachment_ids: ids,
            reply: None,
            forward_of: None,
            save_now: true,
            forwarded: vec![],
        })
        .await
        .unwrap();
    eventually("draft saved", || async {
        engine.draft(draft.id).await.unwrap().saved
    })
    .await;
    engine.send("op-big".to_owned(), draft.id).await.unwrap();
    eventually("sent and copied", || async {
        config
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM outbox WHERE copied_at IS NULL",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
            == 0
    })
    .await;

    let growth = (status_kb("VmHWM:").saturating_sub(before)) * 1024;
    let size = sent_bytes.load(Ordering::SeqCst);
    let appended = sink.appended.lock().unwrap().clone();
    engine.shutdown().await;

    let total = 2 * ATTACHMENT as u64;
    assert!(size > total * 4 / 3, "base64 message sent: {size}");
    assert!(
        appended.iter().any(|(m, n)| m == "Drafts" && *n > total),
        "{appended:?}"
    );
    assert!(
        appended.iter().any(|(m, n)| m == "Sent" && *n == size),
        "{appended:?}"
    );
    println!("peak RSS growth {growth} bytes for a {size}-byte message");
    assert!(
        growth < BUDGET,
        "peak RSS grew {growth} bytes, budget {BUDGET}"
    );
}
