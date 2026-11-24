//! Intents against an in-process server: optimistic overlay, flush with
//! op-id dedupe, failures surfaced, expiry and server-side completion.

mod common;

use common::{DAY, Fixture, now};
use tagmail_api_core::ops_dto::{OpDto, OpKindDto, OpStatusDto};
use tagmail_api_core::writes::{OpRequest, Submitted};
use tagmail_api_core::{Store, View};
use tagmail_core::ops::OpKind;
use tagmail_core::{EmailId, MailboxRowId, ThreadId};
use tagmail_replica::Replica;
use tagmail_replica::intents::{EXPIRED, Intent, IntentState};
use tagmail_store::{ChangeOp, Entity, NewChange, changes};

fn op(op_id: &str, kind: OpKind) -> OpRequest {
    OpRequest {
        op_id: op_id.into(),
        kind,
        thread_ids: vec![ThreadId(2)],
        email_ids: Vec::new(),
        label: None,
        mailbox: None,
    }
}

async fn inbox(replica: &Replica) -> Vec<ThreadId> {
    replica
        .store()
        .unwrap()
        .view_index(View::Inbox)
        .await
        .unwrap()
        .ids
}

async fn unread(replica: &Replica) -> u32 {
    replica
        .store()
        .unwrap()
        .threads(vec![ThreadId(2)])
        .await
        .unwrap()
        .rows[0]
        .unread_count
}

fn state(replica: &Replica, op_id: &str) -> (IntentState, Option<String>) {
    let i = replica.durable().intent(op_id).unwrap().unwrap();
    (i.state, i.error)
}

#[tokio::test]
async fn an_intent_shows_before_flush_and_a_replay_stays_one_op() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    assert_eq!(inbox(&replica).await, vec![ThreadId(2)]);
    let head = replica.store().unwrap().head().await.unwrap();

    let Submitted::Created(created) = replica
        .submit_op(op("op-a", OpKind::Archive))
        .await
        .unwrap()
    else {
        panic!("not created");
    };

    assert_eq!(created.status, OpStatusDto::Pending);
    assert_eq!(inbox(&replica).await, Vec::<ThreadId>::new());
    let archive = replica
        .store()
        .unwrap()
        .view_index(View::Mailbox(MailboxRowId(2)))
        .await
        .unwrap()
        .ids;
    assert_eq!(archive, vec![ThreadId(2)]);
    assert!(f.server_ops().await.is_empty(), "nothing flushed yet");
    let logged = replica.store().unwrap().head().await.unwrap();
    assert!(
        logged.seq > head.seq,
        "the overlay is in the local change log"
    );
    assert!(matches!(
        replica
            .submit_op(op("op-a", OpKind::Archive))
            .await
            .unwrap(),
        Submitted::Existing(_)
    ));

    replica.flush().await.unwrap();
    assert_eq!(state(&replica, "op-a").0, IntentState::Sent);
    replica
        .durable()
        .conn()
        .execute("UPDATE intent SET state = 'pending', next_at = NULL", [])
        .unwrap();
    replica.flush().await.unwrap();

    let ops = f.server_ops().await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["opId"], "op-a");
    assert_eq!(state(&replica, "op-a").0, IntentState::Sent);
    assert_eq!(
        inbox(&replica).await,
        Vec::<ThreadId>::new(),
        "still projected"
    );
    f.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_target_deleted_on_the_server_fails_the_intent_visibly() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    replica
        .submit_op(op("op-r", OpKind::MarkRead))
        .await
        .unwrap();
    assert_eq!(unread(&replica).await, 0);
    f.server
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch("DELETE FROM location; DELETE FROM message;")?;
            Ok(())
        })
        .await
        .unwrap();

    replica.flush().await.unwrap();

    let (s, error) = state(&replica, "op-r");
    assert_eq!(s, IntentState::Failed);
    assert!(error.unwrap().contains("no such"));
    assert_eq!(unread(&replica).await, 1, "the overlay is gone");
    let failed = replica.list_ops(Some(OpStatusDto::Failed)).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].op_id, "op-r");
    replica.discard_op("op-r".into()).await.unwrap();
    assert!(replica.list_ops(None).await.unwrap().is_empty());
    f.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_intent_older_than_ninety_days_expires_instead_of_flushing() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    replica
        .durable()
        .add_intent(&Intent {
            op_id: "op-old".into(),
            seq: 0,
            kind: OpKind::Star,
            account_id: f.account,
            email_ids: [EmailId::parse("g:10").unwrap()].into(),
            label: None,
            mailbox: None,
            created_at: now() - 91 * DAY,
            state: IntentState::Pending,
            attempts: 0,
            next_at: None,
            error: None,
        })
        .unwrap();

    replica.flush().await.unwrap();

    assert_eq!(
        state(&replica, "op-old"),
        (IntentState::Failed, Some(EXPIRED.to_owned()))
    );
    assert!(f.server_ops().await.is_empty());
    f.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_op_change_settles_the_intent() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    replica.submit_op(op("op-s", OpKind::Star)).await.unwrap();
    replica.flush().await.unwrap();
    let starred = |r: &Replica| {
        let store = r.store().unwrap();
        async move { store.threads(vec![ThreadId(2)]).await.unwrap().rows[0].starred }
    };
    assert!(starred(&replica).await);

    let done = OpDto {
        op_id: "op-s".into(),
        seq: 1,
        account_id: f.account,
        kind: OpKindDto::Star,
        status: OpStatusDto::Done,
        error: None,
        attempts: 1,
        thread_ids: Vec::new(),
        label: None,
        mailbox_id: None,
        created_at: now(),
    };
    let payload = serde_json::to_string(&done).unwrap();
    f.server
        .store()
        .cache()
        .write(move |tx| {
            changes::append(
                tx,
                &NewChange {
                    entity: Entity::Op,
                    entity_id: 1,
                    op: ChangeOp::Upsert,
                    payload: Some(payload),
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    replica.sync_once().await.unwrap();

    assert_eq!(state(&replica, "op-s").0, IntentState::Done);
    assert!(
        !starred(&replica).await,
        "without the overlay the replica shows the server state again"
    );
    f.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_sent_intent_settles_after_a_rebootstrap_skipped_its_verdict() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    replica
        .submit_op(op("op-s", OpKind::MarkRead))
        .await
        .unwrap();
    replica.flush().await.unwrap();
    assert_eq!(state(&replica, "op-s").0, IntentState::Sent);
    let mut settled = 0;
    for _ in 0..200 {
        settled = f
            .server
            .store()
            .config()
            .write(|tx| {
                Ok(tx.execute(
                    "UPDATE pending_op SET state = 'done' WHERE op_id = 'op-s' AND state != 'running'",
                    [],
                )?)
            })
            .await
            .unwrap();
        if settled == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(settled, 1);

    replica.rebootstrap().await.unwrap();
    replica.flush().await.unwrap();

    assert_eq!(state(&replica, "op-s").0, IntentState::Done);
}
