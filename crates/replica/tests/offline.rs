//! AC 19 (plan §6 row 19) at the replica: with the server stopped the
//! desktop reads, searches and queues writes; once it is back the queue
//! is sent exactly once, conflicts surface as failed, and a rebuilt server
//! cache re-bootstraps the replica without losing the queue.

mod fixture;

use fixture::{DAY, Stack, call, get, ids, json, now, search_path};
use serde_json::{Value, json};
use tagmail_api_core::{Method, PARTIAL_HEADER, Response};
use tagmail_core::EmailId;
use tagmail_core::ops::OpKind;
use tagmail_replica::intents::{EXPIRED, Intent, IntentState};
use tagmail_replica::router::{OLDER_MESSAGE, PARTIAL_OFFLINE};
use tagmail_replica::store::NOT_DOWNLOADED;
use tagmail_replica::{Replica, Router};

fn body(resp: &Response) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn partial(resp: &Response) -> Option<&str> {
    resp.header(PARTIAL_HEADER)
}

async fn post(router: &Router, path: &str, value: Value) -> Response {
    call(router, Method::Post, path, value.to_string().as_bytes()).await
}

/// Queues a draft to bob and its send under `op_id`; returns the draft id.
async fn queue_send(router: &Router, op_id: &str) -> i64 {
    let identities = json(&get(router, "/api/identities").await);
    let identity = identities["identities"][0]["id"].as_i64().unwrap();
    let draft = post(
        router,
        "/api/drafts",
        json!({
            "identityId": identity,
            "to": [{ "email": "bob@example.org" }],
            "subject": "Offline report",
            "text": "written on a train",
        }),
    )
    .await;
    assert_eq!(draft.status, 201, "{}", body(&draft));
    let draft = json(&draft)["id"].as_i64().unwrap();
    let sent = post(
        router,
        "/api/send",
        json!({ "opId": op_id, "draftId": draft }),
    )
    .await;
    assert_eq!(sent.status, 202, "{}", body(&sent));
    assert_eq!(json(&sent)["status"], "queued");
    draft
}

