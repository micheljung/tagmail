//! Calendar ops and invitations through the engine, against the in-memory
//! CalDAV server: RSVP in both scheduling modes, the 412 policies, op ids
//! and the sync-time apply.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tagmail_api_core::calendar_dto::{
    CalEditDto, CalEditKindDto, EventAttendeeDto, EventFieldsDto, RsvpPartstatDto,
};
use tagmail_api_core::compose::Compose;
use tagmail_api_core::mime::reduce::{CalendarParts, calendar_parts};
use tagmail_api_core::ops_dto::{CalConflictDto, CalReplyStateDto, OpDto, OpStatusDto};
use tagmail_api_core::{
    BoxFuture, CalAction, CalRequest, CalendarService, Submitted, WriteError, WriteService,
};
use tagmail_core::{AccountId, MessageRowId};
use tagmail_dav::{DavError, DiscoveredAccount, Discovery, Scheduling};
use tagmail_store::calendar;
use tagmail_store::rusqlite::params;
use tagmail_store::{ConfigDb, MasterKey, Vault};
use tempfile::TempDir;

use super::executor::{self, CalOp, CalOpKind};
use super::fake::{FakeConnector, HOME, ME, WORK, url};
use super::invite::{self, MailAccess};
use crate::fake::FakeAccounts;
use crate::supervisor::{Engine, EngineConfig, Stores};
use crate::testutil::{TestCache, open};

const ACCOUNT: AccountId = AccountId(1);
const IDENTITY: i64 = 7;
const BOSS: &str = "boss@example.com";
const TRUSTED: &str = "mx.example.com";
const UID: &str = "meet-1";
const POLL_STEP: Duration = Duration::from_millis(20);
const POLL_TRIES: usize = 250;

struct Fixture {
    engine: Engine<FakeAccounts>,
    connector: FakeConnector,
    stores: Stores,
    _cache: TestCache,
    _dir: TempDir,
}

