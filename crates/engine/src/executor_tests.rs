//! Ops and label jobs end to end against the fake server: optimistic
//! overlay, execution on every copy, failures, restarts and cache wipes.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tagmail_api_core::ops_dto::{LabelPatchDto, NewLabelDto, OpStatusDto};
use tagmail_api_core::{ChangeFeed, OpRequest, Submitted};
use tagmail_core::ops::OpKind;
use tagmail_core::{AccountId, ChangeCursor, MailboxRowId, SpecialUse, ThreadId};
use tagmail_imap::{Flag, PoolConfig};
use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey, Vault};
use tempfile::TempDir;
use tokio::time::Instant;

use crate::fake::{FakeConnector, FakeMsg, Shared, lock, server};
use crate::supervisor::{Engine, EngineConfig, Stores};

const CAPS: &[&str] = &[
    "IMAP4rev1",
    "ENABLE",
    "IDLE",
    "CONDSTORE",
    "QRESYNC",
    "UIDPLUS",
    "MOVE",
];

struct Fixture {
    engine: Engine<FakeConnector>,
    stores: Stores,
    dir: Arc<TempDir>,
}

async fn config_db(path: &Path) -> (Arc<ConfigDb>, Arc<Vault>) {
    let config = Arc::new(ConfigDb::open(&path.join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let sealed = vault.seal(AccountId(1), "secret").unwrap();
    config
        .write(move |tx| {
            tx.execute(
                "INSERT OR IGNORE INTO account (id, name, imap_host, imap_port, imap_tls, \
                 smtp_host, smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                 VALUES (1, 'a', 'imap', 993, 'tls', 'smtp', 465, 'tls', 'u', ?1, ?2)",
                tagmail_store::rusqlite::params![sealed.ciphertext, sealed.nonce],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (config, Arc::new(vault))
}

fn open_cache(dir: &Path) -> (Arc<CacheDb>, Arc<BlobStore>, Arc<ChangeFeed>) {
    let cache = Arc::new(CacheDb::open(&dir.join("cache.db")).unwrap());
    let blobs = Arc::new(BlobStore::open(&dir.join("blobs")).unwrap());
    let feed = Arc::new(ChangeFeed::new(ChangeCursor {
        epoch: cache.epoch(),
        seq: 0,
    }));
    (cache, blobs, feed)
}

fn engine(srv: &Shared, stores: Stores) -> Engine<FakeConnector> {
    let config = EngineConfig {
        poll_interval: Duration::from_millis(200),
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        pool: PoolConfig {
            auth_backoff_base: Duration::from_secs(60),
            ..PoolConfig::default()
        },
        ..EngineConfig::default()
    };
    Engine::new(stores, FakeConnector(srv.clone()), config)
}

async fn fixture(srv: &Shared) -> Fixture {
    let dir = Arc::new(TempDir::new().unwrap());
    let (config, vault) = config_db(dir.path()).await;
    let (cache, blobs, feed) = open_cache(dir.path());
    let stores = Stores {
        config,
        vault,
        cache,
        blobs,
        feed,
        uploads: dir.path().join("uploads"),
    };
    Fixture {
        engine: engine(srv, stores.clone()),
        stores,
        dir,
    }
}

impl Fixture {
    /// The same databases under a new engine, as after a restart; with
    /// `wipe` the cache and blobs start empty.
    async fn restart(self, srv: &Shared, wipe: bool) -> Fixture {
        self.engine.shutdown().await;
        let mut stores = self.stores.clone();
        if wipe {
            for f in ["cache.db", "cache.db-wal", "cache.db-shm"] {
                let _ = std::fs::remove_file(self.dir.path().join(f));
            }
            std::fs::remove_dir_all(self.dir.path().join("blobs")).unwrap();
            let (cache, blobs, feed) = open_cache(self.dir.path());
            stores = Stores {
                cache,
                blobs,
                feed,
                ..stores
            };
        }
        Fixture {
            engine: engine(srv, stores.clone()),
            stores,
            dir: self.dir,
        }
    }

    async fn query<T: Send + 'static>(
        &self,
        sql: &'static str,
        f: impl Fn(&tagmail_store::rusqlite::Row<'_>) -> tagmail_store::rusqlite::Result<T>
        + Send
        + 'static,
    ) -> Vec<T> {
        self.stores
            .cache
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

    async fn all_live(&self) -> bool {
        let states = self
            .query("SELECT sync_state FROM mailbox", |r| r.get::<_, String>(0))
            .await;
        !states.is_empty() && states.iter().all(|s| s == "live")
    }

    async fn thread_of(&self, msgid: &'static str) -> ThreadId {
        let rows = self
            .query("SELECT thread_id, msgid_hdr FROM message", |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .await;
        ThreadId(
            rows.into_iter()
                .find(|(_, m)| m.as_deref() == Some(msgid))
                .unwrap()
                .0,
        )
    }

    async fn mailbox_id(&self, name: &'static str) -> i64 {
        self.query("SELECT id, name FROM mailbox", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .await
        .into_iter()
        .find(|(_, n)| n == name)
        .unwrap()
        .0
    }

    /// Mailbox names whose thread list shows `thread`.
    async fn lists(&self, thread: ThreadId) -> Vec<String> {
        let mut out: Vec<String> = self
            .query(
                "SELECT tm.thread_id, mb.name FROM thread_mailbox tm \
                 JOIN mailbox mb ON mb.id = tm.mailbox_id",
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .await
            .into_iter()
            .filter(|(t, _)| *t == thread.0)
            .map(|(_, n)| n)
            .collect();
        out.sort();
        out
    }

    async fn unread(&self, thread: ThreadId) -> u32 {
        self.query("SELECT id, unread_count FROM thread", |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?))
        })
        .await
        .into_iter()
        .find(|(t, _)| *t == thread.0)
        .unwrap()
        .1
    }

    async fn labels_of(&self, msgid: &'static str) -> Vec<String> {
        let mut out: Vec<String> = self
            .query(
                "SELECT m.msgid_hdr, l.label_key FROM message_label ml \
                 JOIN message m ON m.id = ml.message_id JOIN label l ON l.id = ml.label_id",
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )
            .await
            .into_iter()
            .filter(|(m, _)| m.as_deref() == Some(msgid))
            .map(|(_, k)| k)
            .collect();
        out.sort();
        out
    }

    async fn status(&self, op_id: &str) -> OpStatusDto {
        self.engine.op_dto(op_id.to_owned()).await.unwrap().status
    }
}

async fn resync(s: &mut crate::fake::FakeSession, ctx: &crate::ctx::Ctx) {
    for row in crate::sync::list(s, ctx).await.unwrap() {
        crate::sync::sync_mailbox(s, ctx, &row, &|| false)
            .await
            .unwrap();
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

fn op(id: &str, kind: OpKind, thread: ThreadId) -> OpRequest {
    OpRequest {
        op_id: id.to_owned(),
        kind,
        thread_ids: vec![thread],
        email_ids: Vec::new(),
        label: None,
        mailbox: None,
    }
}

fn created(s: Submitted) -> tagmail_api_core::ops_dto::OpDto {
    match s {
        Submitted::Created(dto) => dto,
        Submitted::Existing(dto) => panic!("not created: {dto:?}"),
    }
}

fn msg(id: &str, subject: &str, date: i64) -> FakeMsg {
    FakeMsg::new(id, subject, "ann@x", "me@x", date)
}

#[tokio::test]
async fn label_applies_to_every_copy_at_once_and_on_the_server() {
    let srv = server(CAPS);
    let uid = lock(&srv).append("INBOX", msg("a1@x", "Plan", 1_790_000_000));
    let copy = lock(&srv).copy_msg("INBOX", uid, "Archive");
    lock(&srv).set_flags("Archive", copy, &[Flag::Keyword("$label_Other".into())]);
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    f.engine
        .create_label(NewLabelDto {
            name: "Work".into(),
            color: Some("#AA0000".into()),
        })
        .await
        .unwrap();
    let thread = f.thread_of("a1@x").await;

    let mut add = op("op-1", OpKind::LabelAdd, thread);
    add.label = Some("work".into());
    let dto = created(f.engine.queue_op(add.clone()).await.unwrap());
    assert_eq!(dto.status, OpStatusDto::Pending);
    assert!(
        f.labels_of("a1@x").await.contains(&"work".to_owned()),
        "visible before the server has it"
    );

    eventually("keyword on both copies", || async {
        let mut s = lock(&srv);
        let inbox = s.flags_of("INBOX", uid);
        let archive = s.flags_of("Archive", copy);
        inbox == ["$label_Work"] && archive == ["$label_Other", "$label_Work"]
    })
    .await;
    eventually("op done", || async {
        f.status("op-1").await == OpStatusDto::Done
    })
    .await;
    eventually("labels kept after the op", || async {
        f.labels_of("a1@x").await == ["other", "work"]
    })
    .await;
    match f.engine.queue_op(add).await.unwrap() {
        Submitted::Existing(d) => assert_eq!(d.status, OpStatusDto::Done),
        Submitted::Created(_) => panic!("replayed op ran twice"),
    }

    let mut remove = op("op-2", OpKind::LabelRemove, thread);
    remove.label = Some("other".into());
    created(f.engine.queue_op(remove).await.unwrap());
    assert_eq!(f.labels_of("a1@x").await, ["work"]);
    eventually("keyword removed from the one copy", || async {
        lock(&srv).flags_of("Archive", copy) == ["$label_Work"]
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn failed_op_reverts_the_overlay_until_retried() {
    let srv = server(CAPS);
    let uid = lock(&srv).append("INBOX", msg("r1@x", "Read me", 1_790_000_000));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("r1@x").await;
    lock(&srv).fail_stores = true;

    created(
        f.engine
            .queue_op(op("read-1", OpKind::MarkRead, thread))
            .await
            .unwrap(),
    );
    assert_eq!(f.unread(thread).await, 0, "optimistic");
    eventually("visible failure", || async {
        f.status("read-1").await == OpStatusDto::Failed
    })
    .await;
    let failed = f.engine.list_ops(Some(OpStatusDto::Failed)).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]
            .error
            .as_deref()
            .unwrap()
            .contains("store refused")
    );
    eventually("reverted to the server state", || async {
        f.unread(thread).await == 1
    })
    .await;

    lock(&srv).fail_stores = false;
    f.engine.retry_failed_op("read-1".into()).await.unwrap();
    assert_eq!(f.unread(thread).await, 0);
    eventually("applied after retry", || async {
        lock(&srv).flags_of("INBOX", uid) == ["\\Seen"]
    })
    .await;
    eventually("done", || async {
        f.status("read-1").await == OpStatusDto::Done
    })
    .await;

    lock(&srv).fail_stores = true;
    created(
        f.engine
            .queue_op(op("unread-1", OpKind::MarkUnread, thread))
            .await
            .unwrap(),
    );
    eventually("second failure", || async {
        f.status("unread-1").await == OpStatusDto::Failed
    })
    .await;
    f.engine.discard_op_id("unread-1".into()).await.unwrap();
    assert!(
        matches!(
            f.engine
                .queue_op(op("unread-1", OpKind::MarkUnread, thread))
                .await,
            Err(tagmail_api_core::WriteError::Conflict(_))
        ),
        "a discarded op id is never reused"
    );
    assert_eq!(f.unread(thread).await, 0);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn archive_leaves_inbox_at_once_and_moves_on_the_server() {
    let srv = server(CAPS);
    lock(&srv).append("INBOX", msg("ar@x", "Old news", 1_790_000_000));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("ar@x").await;
    assert_eq!(f.lists(thread).await, ["INBOX"]);

    created(
        f.engine
            .queue_op(op("arch-1", OpKind::Archive, thread))
            .await
            .unwrap(),
    );
    assert_eq!(f.lists(thread).await, ["Archive"], "optimistic");
    eventually("moved on the server", || async {
        let mut s = lock(&srv);
        s.uid_of("INBOX", "ar@x").is_none() && s.uid_of("Archive", "ar@x").is_some()
    })
    .await;
    eventually("done", || async {
        f.status("arch-1").await == OpStatusDto::Done
    })
    .await;
    eventually("cache follows", || async {
        f.lists(thread).await == ["Archive"]
    })
    .await;

    created(
        f.engine
            .queue_op(op("unarch-1", OpKind::Unarchive, thread))
            .await
            .unwrap(),
    );
    assert_eq!(f.lists(thread).await, ["INBOX"]);
    eventually("back in INBOX", || async {
        lock(&srv).uid_of("INBOX", "ar@x").is_some()
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn gmail_mode_archives_by_leaving_inbox_for_all() {
    let srv = server(CAPS);
    lock(&srv).add_mailbox("[Gmail]/All", Some(SpecialUse::All), 400);
    let uid = lock(&srv).append("INBOX", msg("g1@x", "Gmail", 1_790_000_000));
    lock(&srv).copy_msg("INBOX", uid, "[Gmail]/All");
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("g1@x").await;

    created(
        f.engine
            .queue_op(op("g-arch", OpKind::Archive, thread))
            .await
            .unwrap(),
    );
    assert_eq!(f.lists(thread).await, ["[Gmail]/All"]);
    eventually("left INBOX", || async {
        lock(&srv).uid_of("INBOX", "g1@x").is_none()
    })
    .await;
    assert!(
        lock(&srv).uid_of("Archive", "g1@x").is_none(),
        "Gmail mode never uses \\Archive"
    );
    eventually("done", || async {
        f.status("g-arch").await == OpStatusDto::Done
    })
    .await;

    created(
        f.engine
            .queue_op(op("g-unarch", OpKind::Unarchive, thread))
            .await
            .unwrap(),
    );
    eventually("copied back to INBOX", || async {
        lock(&srv).uid_of("INBOX", "g1@x").is_some()
    })
    .await;
    assert!(
        lock(&srv).uid_of("[Gmail]/All", "g1@x").is_some(),
        "still in \\All"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn move_and_trash_fail_visibly_without_a_destination() {
    let srv = server(CAPS);
    lock(&srv).add_mailbox("Projects", None, 500);
    lock(&srv).append("INBOX", msg("mv@x", "Move me", 1_790_000_000));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("mv@x").await;

    created(
        f.engine
            .queue_op(op("trash-1", OpKind::Trash, thread))
            .await
            .unwrap(),
    );
    eventually("no Trash", || async {
        f.status("trash-1").await == OpStatusDto::Failed
    })
    .await;
    let err = f
        .engine
        .op_dto("trash-1".into())
        .await
        .unwrap()
        .error
        .unwrap();
    assert!(err.contains("\\Trash"), "{err}");
    eventually("back in INBOX, not misapplied", || async {
        f.lists(thread).await == ["INBOX"]
    })
    .await;

    let projects = f.mailbox_id("Projects").await;
    let mut mv = op("move-1", OpKind::Move, thread);
    mv.mailbox = Some(MailboxRowId(projects));
    created(f.engine.queue_op(mv).await.unwrap());
    assert_eq!(f.lists(thread).await, ["Projects"]);
    eventually("moved", || async {
        lock(&srv).uid_of("Projects", "mv@x").is_some()
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn queued_ops_survive_a_cache_wipe_and_apply_after_resync() {
    let srv = server(CAPS);
    let a = lock(&srv).append("INBOX", msg("w1@x", "Wiped", 1_790_000_000));
    lock(&srv).append("INBOX", msg("w2@x", "Kept", 1_790_000_100));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    f.engine
        .create_label(NewLabelDto {
            name: "Keep".into(),
            color: None,
        })
        .await
        .unwrap();
    let t1 = f.thread_of("w1@x").await;
    let t2 = f.thread_of("w2@x").await;
    f.engine.shutdown().await;

    let mut label = op("q-label", OpKind::LabelAdd, t1);
    label.label = Some("keep".into());
    created(f.engine.queue_op(label).await.unwrap());
    created(
        f.engine
            .queue_op(op("q-arch", OpKind::Archive, t2))
            .await
            .unwrap(),
    );

    let f = f.restart(&srv, true).await;
    assert!(
        f.query("SELECT id FROM message", |r| r.get::<_, i64>(0))
            .await
            .is_empty()
    );
    f.engine.start_all().await.unwrap();
    eventually("both applied", || async {
        let mut s = lock(&srv);
        s.flags_of("INBOX", a) == ["$label_Keep"]
            && s.uid_of("INBOX", "w2@x").is_none()
            && s.uid_of("Archive", "w2@x").is_some()
    })
    .await;
    eventually("done", || async {
        f.status("q-label").await == OpStatusDto::Done
            && f.status("q-arch").await == OpStatusDto::Done
    })
    .await;
    eventually("label cached", || async {
        f.labels_of("w1@x").await == ["keep"]
    })
    .await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn overlay_is_rebuilt_over_resynced_rows_and_echoes_do_not_clobber_it() {
    let srv = server(CAPS);
    let uid = lock(&srv).append("INBOX", msg("o1@x", "Overlay", 1_790_000_000));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("o1@x").await;
    f.engine.shutdown().await;
    created(
        f.engine
            .queue_op(op("ov-1", OpKind::Star, thread))
            .await
            .unwrap(),
    );

    let f = f.restart(&srv, true).await;
    f.engine.restore_overlays().await.unwrap();
    let ctx = f.engine.ctx(AccountId(1));
    let mut session = FakeConnector(srv.clone()).session();
    resync(&mut session, &ctx).await;
    let starred = || async {
        f.query("SELECT starred FROM thread", |r| r.get::<_, bool>(0))
            .await
    };
    assert_eq!(starred().await, [true], "projected over the rebuilt cache");

    lock(&srv).set_flags("INBOX", uid, &[Flag::Seen]);
    resync(&mut session, &ctx).await;
    let thread = f.thread_of("o1@x").await;
    assert_eq!(starred().await, [true], "the echo keeps the overlay");
    assert_eq!(
        f.unread(thread).await,
        0,
        "and takes the server's other flags"
    );

    f.engine.discard_op_id("ov-1".into()).await.unwrap();
    assert_eq!(starred().await, [false], "back to the server state");
}

#[tokio::test]
async fn ops_find_messages_again_after_uidvalidity_changes() {
    let srv = server(CAPS);
    lock(&srv).add_mailbox("Projects", None, 500);
    lock(&srv).append("INBOX", msg("u1@x", "Renumbered", 1_790_000_000));
    lock(&srv).append("INBOX", msg("u0@x", "Other", 1_790_000_000));
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    let thread = f.thread_of("u1@x").await;
    let projects = f.mailbox_id("Projects").await;
    f.engine.shutdown().await;
    created(
        f.engine
            .queue_op(op("rr-1", OpKind::Star, thread))
            .await
            .unwrap(),
    );
    let mut mv = op("rr-2", OpKind::Move, thread);
    mv.mailbox = Some(MailboxRowId(projects));
    created(f.engine.queue_op(mv).await.unwrap());

    {
        let mut s = lock(&srv);
        let readded = s.expunge("INBOX", 1);
        s.append("INBOX", readded);
        s.renumber("INBOX");
        assert_eq!(
            s.uid_of("INBOX", "u0@x"),
            Some(1),
            "the stale UID is another message"
        );
        s.mailbox("Projects").name = "Work/Projects".into();
    }
    let ctx = f.engine.ctx(AccountId(1));
    let ops = f
        .stores
        .config
        .read(|c| {
            Ok(crate::executor::load_pending(c)?
                .into_iter()
                .map(|s| s.op)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    let mut session = FakeConnector(srv.clone()).session();
    let batch = crate::executor::execute_batch(&mut session, &ctx, ops, None)
        .await
        .unwrap();
    assert_eq!(
        batch.outcomes,
        [
            ("rr-1".to_owned(), crate::executor::Outcome::Done),
            ("rr-2".to_owned(), crate::executor::Outcome::Done),
        ]
    );
    let mut s = lock(&srv);
    let moved = s
        .uid_of("Work/Projects", "u1@x")
        .expect("moved to the renamed folder");
    assert_eq!(s.flags_of("Work/Projects", moved), ["\\Flagged"]);
    assert!(s.uid_of("INBOX", "u1@x").is_none());
    assert_eq!(
        s.uid_of("INBOX", "u0@x"),
        Some(1),
        "the other message untouched"
    );
    assert!(s.flags_of("INBOX", 1).is_empty());
}

#[tokio::test]
async fn label_rename_job_resumes_from_persisted_progress() {
    let srv = server(CAPS);
    let a = lock(&srv).append("INBOX", msg("l1@x", "One", 1_790_000_000));
    let b = lock(&srv).append("Archive", msg("l2@x", "Two", 1_790_000_100));
    lock(&srv).set_flags("INBOX", a, &[Flag::Keyword("$label_Old".into())]);
    lock(&srv).set_flags("Archive", b, &[Flag::Keyword("$label_old".into())]);
    let f = fixture(&srv).await;
    f.engine.start_all().await.unwrap();
    eventually("initial sync", || f.all_live()).await;
    f.engine.shutdown().await;

    let change = f
        .engine
        .update_label(
            "old".into(),
            LabelPatchDto {
                name: Some("New Name".into()),
                color: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(change.label.unwrap().key, "new name");
    assert_eq!(change.job_ids.len(), 1);
    let id = change.job_ids[0];
    f.stores
        .config
        .write(move |tx| {
            tx.execute(
                "UPDATE label_job SET progress = '[\"INBOX\"]' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let jobs = f.engine.label_job_dtos().await.unwrap();
    assert_eq!((jobs[0].done, jobs[0].label.as_str()), (1, "old"));

    let f = f.restart(&srv, false).await;
    f.engine.start_all().await.unwrap();
    eventually("job finished", || async {
        f.engine.label_job_dtos().await.unwrap().is_empty()
    })
    .await;
    {
        let mut s = lock(&srv);
        assert_eq!(s.flags_of("Archive", b), ["$label_New=20Name"]);
        assert_eq!(
            s.flags_of("INBOX", a),
            ["$label_Old"],
            "INBOX was recorded as done before the restart"
        );
    }

    let change = f.engine.delete_label("new name".into()).await.unwrap();
    assert_eq!(change.job_ids.len(), 1);
    eventually("keyword deleted everywhere", || async {
        lock(&srv).flags_of("Archive", b).is_empty()
    })
    .await;
    f.engine.shutdown().await;
}