fn add_expired_intent(replica: &Replica, account: i64) {
    replica
        .durable()
        .add_intent(&Intent {
            op_id: "op-old".into(),
            seq: 0,
            kind: OpKind::Star,
            account_id: account,
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
}

fn local_outbox_state(replica: &Replica) -> Vec<String> {
    let conn = replica.durable().conn();
    let mut stmt = conn
        .prepare("SELECT state FROM outbox ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

async fn server_outbox_items(stack: &Stack, op_id: &str) -> usize {
    stack.owner_get("/api/outbox").await["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["opId"] == op_id)
        .count()
}

#[tokio::test]
async fn the_desktop_works_offline_and_sends_once_when_back() {
    let mut stack = Stack::start().await;
    let replica = stack.replica().await;
    let router = Router::new(replica.clone());
    let online_open = get(&router, "/api/messages/10/render").await;
    assert_eq!(online_open.status, 200, "{}", body(&online_open));

    stack.stop().await;
    replica.client().link().mark_down();

    assert_eq!(
        ids(&get(&router, "/api/views/inbox/index").await),
        vec![2, 3]
    );
    let thread = get(&router, "/api/threads/2").await;
    assert_eq!(thread.status, 200, "{}", body(&thread));
    let cached = get(&router, "/api/messages/10/render").await;
    assert_eq!(cached.status, 200, "{}", body(&cached));
    assert!(body(&cached).contains("quick fox"));
    assert!(!body(&cached).contains(NOT_DOWNLOADED));
    let starred = get(&router, "/api/messages/12/render").await;
    assert_eq!(starred.status, 200, "{}", body(&starred));
    assert!(body(&starred).contains("starred gem"));
    assert!(body(&starred).contains(NOT_DOWNLOADED));

    for (q, hits) in [
        ("fox", vec![2]),
        ("gem", vec![2]),
        ("news", vec![]),
        ("ancient", vec![]),
        ("other", vec![3]),
    ] {
        let resp = get(&router, &search_path(q)).await;
        assert_eq!(ids(&resp), hits, "offline search {q}");
        assert_eq!(partial(&resp), Some(PARTIAL_OFFLINE), "offline search {q}");
    }

    let older = get(&router, "/api/messages/13/render").await;
    assert_eq!(older.status, 503);
    assert_eq!(json(&older)["error"], "offline");
    assert_eq!(json(&older)["message"], OLDER_MESSAGE);

    add_expired_intent(&replica, stack.account);
    let op = post(
        &router,
        "/api/ops",
        json!({ "opId": "op-read", "kind": "markRead", "threadIds": [3] }),
    )
    .await;
    assert!(op.status == 201 || op.status == 202, "{}", body(&op));
    queue_send(&router, "send-1").await;
    replica.flush().await.unwrap();

    assert_eq!(
        replica
            .durable()
            .intent("op-old")
            .unwrap()
            .unwrap()
            .error
            .as_deref(),
        Some(EXPIRED)
    );
    assert_eq!(
        replica.durable().intent("op-read").unwrap().unwrap().state,
        IntentState::Pending
    );
    assert_eq!(local_outbox_state(&replica), vec!["queued"]);
    let outbox = json(&get(&router, "/api/outbox").await);
    assert_eq!(outbox["items"][0]["status"], "queued");

    stack.restart().await;
    stack
        .server()
        .store()
        .cache()
        .write(|tx| {
            tx.execute_batch(
                "DELETE FROM location WHERE message_id = 13; DELETE FROM message WHERE id = 13;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    replica.flush().await.unwrap();

    assert_eq!(server_outbox_items(&stack, "send-1").await, 1);
    assert_ne!(local_outbox_state(&replica), vec!["queued"]);
    replica
        .durable()
        .conn()
        .execute("UPDATE outbox SET state = 'queued'", [])
        .unwrap();
    replica.flush().await.unwrap();
    assert_eq!(
        server_outbox_items(&stack, "send-1").await,
        1,
        "a lost acknowledgement does not send twice"
    );

    let failed = json(&get(&router, "/api/ops?status=failed").await);
    let failed: Vec<&Value> = failed["ops"].as_array().unwrap().iter().collect();
    assert!(
        failed
            .iter()
            .any(|o| o["opId"] == "op-read" && o["error"].as_str().is_some()),
        "{failed:?}"
    );
    assert!(failed.iter().any(|o| o["opId"] == "op-old"));
    stack.stop().await;
}

#[tokio::test]
async fn a_rebuilt_server_cache_keeps_the_queue() {
    let mut stack = Stack::start().await;
    let replica = stack.replica().await;
    let router = Router::new(replica.clone());
    let before = replica.server_cursor().unwrap();

    stack.stop().await;
    replica.client().link().mark_down();
    let op = post(
        &router,
        "/api/ops",
        json!({ "opId": "op-star", "kind": "star", "threadIds": [3] }),
    )
    .await;
    assert!(op.status == 201 || op.status == 202, "{}", body(&op));
    queue_send(&router, "send-2").await;

    stack.wipe_cache();
    stack.restart().await;
    stack.seed().await;
    let err = replica.sync_once().await.unwrap_err();
    assert!(
        matches!(
            err,
            tagmail_replica::Error::Client(tagmail_replica::ClientError::Gone)
        ),
        "{err:?}"
    );
    replica.rebootstrap().await.unwrap();

    assert_ne!(replica.server_cursor().unwrap().epoch, before.epoch);
    assert!(replica.durable().intent("op-star").unwrap().is_some());
    assert_eq!(local_outbox_state(&replica).len(), 1);
    assert_eq!(
        ids(&get(&router, "/api/views/inbox/index").await),
        vec![2, 3]
    );

    replica.flush().await.unwrap();
    assert_eq!(server_outbox_items(&stack, "send-2").await, 1);
    let ops = stack.owner_get("/api/ops").await;
    assert!(
        ops["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["opId"] == "op-star")
    );
    stack.stop().await;
}
