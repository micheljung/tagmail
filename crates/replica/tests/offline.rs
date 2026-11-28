//! AC 19 (plan §6 row 19) at the replica: with the server stopped the
//! desktop reads, searches and queues writes; once it is back the queue
//! is sent exactly once, conflicts surface as failed, and a rebuilt server
//! cache re-bootstraps the replica without losing the queue.

#[cfg(feature = "calendar")]
mod calendar_seed;
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

#[cfg(feature = "calendar")]
const INVITE: &str = "From: boss@example.com\r\nTo: me@x.example\r\nSubject: Planning\r\n\
     Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
     MIME-Version: 1.0\r\nContent-Type: text/calendar; method=REQUEST; charset=UTF-8\r\n\r\n\
     BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
     UID:plan@example.com\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261201T090000Z\r\n\
     DTEND:20261201T100000Z\r\nSUMMARY:Planning\r\nORGANIZER:mailto:boss@example.com\r\n\
     ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@x.example\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// Message 10 becomes an invitation; 13 is marked as one but its raw is
/// never downloaded by the replica.
#[cfg(feature = "calendar")]
async fn invitations(stack: &Stack) {
    let key = stack.key("g:10");
    let blobs = stack.server().store().blobs().clone();
    tokio::task::spawn_blocking(move || {
        let mut writer = blobs
            .raw_writer(key, tagmail_store::DEFAULT_RAW_CAP)
            .unwrap();
        writer.write_chunk(INVITE.as_bytes()).unwrap();
        writer.finish().unwrap();
    })
    .await
    .unwrap();
    stack
        .server()
        .store()
        .cache()
        .write(|tx| {
            tx.execute(
                "UPDATE message SET has_calendar = 1 WHERE id IN (10, 13)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[cfg(feature = "calendar")]
#[tokio::test]
async fn offline_invitation_reports_unknown_sender_auth() {
    let mut stack = Stack::start().await;
    invitations(&stack).await;
    let replica = stack.replica().await;
    replica.gate().await.unwrap();
    let router = Router::new(replica.clone());
    let online = get(&router, "/api/messages/10/invitation").await;
    assert_eq!(online.status, 200, "{}", body(&online));
    assert_ne!(json(&online)["senderAuth"], "unknown");
    let opened = get(&router, "/api/messages/10/render").await;
    assert_eq!(opened.status, 200, "{}", body(&opened));

    stack.stop().await;
    replica.client().link().mark_down();

    let offline = get(&router, "/api/messages/10/invitation").await;
    assert_eq!(offline.status, 200, "{}", body(&offline));
    let card = json(&offline);
    assert_eq!(card["senderAuth"], "unknown");
    assert_eq!(card["autoApply"], "none");
    assert_eq!(card["events"][0]["uid"], "plan@example.com");
    let uncached = get(&router, "/api/messages/13/invitation").await;
    assert_eq!(uncached.status, 503, "{}", body(&uncached));
    assert_eq!(json(&uncached)["error"], "offline");
}

#[cfg(feature = "calendar")]
async fn server_op(stack: &Stack, op_id: &str) -> (u16, Value) {
    let resp = stack
        .owner
        .get(stack.url(&format!("/api/ops/{op_id}")))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Fails a calendar op on the server as a CalDAV error would.
#[cfg(feature = "calendar")]
async fn fail_on_server(stack: &Stack, op_id: &'static str) {
    stack
        .server()
        .store()
        .config()
        .write(move |tx| {
            tx.execute(
                "UPDATE cal_op SET state = 'failed', error = 'calendar server error' \
                 WHERE op_id = ?1",
                [op_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Waits for the server's verdict on an op it holds.
#[cfg(feature = "calendar")]
async fn server_failed(stack: &Stack, op_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, op) = server_op(stack, op_id).await;
        if status == 200 && op["status"] == "failed" {
            return op;
        }
        assert!(std::time::Instant::now() < deadline, "{status} {op}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// An invitation answered offline: queued once, flushed once.
#[cfg(feature = "calendar")]
async fn queued_rsvp(stack: &mut Stack, replica: &Replica, router: &Router) {
    stack.stop().await;
    replica.client().link().mark_down();
    let rsvp = json!({ "opId": "rsvp-1", "messageId": "10", "partstat": "accepted" });
    let queued = post(router, "/api/calendar/rsvp", rsvp.clone()).await;
    assert_eq!(queued.status, 202, "{}", body(&queued));
    assert_eq!(json(&queued)["domain"], "cal");
    assert_eq!(json(&queued)["status"], "pending");
    let again = post(router, "/api/calendar/rsvp", rsvp).await;
    assert_eq!(again.status, 200, "{}", body(&again));
    replica.flush().await.unwrap();
    assert_eq!(
        replica
            .durable()
            .cal_intent("rsvp-1")
            .unwrap()
            .unwrap()
            .state,
        IntentState::Pending
    );
    stack.restart().await;
    replica.flush().await.unwrap();
}

#[cfg(feature = "calendar")]
#[tokio::test]
async fn calendar_read_offline_and_rsvp_flushed_once() {
    use tagmail_api_core::Store;

    let mut stack = Stack::start().await;
    invitations(&stack).await;
    calendar_seed::seed(
        stack.server().store(),
        stack.account,
        &["/cal/work/"],
        5,
        now(),
    )
    .await;
    let replica = stack.replica().await;
    replica.gate().await.unwrap();
    let router = Router::new(replica.clone());
    let online = json(&get(&router, "/api/calendars").await);
    assert_eq!(online["calendars"].as_array().unwrap().len(), 1, "{online}");

    stack.stop().await;
    replica.client().link().mark_down();
    let offline = get(&router, "/api/calendars").await;
    assert_eq!(offline.status, 200, "{}", body(&offline));
    assert_eq!(json(&offline), online);
    let listed = replica
        .store()
        .unwrap()
        .events_in(now() - 400 * DAY..now() + 400 * DAY, Vec::new())
        .await
        .unwrap();
    assert!(!listed.is_empty());
    stack.restart().await;

    queued_rsvp(&mut stack, &replica, &router).await;
    let flushed = replica.durable().cal_intent("rsvp-1").unwrap().unwrap();
    assert_ne!(flushed.state, IntentState::Pending, "{flushed:?}");
    assert!(flushed.on_server);
    let (status, first) = server_op(&stack, "rsvp-1").await;
    assert_eq!(status, 200, "{flushed:?}");
    assert_eq!(server_outbox_items(&stack, "rsvp-1#reply").await, 1);

    replica
        .durable()
        .conn()
        .execute("UPDATE cal_intent SET state = 'pending'", [])
        .unwrap();
    replica.flush().await.unwrap();
    let (_, again) = server_op(&stack, "rsvp-1").await;
    assert_eq!(
        again["seq"], first["seq"],
        "a lost acknowledgement is one op"
    );
    assert_eq!(
        server_outbox_items(&stack, "rsvp-1#reply").await,
        1,
        "and one REPLY"
    );
    assert_ne!(
        replica
            .durable()
            .cal_intent("rsvp-1")
            .unwrap()
            .unwrap()
            .state,
        IntentState::Pending
    );
    stack.stop().await;
}

#[cfg(feature = "calendar")]
#[tokio::test]
async fn a_server_failed_rsvp_is_retried_and_discarded_on_the_server() {
    let mut stack = Stack::start().await;
    invitations(&stack).await;
    let replica = stack.replica().await;
    replica.gate().await.unwrap();
    let router = Router::new(replica.clone());
    queued_rsvp(&mut stack, &replica, &router).await;
    fail_on_server(&stack, "rsvp-1").await;
    let failed = server_failed(&stack, "rsvp-1").await;
    let op = serde_json::from_value(failed.clone()).unwrap();
    replica.durable().settle_cal_intent(&op).unwrap();
    let local = replica.durable().cal_intent("rsvp-1").unwrap().unwrap();
    assert_eq!(local.state, IntentState::Failed, "{local:?}");
    assert!(local.on_server);
    let listed = json(&get(&router, "/api/ops/rsvp-1").await);
    assert_eq!(listed["calendar"], failed["calendar"], "{listed}");
    let attempts = failed["attempts"].as_u64().unwrap();

    let retried = post(&router, "/api/ops/rsvp-1/retry", json!({})).await;
    assert!(
        retried.status == 200 || retried.status == 202,
        "{}",
        body(&retried)
    );
    assert_eq!(json(&retried)["status"], "pending");
    let (_, on_server) = server_op(&stack, "rsvp-1").await;
    assert_ne!(
        on_server["status"], "failed",
        "retried on the server: {on_server}"
    );
    assert!(on_server["attempts"].as_u64() <= Some(attempts.max(1)));

    let discarded = call(&router, Method::Delete, "/api/ops/rsvp-1", b"").await;
    assert!(discarded.status < 300, "{}", body(&discarded));
    assert_eq!(server_op(&stack, "rsvp-1").await.0, 404);
    assert!(replica.durable().cal_intent("rsvp-1").unwrap().is_none());
    stack.stop().await;
}