async fn fixture(scheduling: Scheduling) -> Fixture {
    let dir = TempDir::new().unwrap();
    let config = Arc::new(ConfigDb::open(&dir.path().join("config.db")).unwrap());
    let (vault, _) = Vault::create(&MasterKey::generate()).unwrap();
    let sealed = vault.seal(ACCOUNT, "imap-secret").unwrap();
    config
        .write(move |tx| {
            tx.execute(
                "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                 smtp_port, smtp_tls, username, secret_ct, secret_nonce, email) \
                 VALUES (?1, 'a', 'imap.example.com', 993, 'tls', 'smtp', 465, 'tls', ?2, ?3, \
                 ?4, ?2)",
                params![ACCOUNT.0, ME, sealed.ciphertext, sealed.nonce],
            )?;
            tx.execute(
                "INSERT INTO identity (id, account_id, address, display_name, is_default) \
                 VALUES (?1, ?2, ?3, 'Me', 1)",
                params![IDENTITY, ACCOUNT.0, ME],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let cache = open();
    let stores = Stores {
        config,
        vault: Arc::new(vault),
        cache: cache.ctx.cache.clone(),
        blobs: cache.ctx.blobs.clone(),
        feed: cache.feed.clone(),
        uploads: dir.path().join("uploads"),
        outbox: dir.path().join("outbox"),
    };
    let engine = Engine::new(
        stores.clone(),
        FakeAccounts::default(),
        EngineConfig::default(),
    );
    let connector = FakeConnector::default();
    connector.dav.calendar(None, None);
    engine.start_calendar_on(Arc::new(connector.clone()));
    let f = Fixture {
        engine,
        connector,
        stores,
        _cache: cache,
        _dir: dir,
    };
    f.connector
        .discovers(Ok(Discovery::Found(Box::new(DiscoveredAccount {
            host: "dav.example.com".into(),
            context_url: url("/"),
            principal: url("/principals/me/"),
            home: url(HOME),
            scheduling,
            calendars: f.connector.dav.calendars(),
        }))));
    f.engine.discover(ACCOUNT).await.unwrap();
    f.synced().await;
    f.stores
        .config
        .write(|tx| {
            calendar::set_default_calendar(tx, ACCOUNT, Some(WORK))?;
            let mut c = calendar::load_caldav(tx, ACCOUNT)?.unwrap();
            c.authserv_id = Some(TRUSTED.into());
            calendar::save_caldav(tx, ACCOUNT, &c)
        })
        .await
        .unwrap();
    f
}

fn server_scheduling() -> Scheduling {
    Scheduling {
        auto_schedule: true,
        inbox: Some(url("/inbox/")),
        outbox: Some(url("/outbox/")),
    }
}

fn vevent(sequence: u32, dtstamp: &str, partstat: &str, method: Option<&str>) -> String {
    let method = method
        .map(|m| format!("METHOD:{m}\r\n"))
        .unwrap_or_default();
    let status = if method.contains("CANCEL") {
        "STATUS:CANCELLED\r\n"
    } else {
        ""
    };
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{method}BEGIN:VEVENT\r\n\
         UID:{UID}\r\nDTSTAMP:{dtstamp}\r\nSEQUENCE:{sequence}\r\n\
         DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:Review\r\n{status}\
         ORGANIZER;CN=Boss:mailto:{BOSS}\r\n\
         ATTENDEE;PARTSTAT={partstat};RSVP=TRUE:mailto:{ME}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn message(ics: &str, method: &str, results: Option<&str>) -> CalendarParts {
    let results = results
        .map(|r| format!("Authentication-Results: {r}\r\n"))
        .unwrap_or_default();
    let raw = format!(
        "{results}From: Boss <{BOSS}>\r\nTo: {ME}\r\nSubject: Invitation\r\n\
         Content-Type: text/calendar; method={method}; charset=utf-8\r\n\r\n{ics}"
    );
    calendar_parts(raw.as_bytes()).unwrap()
}

fn invitation() -> CalendarParts {
    message(
        &vevent(0, "20261101T120000Z", "NEEDS-ACTION", Some("REQUEST")),
        "REQUEST",
        None,
    )
}

fn rsvp(op_id: &str, partstat: RsvpPartstatDto) -> CalRequest {
    CalRequest {
        op_id: op_id.into(),
        account: ACCOUNT,
        message: MessageRowId(1),
        source: invitation(),
        action: CalAction::Rsvp {
            partstat,
            identity_id: None,
            allow_organizer_change: false,
            send_reply: true,
        },
    }
}

fn created(s: Submitted) -> OpDto {
    match s {
        Submitted::Created(dto) => dto,
        Submitted::Existing(dto) => panic!("already submitted: {dto:?}"),
    }
}

impl Fixture {
    /// Waits for a poll that saw the discovered account, so an earlier
    /// poll no longer drops its calendars.
    async fn synced(&self) {
        for _ in 0..POLL_TRIES {
            let synced = self
                .stores
                .cache
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM calendar WHERE synced_at IS NOT NULL",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?)
                })
                .await
                .unwrap();
            if synced > 0 {
                return;
            }
            tokio::time::sleep(POLL_STEP).await;
        }
        panic!("the calendar never synced");
    }

    async fn op(&self, op_id: &str) -> OpDto {
        WriteService::op(&self.engine, op_id.into()).await.unwrap()
    }

    async fn settled(&self, op_id: &str) -> OpDto {
        for _ in 0..POLL_TRIES {
            let op = self.op(op_id).await;
            if op.status != OpStatusDto::Pending {
                return op;
            }
            tokio::time::sleep(POLL_STEP).await;
        }
        panic!("{op_id} never settled: {:?}", self.op(op_id).await);
    }

    async fn outbox_ops(&self) -> Vec<String> {
        self.engine
            .outbox()
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.op_id)
            .collect()
    }

    async fn seen(&self) -> Vec<(String, String)> {
        self.stores
            .config
            .read(|c| {
                let mut stmt = c.prepare("SELECT op_id, kind FROM op_seen ORDER BY op_id")?;
                Ok(stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?)
            })
            .await
            .unwrap()
    }

    fn stored(&self) -> Option<String> {
        self.connector
            .dav
            .paths()
            .into_iter()
            .find_map(|p| self.connector.dav.resource(&p).map(|(_, d)| d))
    }
}

#[tokio::test]
async fn rsvp_client_mode_queues_one_reply_and_one_op_seen() {
    let f = fixture(Scheduling::default()).await;
    let op = created(
        f.engine
            .submit(rsvp("r-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    assert_eq!(
        op.calendar.as_ref().unwrap().reply_op_id.as_deref(),
        Some("r-1#reply")
    );
    let again = f
        .engine
        .submit(rsvp("r-1", RsvpPartstatDto::Accepted))
        .await
        .unwrap();
    assert!(matches!(again, Submitted::Existing(_)));
    assert_eq!(f.outbox_ops().await, ["r-1#reply"]);
    assert_eq!(f.seen().await, [("r-1".to_owned(), "cal".to_owned())]);

    let done = f.settled("r-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    let stored = f.stored().expect("the event is saved");
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    assert!(stored.contains("SCHEDULE-AGENT=CLIENT"), "{stored}");
    assert!(!stored.contains("METHOD:"), "{stored}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_revert_writes_the_prior_partstat_and_sends_nothing() {
    let f = fixture(Scheduling::default()).await;
    created(
        f.engine
            .submit(rsvp("r-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    assert_eq!(f.settled("r-1").await.status, OpStatusDto::Done);
    let prior: Option<String> = f
        .stores
        .config
        .read(|c| {
            Ok(c.query_row(
                "SELECT prior_partstat FROM cal_op WHERE op_id = 'r-1'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(prior.as_deref(), Some("NEEDS-ACTION"));

    let revert = |op_id: &str, send_reply| CalRequest {
        action: CalAction::Rsvp {
            partstat: RsvpPartstatDto::NeedsAction,
            identity_id: None,
            allow_organizer_change: false,
            send_reply,
        },
        ..rsvp(op_id, RsvpPartstatDto::NeedsAction)
    };
    let refused = f.engine.submit(revert("v-0", true)).await.unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { field, .. } if field == "partstat"),
        "{refused}"
    );
    let op = created(f.engine.submit(revert("v-1", false)).await.unwrap());
    assert_eq!(op.calendar.as_ref().unwrap().reply_op_id, None);
    assert_eq!(f.settled("v-1").await.status, OpStatusDto::Done);
    let stored = f.stored().expect("the event is saved");
    assert!(stored.contains("PARTSTAT=NEEDS-ACTION"), "{stored}");
    assert_eq!(f.outbox_ops().await, ["r-1#reply"]);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_mail_op_id_is_a_conflict_for_a_calendar_op() {
    let f = fixture(Scheduling::default()).await;
    f.stores
        .config
        .write(|tx| {
            tx.execute(
                "INSERT INTO op_seen (op_id, account_id) VALUES ('m-1', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let err = f
        .engine
        .submit(rsvp("m-1", RsvpPartstatDto::Accepted))
        .await
        .unwrap_err();
    assert!(matches!(err, WriteError::Conflict(_)), "{err}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_the_reply_discards_a_pending_rsvp_and_keeps_op_seen() {
    let f = fixture(Scheduling::default()).await;
    f.engine.shutdown().await;
    created(
        f.engine
            .submit(rsvp("r-2", RsvpPartstatDto::Declined))
            .await
            .unwrap(),
    );
    let reply = f.engine.outbox().await.unwrap().remove(0);
    f.engine.cancel(reply.id).await.unwrap();
    assert!(f.outbox_ops().await.is_empty());
    assert!(WriteService::op(&f.engine, "r-2".into()).await.is_err());
    assert_eq!(f.seen().await, [("r-2".to_owned(), "cal".to_owned())]);
}

#[tokio::test]
async fn rsvp_server_mode_queues_no_outbox_when_the_server_schedules() {
    let f = fixture(server_scheduling()).await;
    let mut stored = vevent(0, "20261101T120000Z", "NEEDS-ACTION", None);
    stored = stored.replace(
        &format!("ORGANIZER;CN=Boss:mailto:{BOSS}"),
        &format!("ORGANIZER;CN=Boss;SCHEDULE-STATUS=1.2:mailto:{BOSS}"),
    );
    f.connector.dav.put("meet.ics", "\"s1\"", &stored);
    let op = created(
        f.engine
            .submit(rsvp("s-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    assert_eq!(op.calendar.unwrap().reply_op_id, None);
    let done = f.settled("s-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert!(f.outbox_ops().await.is_empty());
    let stored = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(stored.contains("PARTSTAT=ACCEPTED"));
    assert!(!stored.contains("SCHEDULE-AGENT=CLIENT"));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_server_mode_revert_sends_nothing_and_a_later_answer_is_scheduled_again() {
    let f = fixture(server_scheduling()).await;
    let stored = vevent(0, "20261101T120000Z", "TENTATIVE", None).replace(
        &format!("ORGANIZER;CN=Boss:mailto:{BOSS}"),
        &format!("ORGANIZER;CN=Boss;SCHEDULE-STATUS=1.2:mailto:{BOSS}"),
    );
    f.connector.dav.put("meet.ics", "\"s1\"", &stored);
    let revert = CalRequest {
        action: CalAction::Rsvp {
            partstat: RsvpPartstatDto::NeedsAction,
            identity_id: None,
            allow_organizer_change: false,
            send_reply: false,
        },
        ..rsvp("v-1", RsvpPartstatDto::NeedsAction)
    };
    created(f.engine.submit(revert).await.unwrap());
    assert_eq!(f.settled("v-1").await.status, OpStatusDto::Done);
    assert!(f.outbox_ops().await.is_empty());
    let reverted = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(reverted.contains("PARTSTAT=NEEDS-ACTION"), "{reverted}");
    assert!(reverted.contains("SCHEDULE-AGENT=CLIENT"), "{reverted}");

    created(
        f.engine
            .submit(rsvp("s-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    assert_eq!(f.settled("s-1").await.status, OpStatusDto::Done);
    let answered = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(answered.contains("PARTSTAT=ACCEPTED"), "{answered}");
    assert!(!answered.contains("SCHEDULE-AGENT"), "{answered}");
    assert!(f.outbox_ops().await.is_empty());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn an_unreadable_copy_after_a_server_mode_decline_still_replies() {
    let f = fixture(server_scheduling()).await;
    let stored = vevent(0, "20261101T120000Z", "NEEDS-ACTION", None).replace(
        &format!("ORGANIZER;CN=Boss:mailto:{BOSS}"),
        &format!("ORGANIZER;CN=Boss;SCHEDULE-STATUS=1.2:mailto:{BOSS}"),
    );
    f.connector.dav.put("meet.ics", "\"s1\"", &stored);
    f.connector.dav.fail_next(
        "get",
        DavError::Status {
            status: 503,
            url: url("/home/work/meet.ics"),
        },
    );
    created(
        f.engine
            .submit(rsvp("u-1", RsvpPartstatDto::Declined))
            .await
            .unwrap(),
    );
    let done = f.settled("u-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(f.outbox_ops().await, ["u-1#reply"]);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn server_mode_without_schedule_status_queues_the_reply() {
    let f = fixture(server_scheduling()).await;
    f.connector.dav.put(
        "meet.ics",
        "\"s1\"",
        &vevent(0, "20261101T120000Z", "NEEDS-ACTION", None),
    );
    created(
        f.engine
            .submit(rsvp("s-2", RsvpPartstatDto::Tentative))
            .await
            .unwrap(),
    );
    let done = f.settled("s-2").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(f.outbox_ops().await, ["s-2#reply"]);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn server_mode_uid_miss_replies_like_client_mode() {
    let f = fixture(server_scheduling()).await;
    created(
        f.engine
            .submit(rsvp("s-3", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    let done = f.settled("s-3").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(f.outbox_ops().await, ["s-3#reply"]);
    assert!(f.stored().unwrap().contains("SCHEDULE-AGENT=CLIENT"));
    f.engine.shutdown().await;
}

#[tokio::test]
async fn rsvp_412_rebases_partstat() {
    let f = fixture(Scheduling::default()).await;
    f.connector.dav.put(
        "meet.ics",
        "\"s1\"",
        &vevent(0, "20261101T120000Z", "NEEDS-ACTION", None),
    );
    let moved = vevent(0, "20261101T120000Z", "NEEDS-ACTION", None).replace("Review", "Moved");
    f.connector
        .dav
        .rewrite_before_next_put("/home/work/meet.ics", &moved);
    created(
        f.engine
            .submit(rsvp("r-3", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    let done = f.settled("r-3").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    let stored = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(stored.contains("SUMMARY:Moved"), "{stored}");
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn answering_a_newer_invitation_saves_the_newer_version() {
    let f = fixture(Scheduling::default()).await;
    f.connector.dav.put(
        "meet.ics",
        "\"s1\"",
        &vevent(0, "20261101T120000Z", "NEEDS-ACTION", None),
    );
    let newer =
        vevent(1, "20261102T120000Z", "NEEDS-ACTION", Some("REQUEST")).replace("Review", "Moved");
    let request = CalRequest {
        source: message(&newer, "REQUEST", None),
        ..rsvp("r-5", RsvpPartstatDto::Accepted)
    };
    created(f.engine.submit(request).await.unwrap());
    let done = f.settled("r-5").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    let stored = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(
        stored.contains("SUMMARY:Moved") && stored.contains("SEQUENCE:1"),
        "{stored}"
    );
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn rsvp_fails_after_three_412s() {
    let f = fixture(Scheduling::default()).await;
    let original = vevent(0, "20261101T120000Z", "NEEDS-ACTION", None);
    f.connector.dav.put("meet.ics", "\"s1\"", &original);
    for n in 0..executor::MAX_REBASES {
        f.connector.dav.rewrite_before_next_put(
            "/home/work/meet.ics",
            &original.replace("Review", &format!("Moved {n}")),
        );
    }
    created(
        f.engine
            .submit(rsvp("r-4", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    let failed = f.settled("r-4").await;
    assert_eq!(failed.status, OpStatusDto::Failed);
    assert!(
        !f.connector
            .dav
            .resource("/home/work/meet.ics")
            .unwrap()
            .1
            .contains("PARTSTAT=ACCEPTED")
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn ops_of_an_account_being_removed_back_off() {
    let f = fixture(Scheduling::default()).await;
    f.engine.inner.calendar.removing(ACCOUNT);
    created(
        f.engine
            .submit(rsvp("b-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    let mut deferred = None;
    for _ in 0..POLL_TRIES {
        let ops = f
            .stores
            .config
            .read(|c| Ok(executor::load(c, "op_id = 'b-1'", [])?))
            .await
            .unwrap();
        if let Some(op) = ops.into_iter().find(|o| o.next_at.is_some()) {
            deferred = Some(op);
            break;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
    let op = deferred.expect("the op is put off");
    assert_eq!(op.state, tagmail_core::ops::OpState::Pending);
    assert!(op.next_at.unwrap() > crate::executor::now_secs());
    assert_eq!(op.attempts, 1, "deferred once, not spinning");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn put_412_server_wins_and_keeps_rejected_ics() {
    let f = fixture(Scheduling::default()).await;
    let server_copy = vevent(3, "20261103T120000Z", "ACCEPTED", None);
    f.connector.dav.put("meet.ics", "\"new\"", &server_copy);
    f.engine.shutdown().await;
    let edit = vevent(2, "20261102T120000Z", "ACCEPTED", None).replace("Review", "Edited");
    let dir = executor::ics_dir(&f.stores);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("p-1.ics"), &edit).unwrap();
    let mut op = CalOp::new("p-1".into(), ACCOUNT, CalOpKind::Put, UID.into());
    op.calendar_href = Some(WORK.into());
    op.href = Some("/home/work/meet.ics".into());
    op.if_match = Some("\"old\"".into());
    op.ics_file = Some("p-1.ics".into());
    f.stores
        .config
        .write(move |tx| Ok(executor::insert(tx, &op)?))
        .await
        .unwrap();
    f.engine.start_calendar_on(Arc::new(f.connector.clone()));

    let failed = f.settled("p-1").await;
    assert_eq!(failed.status, OpStatusDto::Failed);
    assert_eq!(
        failed.calendar.as_ref().unwrap().conflict,
        Some(CalConflictDto::ServerWon)
    );
    assert_eq!(
        f.connector.dav.resource("/home/work/meet.ics").unwrap().1,
        server_copy
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("p-1.conflict.ics")).unwrap(),
        edit
    );

    WriteService::retry_op(&f.engine, "p-1".into())
        .await
        .unwrap();
    let done = f.settled("p-1").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert!(
        f.connector
            .dav
            .resource("/home/work/meet.ics")
            .unwrap()
            .1
            .contains("Edited")
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn no_calendar_still_sends_reply() {
    let f = fixture(Scheduling::default()).await;
    f.stores
        .config
        .write(|tx| calendar::set_default_calendar(tx, ACCOUNT, None))
        .await
        .unwrap();
    let op = created(
        f.engine
            .submit(rsvp("n-1", RsvpPartstatDto::Accepted))
            .await
            .unwrap(),
    );
    assert_eq!(op.status, OpStatusDto::Done);
    assert_eq!(op.calendar.unwrap().reply, Some(CalReplyStateDto::Queued));
    assert_eq!(f.outbox_ops().await, ["n-1#reply"]);
    f.engine.shutdown().await;
}

/// Mail as the runner sees it, from a script.
#[derive(Default)]
struct FakeMail(Mutex<Option<CalendarParts>>);

impl MailAccess for FakeMail {
    fn invitation(
        &self,
        _account: AccountId,
        _email_id: String,
    ) -> BoxFuture<crate::Result<Option<CalendarParts>>> {
        let parts = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(parts) })
    }

    fn authserv_samples(
        &self,
        _account: AccountId,
    ) -> BoxFuture<crate::Result<Vec<(String, String)>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn outbox_queued(&self) {}
}

async fn queue_invite(f: &Fixture) {
    f.stores
        .cache
        .write(|tx| {
            tx.execute(
                "INSERT INTO cal_inbox (account_id, email_id, internal_date) VALUES (1, 'e1', ?1)",
                [crate::executor::now_secs()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn applies(f: &Fixture) -> Vec<String> {
    f.stores
        .config
        .read(|c| {
            Ok(executor::load(c, "kind = 'apply'", [])?
                .into_iter()
                .map(|o| o.op_id)
                .collect())
        })
        .await
        .unwrap()
}

async fn synced_with_answered_event(f: &Fixture) {
    f.connector.dav.put(
        "meet.ics",
        "\"s1\"",
        &vevent(0, "20261101T120000Z", "ACCEPTED", None),
    );
    f.engine.settings_changed(ACCOUNT);
    for _ in 0..POLL_TRIES {
        let synced = f
            .stores
            .cache
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM cal_event e JOIN calendar c ON c.id = e.calendar_id \
                     WHERE c.synced_at IS NOT NULL",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap();
        if synced == 1 {
            return;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
    panic!("the calendar never synced");
}

const PASS: &str = "mx.example.com; dmarc=pass header.from=example.com";

#[tokio::test]
async fn higher_sequence_auto_applies_when_responded_and_verified() {
    let f = fixture(Scheduling::default()).await;
    synced_with_answered_event(&f).await;
    f.engine.shutdown().await;
    let update =
        vevent(1, "20261102T120000Z", "NEEDS-ACTION", Some("REQUEST")).replace("Review", "Moved");
    let mail = FakeMail(Mutex::new(Some(message(&update, "REQUEST", Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    let ids = applies(&f).await;
    assert_eq!(ids.len(), 1);

    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert_eq!(
        applies(&f).await,
        ids,
        "a resync does not requeue the apply"
    );

    f.engine.start_calendar_on(Arc::new(f.connector.clone()));
    let done = f.settled(&ids[0]).await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    let stored = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(stored.contains("Moved"), "{stored}");
    assert!(stored.contains("PARTSTAT=ACCEPTED"), "{stored}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn organizer_takeover_is_not_auto_applied() {
    let f = fixture(Scheduling::default()).await;
    synced_with_answered_event(&f).await;
    f.engine.shutdown().await;
    let takeover = vevent(1, "20261102T120000Z", "NEEDS-ACTION", Some("REQUEST"))
        .replace(&format!("mailto:{BOSS}"), "mailto:thief@example.com");
    let results = "mx.example.com; dmarc=pass header.from=example.com";
    let raw = format!(
        "Authentication-Results: {results}\r\nFrom: <thief@example.com>\r\nTo: {ME}\r\n\
         Subject: Invitation\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\n{takeover}"
    );
    let mail = FakeMail(Mutex::new(Some(calendar_parts(raw.as_bytes()).unwrap())));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert!(applies(&f).await.is_empty());

    let apply = |allow| CalRequest {
        op_id: format!("t-{allow}"),
        account: ACCOUNT,
        message: MessageRowId(1),
        source: calendar_parts(raw.as_bytes()).unwrap(),
        action: CalAction::Apply {
            allow_organizer_change: allow,
        },
    };
    let answer = |allow| CalRequest {
        op_id: format!("r-{allow}"),
        action: CalAction::Rsvp {
            partstat: RsvpPartstatDto::Accepted,
            identity_id: None,
            allow_organizer_change: allow,
            send_reply: true,
        },
        ..apply(allow)
    };
    for refused in [apply(false), answer(false)] {
        let refused = f.engine.submit(refused).await.unwrap_err();
        assert!(
            matches!(&refused, WriteError::Invalid { field, .. } if field == "allowOrganizerChange"),
            "{refused}"
        );
    }
    created(f.engine.submit(apply(true)).await.unwrap());
    created(f.engine.submit(answer(true)).await.unwrap());
}

#[tokio::test]
async fn unauthenticated_or_untrusted_updates_are_manual() {
    let f = fixture(Scheduling::default()).await;
    synced_with_answered_event(&f).await;
    f.engine.shutdown().await;
    let update = vevent(1, "20261102T120000Z", "NEEDS-ACTION", Some("REQUEST"));
    for results in [
        None,
        Some("evil.example; dmarc=pass header.from=example.com"),
        Some("mx.example.com; dkim=pass header.d=other.example"),
    ] {
        let mail = FakeMail(Mutex::new(Some(message(&update, "REQUEST", results))));
        queue_invite(&f).await;
        invite::process_inbox(&f.stores, &mail).await;
    }
    let spoofed = update.replace(&format!("mailto:{BOSS}"), "mailto:other@example.com");
    let mail = FakeMail(Mutex::new(Some(message(&spoofed, "REQUEST", Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert!(applies(&f).await.is_empty());
}

fn fields(summary: &str, start: &str, attendees: &[&str]) -> EventFieldsDto {
    EventFieldsDto {
        summary: Some(summary.into()),
        location: None,
        description: None,
        start: format!("2099-12-01T{start}"),
        end: "2099-12-01T18:00:00".into(),
        all_day: false,
        tzid: Some("Europe/Zurich".into()),
        attendees: attendees
            .iter()
            .map(|a| EventAttendeeDto {
                email: (*a).into(),
                name: None,
                partstat: None,
            })
            .collect(),
        alarms: vec![15],
    }
}

fn edit(op_id: &str, kind: CalEditKindDto, href: Option<&str>) -> CalEditDto {
    CalEditDto {
        op_id: op_id.into(),
        kind,
        account_id: ACCOUNT.0,
        calendar_href: WORK.into(),
        href: href.map(str::to_owned),
        etag: None,
        event: None,
        dest_calendar_href: None,
        send_decline: None,
    }
}

impl Fixture {
    async fn edited(&self, request: CalEditDto) -> OpDto {
        let op_id = request.op_id.clone();
        created(self.engine.edit(request).await.unwrap());
        let done = self.settled(&op_id).await;
        assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
        done
    }

    async fn href_of(&self, op_id: &str) -> String {
        let id = op_id.to_owned();
        self.stores
            .config
            .read(move |c| Ok(executor::load_one(c, &id)?))
            .await
            .unwrap()
            .and_then(|op| op.href)
            .unwrap()
    }

    async fn put(&self, op_id: &str, href: Option<&str>, event: EventFieldsDto) -> String {
        self.edited(CalEditDto {
            event: Some(event),
            ..edit(op_id, CalEditKindDto::Put, href)
        })
        .await;
        self.href_of(op_id).await
    }
}

#[tokio::test]
async fn events_are_created_edited_moved_and_deleted() {
    let f = fixture(Scheduling::default()).await;
    f.connector.dav.add_calendar(super::fake::OTHER, None, None);
    f.engine.settings_changed(ACCOUNT);
    let href = f.put("n-1", None, fields("Lunch", "12:00:00", &[])).await;
    let (_, stored) = f.connector.dav.resource(&href).unwrap();
    assert!(stored.contains("SUMMARY:Lunch") && stored.contains("TRIGGER:-PT15M"));
    assert!(stored.contains("DTSTART;TZID=Europe/Zurich:20991201T120000"));
    assert!(stored.contains("BEGIN:VTIMEZONE"));
    assert!(f.outbox_ops().await.is_empty(), "no attendees, no mail");

    f.put("n-2", Some(&href), fields("Long lunch", "12:00:00", &[]))
        .await;
    let (_, stored) = f.connector.dav.resource(&href).unwrap();
    assert!(stored.contains("SUMMARY:Long lunch") && stored.contains("SEQUENCE:1"));

    for _ in 0..POLL_TRIES {
        if f.engine
            .edit(CalEditDto {
                dest_calendar_href: Some(super::fake::OTHER.into()),
                ..edit("n-3", CalEditKindDto::Move, Some(&href))
            })
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
    assert_eq!(f.settled("n-3").await.status, OpStatusDto::Done);
    let moved = f.href_of("n-3").await;
    assert!(moved.starts_with(super::fake::OTHER), "{moved}");
    assert!(f.connector.dav.resource(&href).is_none());

    let delete = CalEditDto {
        calendar_href: super::fake::OTHER.into(),
        ..edit("n-4", CalEditKindDto::Delete, Some(&moved))
    };
    f.edited(delete).await;
    assert!(f.connector.dav.resource(&moved).is_none());
    assert!(f.outbox_ops().await.is_empty());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_changed_etag_is_a_conflict() {
    let f = fixture(Scheduling::default()).await;
    let href = f.put("c-1", None, fields("Lunch", "12:00:00", &[])).await;
    let stale = CalEditDto {
        etag: Some("\"stale\"".into()),
        event: Some(fields("Mine", "12:00:00", &[])),
        ..edit("c-2", CalEditKindDto::Put, Some(&href))
    };
    let refused = f.engine.edit(stale).await.unwrap_err();
    assert!(matches!(refused, WriteError::Conflict(_)), "{refused}");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn invitations_in_client_mode_send_request_and_cancel() {
    let f = fixture(Scheduling::default()).await;
    let guests = ["ann@example.com", "bo@example.com"];
    let href = f
        .put("i-1", None, fields("Plan", "12:00:00", &guests))
        .await;
    let (_, stored) = f.connector.dav.resource(&href).unwrap();
    assert!(
        stored.contains(&format!("ORGANIZER;CN=Me:mailto:{ME}")),
        "{stored}"
    );
    assert!(stored.contains("SCHEDULE-AGENT=CLIENT"), "{stored}");
    assert_eq!(f.outbox_ops().await, ["i-1#request"]);

    f.put("i-2", Some(&href), fields("Plan", "13:00:00", &guests[..1]))
        .await;
    let (_, stored) = f.connector.dav.resource(&href).unwrap();
    assert!(stored.contains("SEQUENCE:1") && !stored.contains("bo@example.com"));
    assert_eq!(
        f.outbox_ops().await,
        ["i-1#request", "i-2#request", "i-2#cancel"]
    );

    let mut only_alarm = fields("Plan", "13:00:00", &guests[..1]);
    only_alarm.alarms = vec![30];
    f.put("i-3", Some(&href), only_alarm).await;
    assert_eq!(f.outbox_ops().await.len(), 3, "a reminder is mine alone");

    f.edited(edit("i-4", CalEditKindDto::Delete, Some(&href)))
        .await;
    assert_eq!(f.outbox_ops().await.last().unwrap(), "i-4#cancel");
    f.engine.shutdown().await;
}

#[tokio::test]
async fn invitations_in_server_mode_queue_no_outbox() {
    let f = fixture(server_scheduling()).await;
    let href = f
        .put(
            "s-1",
            None,
            fields("Plan", "12:00:00", &["ann@example.com"]),
        )
        .await;
    let (_, stored) = f.connector.dav.resource(&href).unwrap();
    assert!(!stored.contains("SCHEDULE-AGENT"), "{stored}");
    f.edited(edit("s-2", CalEditKindDto::Delete, Some(&href)))
        .await;
    assert!(f.outbox_ops().await.is_empty());
    f.engine.shutdown().await;
}

fn organized(partstat: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\n\
         UID:{UID}\r\nDTSTAMP:20261101T120000Z\r\nSEQUENCE:0\r\n\
         DTSTART:20991201T090000Z\r\nDTEND:20991201T100000Z\r\nSUMMARY:Review\r\n\
         ORGANIZER;CN=Me:mailto:{ME}\r\n\
         ATTENDEE;PARTSTAT={partstat};RSVP=TRUE:mailto:{BOSS}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn reply_mail(from: &str, results: Option<&str>) -> CalendarParts {
    let ics = organized("ACCEPTED")
        .replace("VERSION:2.0\r\n", "VERSION:2.0\r\nMETHOD:REPLY\r\n")
        .replace("DTSTAMP:20261101T120000Z", "DTSTAMP:20261102T120000Z");
    let results = results
        .map(|r| format!("Authentication-Results: {r}\r\n"))
        .unwrap_or_default();
    let raw = format!(
        "{results}From: <{from}>\r\nTo: {ME}\r\nSubject: Accepted\r\n\
         Content-Type: text/calendar; method=REPLY\r\n\r\n{ics}"
    );
    calendar_parts(raw.as_bytes()).unwrap()
}

async fn organized_and_synced(f: &Fixture, partstat: &str) {
    f.connector
        .dav
        .put("meet.ics", "\"o1\"", &organized(partstat));
    f.engine.settings_changed(ACCOUNT);
    for _ in 0..POLL_TRIES {
        let n = f
            .stores
            .cache
            .read(
                |c| Ok(c.query_row("SELECT count(*) FROM cal_event", [], |r| r.get::<_, i64>(0))?),
            )
            .await
            .unwrap();
        if n == 1 {
            return;
        }
        tokio::time::sleep(POLL_STEP).await;
    }
    panic!("the calendar never synced");
}

#[tokio::test]
async fn an_incoming_reply_sets_the_attendees_partstat_once() {
    let f = fixture(Scheduling::default()).await;
    organized_and_synced(&f, "NEEDS-ACTION").await;
    f.engine.shutdown().await;
    for spoofed in [
        reply_mail("eve@example.com", Some(PASS)),
        reply_mail(BOSS, None),
        reply_mail(
            BOSS,
            Some("evil.example; dmarc=pass header.from=example.com"),
        ),
    ] {
        let mail = FakeMail(Mutex::new(Some(spoofed)));
        queue_invite(&f).await;
        invite::process_inbox(&f.stores, &mail).await;
    }
    assert!(applies(&f).await.is_empty());

    let mail = FakeMail(Mutex::new(Some(reply_mail(BOSS, Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    let ids = applies(&f).await;
    assert_eq!(ids.len(), 1);
    f.engine.start_calendar_on(Arc::new(f.connector.clone()));
    assert_eq!(f.settled(&ids[0]).await.status, OpStatusDto::Done);
    let stored = f.connector.dav.resource("/home/work/meet.ics").unwrap().1;
    assert!(
        stored.contains(&format!("PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:{BOSS}")),
        "{stored}"
    );
    assert!(f.outbox_ops().await.is_empty());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_reply_already_stored_queues_nothing() {
    let f = fixture(Scheduling::default()).await;
    organized_and_synced(&f, "ACCEPTED").await;
    f.engine.shutdown().await;
    let mail = FakeMail(Mutex::new(Some(reply_mail(BOSS, Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert!(applies(&f).await.is_empty());
}

/// The editor's fields of the cached event at `href`.
async fn cached_fields(f: &Fixture, href: &str) -> (EventFieldsDto, Option<String>) {
    let href = href.to_owned();
    let (ics, etag) = f
        .stores
        .cache
        .read(move |c| {
            let (id, etag): (i64, Option<String>) = c.query_row(
                "SELECT id, etag FROM cal_event WHERE href = ?1",
                [href],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((calendar::event_ics(c, id)?.unwrap(), etag))
        })
        .await
        .unwrap();
    let parsed = tagmail_calendar::parse(&ics).unwrap();
    (
        tagmail_api_core::event_fields::fields_dto(&tagmail_calendar::fields(&parsed).unwrap()),
        etag,
    )
}

const MEET: &str = "/home/work/meet.ics";

async fn held_outbox(f: &Fixture) -> Vec<(String, bool)> {
    f.stores
        .config
        .read(|c| {
            let mut stmt = c.prepare("SELECT op_id, next_at FROM outbox ORDER BY id")?;
            Ok(stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, Option<i64>>(1)? == Some(executor::HELD),
                    ))
                })?
                .collect::<tagmail_store::rusqlite::Result<_>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn an_invited_event_edit_keeps_its_sequence_so_updates_still_apply() {
    let f = fixture(Scheduling::default()).await;
    synced_with_answered_event(&f).await;
    let (mut fields, _) = cached_fields(&f, MEET).await;
    fields.summary = Some("My name for it".into());
    fields.alarms = vec![5];
    f.put("v-1", Some(MEET), fields.clone()).await;
    let stored = f.connector.dav.resource(MEET).unwrap().1;
    assert!(
        stored.contains("SUMMARY:My name for it") && stored.contains("SEQUENCE:0"),
        "{stored}"
    );
    assert!(stored.contains("DTSTAMP:20261101T120000Z"), "{stored}");
    assert!(
        f.outbox_ops().await.is_empty(),
        "the organizer is told nothing"
    );

    fields.attendees.push(EventAttendeeDto {
        email: "friend@example.com".into(),
        name: None,
        partstat: None,
    });
    let refused = f
        .engine
        .edit(CalEditDto {
            event: Some(fields),
            ..edit("v-2", CalEditKindDto::Put, Some(MEET))
        })
        .await
        .unwrap_err();
    assert!(matches!(&refused, WriteError::Invalid { field, .. } if field == "attendees"));

    f.engine.shutdown().await;
    let update = vevent(1, "20261102T120000Z", "NEEDS-ACTION", Some("REQUEST"));
    let mail = FakeMail(Mutex::new(Some(message(&update, "REQUEST", Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert_eq!(
        applies(&f).await.len(),
        1,
        "the organizer's next update is not stale"
    );
}

#[tokio::test]
async fn deleting_an_invited_event_can_send_a_decline() {
    for (scheduling, decline, replies) in [
        (Scheduling::default(), true, 1),
        (Scheduling::default(), false, 0),
        (server_scheduling(), true, 0),
    ] {
        let f = fixture(scheduling).await;
        synced_with_answered_event(&f).await;
        f.connector.dav.took();
        f.edited(CalEditDto {
            send_decline: Some(decline),
            ..edit("x-1", CalEditKindDto::Delete, Some(MEET))
        })
        .await;
        assert!(f.connector.dav.resource(MEET).is_none());
        let deletes: Vec<&str> = f
            .connector
            .dav
            .took()
            .into_iter()
            .filter(|c| c.starts_with("delete"))
            .collect();
        let asked = if decline {
            "delete"
        } else {
            "delete without reply"
        };
        assert_eq!(deletes, [asked], "Schedule-Reply: F unless declining");
        let outbox = f.outbox_ops().await;
        assert_eq!(outbox.len(), replies, "{outbox:?}");
        if replies > 0 {
            assert_eq!(outbox, ["x-1#reply"]);
            let raw = f
                .stores
                .config
                .read(|c| {
                    Ok(c.query_row("SELECT mime_file FROM outbox", [], |r| {
                        r.get::<_, String>(0)
                    })?)
                })
                .await
                .unwrap();
            let mail = std::fs::read_to_string(f.stores.outbox.join(raw)).unwrap();
            assert!(
                mail.contains("METHOD:REPLY") && mail.contains("PARTSTAT=DECLINED"),
                "{mail}"
            );
        }
        f.engine.shutdown().await;
    }
}

#[tokio::test]
async fn invitations_wait_for_the_edit_and_go_once_it_is_written() {
    let f = fixture(Scheduling::default()).await;
    f.connector
        .dav
        .fail_next("put", DavError::PreconditionFailed);
    created(
        f.engine
            .edit(CalEditDto {
                event: Some(fields("Plan", "12:00:00", &["ann@example.com"])),
                ..edit("h-1", CalEditKindDto::Put, None)
            })
            .await
            .unwrap(),
    );
    let failed = f.settled("h-1").await;
    assert_eq!(
        failed.calendar.as_ref().unwrap().conflict,
        Some(CalConflictDto::ServerWon)
    );
    assert_eq!(
        failed.calendar.as_ref().unwrap().reply_op_id.as_deref(),
        Some("h-1#request")
    );
    assert_eq!(held_outbox(&f).await, [("h-1#request".to_owned(), true)]);

    WriteService::retry_op(&f.engine, "h-1".into())
        .await
        .unwrap();
    assert_eq!(f.settled("h-1").await.status, OpStatusDto::Done);
    assert_eq!(held_outbox(&f).await, [("h-1#request".to_owned(), false)]);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn discarding_a_held_edit_cancels_its_invitations() {
    let f = fixture(Scheduling::default()).await;
    f.connector
        .dav
        .fail_next("put", DavError::PreconditionFailed);
    created(
        f.engine
            .edit(CalEditDto {
                event: Some(fields("Plan", "12:00:00", &["ann@example.com"])),
                ..edit("h-2", CalEditKindDto::Put, None)
            })
            .await
            .unwrap(),
    );
    assert_eq!(f.settled("h-2").await.status, OpStatusDto::Failed);
    WriteService::discard_op(&f.engine, "h-2".into())
        .await
        .unwrap();
    assert!(held_outbox(&f).await.is_empty());
    f.engine.shutdown().await;
}

#[tokio::test]
async fn white_space_and_reminders_change_nothing_attendees_see() {
    let f = fixture(Scheduling::default()).await;
    let href = f
        .put(
            "w-1",
            None,
            fields("Plan", "12:00:00", &["ann@example.com"]),
        )
        .await;
    let stored = f.connector.dav.resource(&href).unwrap().1;
    let padded = stored.replace(
        "SUMMARY:Plan\r\n",
        "SUMMARY:Plan\r\nDESCRIPTION:Agenda\\n\r\n",
    );
    f.connector
        .dav
        .put(href.rsplit('/').next().unwrap(), "\"w2\"", &padded);
    f.engine.settings_changed(ACCOUNT);
    let mut fields = loop {
        let (fields, etag) = cached_fields(&f, &href).await;
        if etag.as_deref() == Some("\"w2\"") {
            break fields;
        }
        tokio::time::sleep(POLL_STEP).await;
    };
    assert_eq!(fields.description.as_deref(), Some("Agenda\n"));
    fields.description = Some("Agenda".into());
    fields.alarms = vec![30];
    f.put("w-2", Some(&href), fields).await;
    let stored = f.connector.dav.resource(&href).unwrap().1;
    assert!(
        stored.contains("SEQUENCE:0") && stored.contains("DESCRIPTION:Agenda\\n"),
        "{stored}"
    );
    assert_eq!(f.outbox_ops().await, ["w-1#request"]);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn guests_without_mail_addresses_are_kept() {
    let f = fixture(Scheduling::default()).await;
    let room = super::fake::event("room-1", 0, None).replace(
        "END:VEVENT",
        "ATTENDEE;CUTYPE=ROOM:urn:uuid:room-4\r\nEND:VEVENT",
    );
    f.connector.dav.put("room.ics", "\"r1\"", &room);
    f.engine.settings_changed(ACCOUNT);
    let href = "/home/work/room.ics";
    let mut fields = loop {
        if let Ok(found) = f
            .stores
            .cache
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM cal_event WHERE href = '/home/work/room.ics'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            && found == 1
        {
            break cached_fields(&f, href).await.0;
        }
        tokio::time::sleep(POLL_STEP).await;
    };
    assert!(fields.attendees.iter().all(|a| a.email.contains('@')));
    fields.summary = Some("Room booked".into());
    f.put("r-1", Some(href), fields).await;
    let stored = f.connector.dav.resource(href).unwrap().1;
    assert!(
        stored.contains("ATTENDEE;CUTYPE=ROOM:urn:uuid:room-4"),
        "{stored}"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_repeating_event_with_exceptions_keeps_its_times() {
    let f = fixture(Scheduling::default()).await;
    let series = super::fake::event("series-1", 0, Some("FREQ=WEEKLY"))
        .replace("END:VEVENT", "EXDATE:20261208T090000Z\r\nEND:VEVENT");
    f.connector.dav.put("series.ics", "\"s1\"", &series);
    f.engine.settings_changed(ACCOUNT);
    let href = "/home/work/series.ics";
    let mut fields = loop {
        let n = f
            .stores
            .cache
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM cal_event WHERE href = '/home/work/series.ics'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap();
        if n == 1 {
            break cached_fields(&f, href).await.0;
        }
        tokio::time::sleep(POLL_STEP).await;
    };
    fields.start = fields.start.replace("T09:", "T10:");
    fields.end = fields.end.replace("T10:", "T11:");
    let refused = f
        .engine
        .edit(CalEditDto {
            event: Some(fields),
            ..edit("s-9", CalEditKindDto::Put, Some(href))
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { field, .. } if field == "start"),
        "{refused}"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn inviting_needs_an_identity_to_send_from() {
    let f = fixture(Scheduling::default()).await;
    f.stores
        .config
        .write(|tx| Ok(tx.execute("DELETE FROM identity", [])?))
        .await
        .unwrap();
    let refused = f
        .engine
        .edit(CalEditDto {
            event: Some(fields("Plan", "12:00:00", &["ann@example.com"])),
            ..edit("n-9", CalEditKindDto::Put, None)
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { message, .. } if message.contains("identity"))
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn an_older_reply_after_a_newer_one_is_not_applied() {
    let f = fixture(Scheduling::default()).await;
    organized_and_synced(&f, "NEEDS-ACTION").await;
    f.engine.shutdown().await;
    let mail = FakeMail(Mutex::new(Some(reply_mail(BOSS, Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert_eq!(applies(&f).await.len(), 1);
    let older = organized("DECLINED")
        .replace("VERSION:2.0\r\n", "VERSION:2.0\r\nMETHOD:REPLY\r\n")
        .replace("DTSTAMP:20261101T120000Z", "DTSTAMP:20261101T130000Z");
    let raw = format!(
        "Authentication-Results: {PASS}\r\nFrom: <{BOSS}>\r\nTo: {ME}\r\nSubject: Declined\r\n\
         Content-Type: text/calendar; method=REPLY\r\n\r\n{older}"
    );
    let mail = FakeMail(Mutex::new(Some(calendar_parts(raw.as_bytes()).unwrap())));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert_eq!(applies(&f).await.len(), 1);
}

#[tokio::test]
async fn edits_of_one_event_run_in_order_through_retries() {
    let f = fixture(Scheduling::default()).await;
    f.engine.shutdown().await;
    let mut put = CalOp::new("o-1".into(), ACCOUNT, CalOpKind::Put, "u-1".into());
    put.next_at = Some(i64::MAX);
    let mut failed = CalOp::new("o-4".into(), ACCOUNT, CalOpKind::Put, "u-3".into());
    failed.state = tagmail_core::ops::OpState::Failed;
    let ops = [
        put,
        CalOp::new("o-2".into(), ACCOUNT, CalOpKind::Move, "u-1".into()),
        CalOp::new("o-3".into(), ACCOUNT, CalOpKind::Move, "u-2".into()),
        failed,
        CalOp::new("o-5".into(), ACCOUNT, CalOpKind::Move, "u-3".into()),
    ];
    f.stores
        .config
        .write(move |tx| {
            for op in &ops {
                executor::insert(tx, op)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let due: Vec<String> = executor::due(&f.stores)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.op_id)
        .collect();
    assert_eq!(
        due,
        ["o-3", "o-5"],
        "a move waits for a pending put before it, not for a failed one"
    );
}

#[tokio::test]
async fn applying_a_reply_by_hand_needs_my_event() {
    let f = fixture(Scheduling::default()).await;
    synced_with_answered_event(&f).await;
    let reply = vevent(0, "20261102T120000Z", "ACCEPTED", Some("REPLY"));
    let refused = f
        .engine
        .submit(CalRequest {
            op_id: "m-9".into(),
            account: ACCOUNT,
            message: MessageRowId(1),
            source: message(&reply, "REPLY", Some(PASS)),
            action: CalAction::Apply {
                allow_organizer_change: false,
            },
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { message, .. } if message.contains("do not organize")),
        "{refused}"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_rerun_after_its_write_landed_is_done() {
    let f = fixture(Scheduling::default()).await;
    f.engine.shutdown().await;
    let written = vevent(1, "20261102T120000Z", "ACCEPTED", None).replace("Review", "Edited");
    f.connector.dav.put("meet.ics", "\"after\"", &written);
    let dir = executor::ics_dir(&f.stores);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c-9.ics"), &written).unwrap();
    let mut op = CalOp::new("c-9".into(), ACCOUNT, CalOpKind::Put, UID.into());
    op.calendar_href = Some(WORK.into());
    op.href = Some(MEET.into());
    op.if_match = Some("\"before\"".into());
    op.ics_file = Some("c-9.ics".into());
    f.stores
        .config
        .write(move |tx| Ok(executor::insert(tx, &op)?))
        .await
        .unwrap();
    f.engine.start_calendar_on(Arc::new(f.connector.clone()));
    let done = f.settled("c-9").await;
    assert_eq!(done.status, OpStatusDto::Done, "{:?}", done.error);
    assert_eq!(done.calendar.unwrap().conflict, None);
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_cancelled_invitation_of_an_edit_reads_cancelled() {
    let f = fixture(Scheduling::default()).await;
    f.connector
        .dav
        .fail_next("put", DavError::PreconditionFailed);
    created(
        f.engine
            .edit(CalEditDto {
                event: Some(fields("Plan", "12:00:00", &["ann@example.com"])),
                ..edit("k-1", CalEditKindDto::Put, None)
            })
            .await
            .unwrap(),
    );
    let failed = f.settled("k-1").await;
    assert_eq!(
        failed.calendar.unwrap().reply,
        Some(CalReplyStateDto::Queued)
    );
    let id = f
        .engine
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.op_id == "k-1#request")
        .unwrap()
        .id;
    f.engine.cancel_impl(id).await.unwrap();
    assert_eq!(
        f.op("k-1").await.calendar.unwrap().reply,
        Some(CalReplyStateDto::Cancelled)
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn updating_guests_needs_the_organizers_identity() {
    let f = fixture(Scheduling::default()).await;
    let href = f
        .put(
            "g-1",
            None,
            fields("Plan", "12:00:00", &["ann@example.com"]),
        )
        .await;
    f.stores
        .config
        .write(|tx| Ok(tx.execute("DELETE FROM identity", [])?))
        .await
        .unwrap();
    let refused = f
        .engine
        .edit(CalEditDto {
            event: Some(fields("Plan", "13:00:00", &["ann@example.com"])),
            ..edit("g-2", CalEditKindDto::Put, Some(&href))
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { message, .. } if message.contains(&format!("identity for {ME}"))),
        "{refused}"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn a_rerun_over_a_changed_copy_is_not_done() {
    let f = fixture(Scheduling::default()).await;
    f.engine.shutdown().await;
    let written = vevent(1, "20261102T120000Z", "ACCEPTED", None)
        .replace("END:VEVENT", "BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:R\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT");
    let other = vevent(1, "20261102T120000Z", "ACCEPTED", None)
        .replace("END:VEVENT", "X-OTHER-CLIENT:changed\r\nEND:VEVENT");
    f.connector.dav.put("meet.ics", "\"after\"", &other);
    let dir = executor::ics_dir(&f.stores);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c-8.ics"), &written).unwrap();
    let mut op = CalOp::new("c-8".into(), ACCOUNT, CalOpKind::Put, UID.into());
    op.calendar_href = Some(WORK.into());
    op.href = Some(MEET.into());
    op.if_match = Some("\"before\"".into());
    op.ics_file = Some("c-8.ics".into());
    f.stores
        .config
        .write(move |tx| Ok(executor::insert(tx, &op)?))
        .await
        .unwrap();
    f.engine.start_calendar_on(Arc::new(f.connector.clone()));
    let failed = f.settled("c-8").await;
    assert_eq!(
        failed.calendar.unwrap().conflict,
        Some(CalConflictDto::ServerWon),
        "a reminder edit that did not land is not done"
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_one_invitation_of_an_edit_cancels_both() {
    let f = fixture(Scheduling::default()).await;
    let guests = ["ann@example.com", "bo@example.com"];
    let href = f
        .put("b-1", None, fields("Plan", "12:00:00", &guests))
        .await;
    f.connector
        .dav
        .fail_next("put", DavError::PreconditionFailed);
    created(
        f.engine
            .edit(CalEditDto {
                event: Some(fields("Plan", "13:00:00", &guests[..1])),
                ..edit("b-2", CalEditKindDto::Put, Some(&href))
            })
            .await
            .unwrap(),
    );
    f.settled("b-2").await;
    let held: Vec<_> = held_outbox(&f)
        .await
        .into_iter()
        .filter(|(id, _)| id.starts_with("b-2"))
        .collect();
    assert_eq!(held.len(), 2, "{held:?}");
    let id = f
        .engine
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.op_id == "b-2#request")
        .unwrap()
        .id;
    f.engine.cancel_impl(id).await.unwrap();
    assert!(
        held_outbox(&f)
            .await
            .iter()
            .all(|(id, _)| !id.starts_with("b-2")),
        "the CANCEL does not go out alone"
    );
    assert_eq!(
        f.op("b-2").await.calendar.unwrap().reply,
        Some(CalReplyStateDto::Cancelled)
    );
    f.engine.shutdown().await;
}

#[tokio::test]
async fn applying_an_older_reply_by_hand_is_refused() {
    let f = fixture(Scheduling::default()).await;
    organized_and_synced(&f, "NEEDS-ACTION").await;
    f.engine.shutdown().await;
    let mail = FakeMail(Mutex::new(Some(reply_mail(BOSS, Some(PASS)))));
    queue_invite(&f).await;
    invite::process_inbox(&f.stores, &mail).await;
    assert_eq!(applies(&f).await.len(), 1);
    let older = organized("DECLINED")
        .replace("VERSION:2.0\r\n", "VERSION:2.0\r\nMETHOD:REPLY\r\n")
        .replace("DTSTAMP:20261101T120000Z", "DTSTAMP:20261101T130000Z");
    let refused = f
        .engine
        .submit(CalRequest {
            op_id: "m-8".into(),
            account: ACCOUNT,
            message: MessageRowId(1),
            source: message(&older, "REPLY", Some(PASS)),
            action: CalAction::Apply {
                allow_organizer_change: false,
            },
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Invalid { message, .. } if message.contains("newer reply")),
        "{refused}"
    );
}
