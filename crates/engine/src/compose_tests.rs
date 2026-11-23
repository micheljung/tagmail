//! Drafts, the outbox and Sent copies against the in-memory IMAP server and
//! a recording SMTP sender.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tagmail_api_core::BoxFuture;
use tagmail_api_core::compose::{
    Compose, ComposeError, DraftInput, ForwardedPart, MAX_UPLOAD, NewUpload, ReplyRefs,
};
use tagmail_api_core::compose_dto::OutboxStateDto;
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::identities::NewIdentity;
use tagmail_core::{AccountId, SpecialUse};
use tagmail_imap::Flag;
use tagmail_store::rusqlite::params;
use tagmail_store::{ChangeOp, Changes, ConfigDb, Entity, MasterKey, Vault};
use tempfile::TempDir;
use tokio::time::Instant;

use crate::fake::{FakeConnector, FakeMsg, Shared, lock, server};
use crate::smtp::{SmtpError, SmtpSender, SmtpServer};
use crate::supervisor::{Engine, EngineConfig, Stores};
use crate::testutil::{TestCache, open};

const CAPS: &[&str] = &["IMAP4rev1", "IDLE", "UIDPLUS"];
const ME: &str = "me@a.example";

/// One submission: envelope sender, recipients, message.
type Submission = (String, Vec<String>, Vec<u8>);

#[derive(Default)]
struct FakeSmtp {
    sent: Mutex<Vec<Submission>>,
    /// Errors returned by the next submissions, in order.
    failures: Mutex<Vec<SmtpError>>,
    /// Files every accepted message in this server's Sent, like Gmail.
    saves_into: Option<Shared>,
}

impl FakeSmtp {
    fn sent(&self) -> Vec<Submission> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SmtpSender for FakeSmtp {
    fn send(
        &self,
        _server: SmtpServer,
        from: String,
        recipients: Vec<String>,
        message: PathBuf,
    ) -> BoxFuture<Result<(), SmtpError>> {
        let message = std::fs::read(message).expect("queued message file");
        let failure = {
            let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            (!failures.is_empty()).then(|| failures.remove(0))
        };
        let result = match failure {
            Some(e) => Err(e),
            None => {
                if let Some(srv) = &self.saves_into {
                    lock(srv).append("Sent", FakeMsg::parsed(&message, &[Flag::Seen]));
                }
                self.sent
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((from, recipients, message));
                Ok(())
            }
        };
        Box::pin(async move { result })
    }
}

struct Fixture {
    engine: Engine<FakeConnector>,
    srv: Shared,
    smtp: Arc<FakeSmtp>,
    config: Arc<ConfigDb>,
    dir: TempDir,
    cache: TestCache,
}

async fn fixture(saves_sent: Option<bool>, smtp: FakeSmtp) -> Fixture {
    fixture_with(saves_sent, smtp, Duration::ZERO).await
}

async fn fixture_with(saves_sent: Option<bool>, smtp: FakeSmtp, debounce: Duration) -> Fixture {
    let srv = smtp.saves_into.clone().unwrap_or_else(|| server(CAPS));
    lock(&srv).add_mailbox("Drafts", Some(SpecialUse::Drafts), 400);
    let dir = TempDir::new().unwrap();
    let config = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let sealed = vault.seal(AccountId(1), "secret").unwrap();
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, max_connections, \
                 server_saves_sent) \
                 VALUES (1, 'a', 'imap', 993, 'tls', 'smtp', 465, 'tls', ?1, ?2, ?3, 2, ?4)",
                params![ME, sealed.ciphertext, sealed.nonce, saves_sent],
            )?;
            tx.execute(
                "INSERT INTO identity (account_id, address, is_default) VALUES (1, ?1, 1)",
                [ME],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let cache = open();
    let stores = Stores {
        config: config.clone(),
        vault: Arc::new(vault),
        cache: cache.ctx.cache.clone(),
        blobs: cache.ctx.blobs.clone(),
        feed: cache.feed.clone(),
        uploads: dir.path().join("uploads"),
        outbox: dir.path().join("outbox"),
    };
    let smtp = Arc::new(smtp);
    let engine = Engine::with_smtp(
        stores,
        FakeConnector(srv.clone()),
        EngineConfig {
            poll_interval: Duration::from_secs(3600),
            retry_min: Duration::from_millis(50),
            retry_max: Duration::from_millis(200),
            draft_debounce: debounce,
            sent_probe: Duration::from_millis(300),
            outbox_retry_min: Duration::from_secs(1),
            outbox_retry_max: Duration::from_secs(2),
            max_send_attempts: 2,
            ..EngineConfig::default()
        },
        smtp.clone(),
    );
    engine.start_all().await.unwrap();
    Fixture {
        engine,
        srv,
        smtp,
        config,
        dir,
        cache,
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn addr(email: &str) -> AddressDto {
    AddressDto {
        name: None,
        email: email.to_owned(),
    }
}

fn input(identity: i64, subject: &str) -> DraftInput {
    DraftInput {
        identity_id: identity,
        to: vec![addr("ann@x.example")],
        cc: vec![],
        bcc: vec![addr("hidden@y.example")],
        subject: subject.to_owned(),
        text: Some(format!("Body of {subject}")),
        html: None,
        attachment_ids: vec![],
        reply: None,
        forward_of: None,
        save_now: false,
        forwarded: vec![],
    }
}

impl Fixture {
    /// (subject, Message-ID, flags) of each message in `mailbox`.
    fn messages(&self, mailbox: &str) -> Vec<(String, String, Vec<Flag>)> {
        lock(&self.srv)
            .mailbox(mailbox)
            .msgs
            .values()
            .map(|m| (m.subject.clone(), m.id.clone(), m.flags.clone()))
            .collect()
    }

    async fn identity(&self) -> i64 {
        let ids = self.engine.identities().await.unwrap();
        assert_eq!(ids.len(), 1);
        ids[0].id
    }

    async fn upload(&self, name: &str, bytes: &[u8], size: u64) -> i64 {
        let file = self.engine.upload_dir().join(format!("t-{name}"));
        std::fs::write(&file, bytes).unwrap();
        self.engine
            .add_upload(NewUpload {
                name: name.to_owned(),
                content_type: "application/octet-stream".to_owned(),
                file,
                size,
            })
            .await
            .unwrap()
            .id
            .parse()
            .unwrap()
    }

    /// Every outbox item is sent and in Sent.
    async fn settled(&self) -> bool {
        self.config
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
    }

    async fn only_item(&self) -> tagmail_api_core::compose_dto::OutboxItemDto {
        let mut items = self.engine.outbox().await.unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        items.remove(0)
    }

    async fn cached(&self, msgid: &'static str) -> bool {
        self.cache
            .query("SELECT msgid_hdr FROM message", |r| {
                r.get::<_, Option<String>>(0)
            })
            .await
            .iter()
            .any(|m| m.as_deref() == Some(msgid))
    }

    fn flags(&self, mailbox: &str, msgid: &str) -> Vec<Flag> {
        lock(&self.srv)
            .mailbox(mailbox)
            .msgs
            .values()
            .find(|m| m.id == msgid)
            .map(|m| m.flags.clone())
            .unwrap_or_default()
    }

    async fn saves_sent(&self) -> Option<bool> {
        self.config
            .read(|c| Ok(c.query_row("SELECT server_saves_sent FROM account", [], |r| r.get(0))?))
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn identity_changes_are_published() {
    let f = fixture(None, FakeSmtp::default()).await;
    let first = f.identity().await;
    let since = f.cache.ctx.cache.head().await.unwrap();
    let alias = f
        .engine
        .create_identity(NewIdentity {
            account_id: AccountId(1),
            address: "alias@a.example".to_owned(),
            name: None,
            signature: Some("<p>sig</p>".to_owned()),
            is_default: true,
        })
        .await
        .unwrap();
    f.engine.delete_identity(alias.id).await.unwrap();
    let ids = f.engine.identities().await.unwrap();
    assert_eq!(ids.len(), 1);
    assert!(ids[0].id == first && ids[0].is_default);
    let page = f.cache.ctx.cache.changes_since(since, 100).await.unwrap();
    let Changes::Page { changes, .. } = page else {
        panic!("gone")
    };
    let events: Vec<(i64, ChangeOp)> = changes
        .iter()
        .filter(|c| c.entity == Entity::Identity)
        .map(|c| (c.entity_id, c.op))
        .collect();
    assert_eq!(
        events,
        [
            (alias.id, ChangeOp::Upsert),
            (first, ChangeOp::Upsert),
            (alias.id, ChangeOp::Delete),
            (first, ChangeOp::Upsert),
        ]
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn drafts_are_saved_to_imap_and_replace_the_previous_version() {
    let f = fixture(None, FakeSmtp::default()).await;
    let identity = f.identity().await;
    let file = f.upload("notes.txt", b"attached", 8).await;
    let mut first = input(identity, "First");
    first.attachment_ids = vec![file];
    let draft = f.engine.create_draft(first).await.unwrap();
    assert_eq!(draft.attachments.len(), 1);
    eventually("first version in Drafts", || async {
        f.messages("Drafts").iter().any(|m| m.0 == "First")
    })
    .await;
    let saved = f.messages("Drafts");
    assert_eq!(saved.len(), 1);
    assert!(saved[0].2.contains(&Flag::Draft), "{saved:?}");
    let raw = lock(&f.srv)
        .mailbox("Drafts")
        .msgs
        .values()
        .next()
        .unwrap()
        .raw();
    assert!(raw.contains("hidden@y.example"), "drafts keep Bcc: {raw}");
    assert!(raw.contains("notes.txt"), "{raw}");
    eventually("saved flag", || async {
        f.engine.draft(draft.id).await.unwrap().saved
    })
    .await;

    let updated = f
        .engine
        .update_draft(draft.id, input(identity, "Second"))
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    assert!(updated.attachments.is_empty());
    eventually("only the second version in Drafts", || async {
        let msgs = f.messages("Drafts");
        msgs.len() == 1 && msgs[0].0 == "Second"
    })
    .await;
    assert_eq!(f.messages("Drafts")[0].1, saved[0].1, "same Message-ID");

    f.engine.delete_draft(draft.id).await.unwrap();
    eventually("Drafts emptied", || async {
        f.messages("Drafts").is_empty()
    })
    .await;
    assert!(matches!(
        f.engine.draft(draft.id).await,
        Err(ComposeError::NotFound("draft"))
    ));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn send_strips_bcc_copies_to_sent_once_and_learns_the_server() {
    let f = fixture(None, FakeSmtp::default()).await;
    let identity = f.identity().await;
    lock(&f.srv).append(
        "INBOX",
        FakeMsg::new(
            "orig@x.example",
            "Plans",
            "ann@x.example",
            ME,
            1_790_000_000,
        ),
    );
    let mut reply = input(identity, "Re: Plans");
    reply.reply = Some(ReplyRefs {
        in_reply_to: "orig@x.example".to_owned(),
        references: vec!["orig@x.example".to_owned()],
    });
    reply.forwarded = vec![ForwardedPart {
        name: "r.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        bytes: b"%PDF".to_vec(),
    }];
    eventually("original cached", || f.cached("orig@x.example")).await;
    let draft = f.engine.create_draft(reply).await.unwrap();
    assert_eq!(
        draft.in_reply_to_message_id.as_deref(),
        Some("orig@x.example")
    );
    assert_eq!(draft.attachments[0].name, "r.pdf");

    let out = f.engine.send("op-1".to_owned(), draft.id).await.unwrap();
    assert!(out.created);
    assert_eq!(out.status.status, OutboxStateDto::Queued);
    assert!(matches!(
        f.engine.update_draft(draft.id, input(identity, "x")).await,
        Err(ComposeError::Conflict(_))
    ));
    eventually("sent and completed", || f.settled()).await;
    eventually("original flagged \\Answered", || async {
        f.flags("INBOX", "orig@x.example").contains(&Flag::Answered)
    })
    .await;

    let sent = f.smtp.sent();
    assert_eq!(sent.len(), 1);
    let (from, rcpt, mime) = &sent[0];
    assert_eq!(from, ME);
    assert_eq!(rcpt, &["ann@x.example", "hidden@y.example"]);
    let text = String::from_utf8_lossy(mime);
    assert!(!text.contains("hidden@y.example"), "{text}");
    assert!(text.contains("In-Reply-To: <orig@x.example>"), "{text}");
    assert!(text.contains("r.pdf"), "{text}");

    let copies = f.messages("Sent");
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].0, "Re: Plans");
    assert_eq!(
        lock(&f.srv)
            .mailbox("Sent")
            .msgs
            .values()
            .next()
            .unwrap()
            .refs,
        ["orig@x.example"]
    );
    assert_eq!(
        f.saves_sent().await,
        Some(false),
        "learned: the server does not save Sent"
    );
    eventually("draft removed from Drafts", || async {
        f.messages("Drafts").is_empty()
    })
    .await;
    assert!(matches!(
        f.engine.draft(draft.id).await,
        Err(ComposeError::NotFound(_))
    ));

    let again = f.engine.send("op-1".to_owned(), draft.id).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.status.status, OutboxStateDto::Sent);
    assert_eq!(f.smtp.sent().len(), 1, "op ids never send twice");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn server_that_saves_sent_gets_no_second_copy() {
    for preset in [Some(true), None] {
        let srv = server(CAPS);
        let smtp = FakeSmtp {
            saves_into: Some(srv.clone()),
            ..FakeSmtp::default()
        };
        let f = fixture(preset, smtp).await;
        let identity = f.identity().await;
        let draft = f
            .engine
            .create_draft(input(identity, "Gmail"))
            .await
            .unwrap();
        f.engine.send("op-g".to_owned(), draft.id).await.unwrap();
        eventually("completed", || f.settled()).await;
        assert_eq!(f.messages("Sent").len(), 1, "{preset:?}");
        assert_eq!(f.saves_sent().await, Some(true));
        f.engine.shutdown().await;
    }
}

#[tokio::test]
async fn sent_probe_reads_each_sent_header_once() {
    let f = fixture(None, FakeSmtp::default()).await;
    lock(&f.srv).header_search_empty = true;
    for i in 0..3 {
        let id = format!("old{i}@x.example");
        lock(&f.srv).append("Sent", FakeMsg::new(&id, "Old", ME, "a@x", 1));
    }
    let identity = f.identity().await;
    let draft = f
        .engine
        .create_draft(input(identity, "Probe"))
        .await
        .unwrap();
    f.engine.send("op-p".to_owned(), draft.id).await.unwrap();
    eventually("sent and completed", || f.settled()).await;
    assert_eq!(f.messages("Sent").len(), 4);
    let mut scanned: Vec<u32> = lock(&f.srv)
        .header_fetches
        .iter()
        .filter(|(mailbox, _)| mailbox == "Sent")
        .flat_map(|(_, uids)| uids.clone())
        .collect();
    scanned.sort_unstable();
    assert_eq!(scanned, [1, 2, 3], "each Sent header read once");
    f.engine.shutdown().await;
}

async fn insert_sending(f: &Fixture, op: &'static str, msgid: &'static str, mime: Vec<u8>) {
    let name = format!("{op}.eml");
    std::fs::write(f.dir.path().join("outbox").join(&name), &mime).unwrap();
    f.config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO outbox (op_id, account_id, mime_file, mime_size, msgid_hdr, sender, \
                 recipients, subject, state, attempts) \
                 VALUES (?1, 1, ?2, ?3, ?4, ?5, '[{\"email\":\"ann@x.example\"}]', 'Crash', \
                 'sending', 1)",
                params![op, name, i64::try_from(mime.len()).unwrap(), msgid, ME],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn crash_after_data_does_not_resend_when_sent_has_it() {
    let f = fixture(Some(true), FakeSmtp::default()).await;
    let mime = b"Message-ID: <crash@a.example>\r\nFrom: me@a.example\r\nTo: ann@x.example\r\n\
                 Subject: Crash\r\n\r\nbody"
        .to_vec();
    lock(&f.srv).append("Sent", FakeMsg::parsed(&mime, &[Flag::Seen]));
    insert_sending(&f, "op-c", "crash@a.example", mime.clone()).await;
    f.engine.inner.outbox_wake.notify_one();
    eventually("recovered", || f.settled()).await;
    assert!(f.smtp.sent().is_empty(), "found in Sent: not resent");
    assert_eq!(f.messages("Sent").len(), 1);

    let mime = String::from_utf8(mime).unwrap().replace("crash@", "lost@");
    insert_sending(&f, "op-l", "lost@a.example", mime.into_bytes()).await;
    f.engine.inner.outbox_wake.notify_one();
    eventually("resent", || f.settled()).await;
    assert_eq!(
        f.smtp.sent().len(),
        1,
        "not in Sent: sent again (at-least-once)"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn failures_show_in_the_outbox_and_can_be_retried_or_cancelled() {
    let permanent = SmtpError {
        permanent: true,
        message: "550 mailbox unavailable".to_owned(),
    };
    let transient = SmtpError {
        permanent: false,
        message: "421 try later".to_owned(),
    };
    let smtp = FakeSmtp {
        failures: Mutex::new(vec![permanent, transient]),
        ..FakeSmtp::default()
    };
    let f = fixture(Some(false), smtp).await;
    let identity = f.identity().await;
    let draft = f
        .engine
        .create_draft(input(identity, "Bounce"))
        .await
        .unwrap();
    f.engine.send("op-f".to_owned(), draft.id).await.unwrap();
    eventually("failed", || async {
        f.engine
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|i| i.status == OutboxStateDto::Failed)
    })
    .await;
    let item = f.only_item().await;
    assert_eq!(item.error.as_deref(), Some("550 mailbox unavailable"));
    assert_eq!(item.to, [addr("ann@x.example"), addr("hidden@y.example")]);
    assert!(matches!(
        f.engine.retry(item.id + 100).await,
        Err(ComposeError::NotFound(_))
    ));

    f.engine.retry(item.id).await.unwrap();
    eventually("transient failure requeued", || async {
        f.engine.outbox().await.unwrap().iter().any(|i| {
            i.status == OutboxStateDto::Queued && i.error.as_deref() == Some("421 try later")
        })
    })
    .await;
    f.engine.cancel(item.id).await.unwrap();
    assert!(f.engine.outbox().await.unwrap().is_empty());
    let editable = f
        .engine
        .update_draft(draft.id, input(identity, "Fixed"))
        .await
        .unwrap();
    assert!(!editable.queued);
    let out = f.engine.send("op-f".to_owned(), draft.id).await.unwrap();
    assert!(out.created, "a cancelled op id may be sent again");
    eventually("sent", || f.settled()).await;
    assert_eq!(f.smtp.sent().len(), 1);
    assert_eq!(f.messages("Sent").len(), 1);
    let sent = f.only_item().await;
    assert_eq!((sent.status, sent.error), (OutboxStateDto::Sent, None));
    assert!(sent.sent_at.is_some(), "sent items stay listed");

    let events = f
        .cache
        .query(
            "SELECT entity_id, op, payload FROM change_log WHERE entity = 'outbox' ORDER BY seq",
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ))
            },
        )
        .await;
    let has = |id: i64, op: &str, part: &str| {
        events
            .iter()
            .any(|(i, o, p)| *i == id && o == op && p.contains(part))
    };
    assert!(has(item.id, "upsert", r#""status":"failed""#), "{events:?}");
    assert!(has(item.id, "delete", ""), "cancel: {events:?}");
    assert!(has(sent.id, "upsert", r#""status":"sent""#), "{events:?}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn attachment_caps_are_enforced_and_orphans_cleaned() {
    let f = fixture(None, FakeSmtp::default()).await;
    let identity = f.identity().await;
    let file = f.engine.upload_dir().join("big");
    std::fs::write(&file, b"x").unwrap();
    let too_big = f
        .engine
        .add_upload(NewUpload {
            name: "big".to_owned(),
            content_type: "application/octet-stream".to_owned(),
            file: file.clone(),
            size: MAX_UPLOAD + 1,
        })
        .await;
    assert!(matches!(too_big, Err(ComposeError::TooLarge(_))));
    assert!(!file.exists());

    let twenty = 20 * 1024 * 1024;
    let a = f.upload("a", b"a", twenty).await;
    let b = f.upload("b", b"b", twenty).await;
    let mut over = input(identity, "Big");
    over.attachment_ids = vec![a, b];
    assert!(matches!(
        f.engine.create_draft(over).await,
        Err(ComposeError::TooLarge(_))
    ));
    let mut fits = input(identity, "Fits");
    fits.attachment_ids = vec![a];
    let draft = f.engine.create_draft(fits).await.unwrap();
    let mut other = input(identity, "Other");
    other.attachment_ids = vec![a];
    assert!(
        matches!(
            f.engine.create_draft(other).await,
            Err(ComposeError::Invalid {
                field: "attachmentIds",
                ..
            })
        ),
        "an attachment belongs to one draft"
    );
    f.engine.delete_draft(draft.id).await.unwrap();
    let uploads = f.dir.path().join("uploads");
    assert!(
        !uploads.join(a.to_string()).exists(),
        "deleted with its draft"
    );
    assert!(
        uploads.join(b.to_string()).exists(),
        "orphan kept for the grace period"
    );
    f.engine.delete_upload(b).await.unwrap();
    assert!(!uploads.join(b.to_string()).exists());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn forwarding_flags_the_original() {
    let f = fixture(Some(true), FakeSmtp::default()).await;
    let identity = f.identity().await;
    lock(&f.srv).append(
        "Archive",
        FakeMsg::new("fw@x.example", "Report", "ann@x.example", ME, 1_790_000_000),
    );
    eventually("original cached", || f.cached("fw@x.example")).await;
    let mut forward = input(identity, "Fwd: Report");
    forward.forward_of = Some("fw@x.example".to_owned());
    let draft = f.engine.create_draft(forward).await.unwrap();
    f.engine.send("op-fw".to_owned(), draft.id).await.unwrap();
    eventually("original flagged $Forwarded", || async {
        f.flags("Archive", "fw@x.example")
            .contains(&Flag::Keyword("$Forwarded".to_owned()))
    })
    .await;
    assert!(!f.flags("Archive", "fw@x.example").contains(&Flag::Answered));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn text_edits_wait_for_idle_but_attachments_and_closing_save_at_once() {
    let f = fixture_with(None, FakeSmtp::default(), Duration::from_secs(3600)).await;
    let identity = f.identity().await;
    let draft = f
        .engine
        .create_draft(input(identity, "Typing"))
        .await
        .unwrap();
    let edited = f
        .engine
        .update_draft(draft.id, input(identity, "Typing more"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(f.messages("Drafts").is_empty(), "text edits wait for idle");
    assert!(!edited.saved);

    let file = f.upload("a.txt", b"attached", 8).await;
    let mut attached = input(identity, "With attachment");
    attached.attachment_ids = vec![file];
    f.engine
        .update_draft(draft.id, attached.clone())
        .await
        .unwrap();
    eventually("saved after the attachment change", || async {
        f.messages("Drafts")
            .iter()
            .any(|m| m.0 == "With attachment")
    })
    .await;

    attached.subject = "Closed".to_owned();
    f.engine
        .update_draft(draft.id, attached.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        f.messages("Drafts").iter().all(|m| m.0 != "Closed"),
        "same attachments: no new APPEND"
    );
    attached.save_now = true;
    f.engine.update_draft(draft.id, attached).await.unwrap();
    eventually("saved when compose closes", || async {
        let msgs = f.messages("Drafts");
        msgs.len() == 1 && msgs[0].0 == "Closed"
    })
    .await;
    f.engine.shutdown().await;
}
