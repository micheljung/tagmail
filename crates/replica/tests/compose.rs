//! Desktop compose against an in-process server: drafts are saved once
//! however often the save is replayed, and a send replays into one outbox
//! item.

mod common;

use std::sync::Arc;

use common::Fixture;
use serde_json::Value;
use tagmail_api_core::compose::{Compose, DraftInput, NewUpload};
use tagmail_api_core::dto::AddressDto;
use tagmail_replica::Replica;
use tagmail_replica::compose::{DesktopCompose, OutboxState};

async fn identity(compose: &DesktopCompose) -> i64 {
    compose.identities().await.unwrap()[0].id
}

fn input(identity_id: i64, subject: &str, attachment_ids: Vec<i64>) -> DraftInput {
    DraftInput {
        identity_id,
        to: vec![AddressDto {
            name: None,
            email: "bob@example.org".into(),
        }],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.into(),
        text: Some("hello".into()),
        html: None,
        attachment_ids,
        reply: None,
        forward_of: None,
        save_now: true,
        client_id: None,
        forwarded: Vec::new(),
    }
}

async fn server_drafts(f: &Fixture) -> Vec<(i64, String)> {
    f.server
        .store()
        .config()
        .read(|c| {
            let mut stmt = c.prepare("SELECT id, content FROM draft ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

async fn server_outbox(f: &Fixture) -> Vec<Value> {
    let outbox: Value = f
        .owner
        .get(f.url("/api/outbox"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    outbox["items"].as_array().cloned().unwrap_or_default()
}

fn forget_server_draft(replica: &Replica) {
    replica
        .durable()
        .conn()
        .execute(
            "UPDATE draft SET server_draft_id = NULL, synced_version = NULL",
            [],
        )
        .unwrap();
}

#[tokio::test]
async fn a_local_draft_reaches_the_server_once() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    let compose = DesktopCompose(Arc::clone(&replica));
    let me = identity(&compose).await;

    let dir = compose.upload_dir();
    let file = dir.join("notes.txt");
    std::fs::write(&file, b"attached").unwrap();
    let upload = compose
        .add_upload(NewUpload {
            name: "notes.txt".into(),
            content_type: "text/plain".into(),
            file,
            size: 8,
        })
        .await
        .unwrap();
    let created = compose
        .create_draft(input(me, "Plans", vec![upload.id.parse().unwrap()]))
        .await
        .unwrap();
    assert!(created.created);
    assert!(!created.draft.saved);
    assert_eq!(created.draft.attachments.len(), 1);
    assert!(server_drafts(&f).await.is_empty(), "nothing synced yet");

    replica.flush().await.unwrap();
    let saved = server_drafts(&f).await;
    assert_eq!(saved.len(), 1);
    assert!(compose.draft(created.draft.id).await.unwrap().saved);
    let server: Value = f
        .owner
        .get(f.url(&format!("/api/drafts/{}", saved[0].0)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(server["subject"], "Plans");
    assert_eq!(server["attachments"][0]["name"], "notes.txt");

    forget_server_draft(&replica);
    compose
        .update_draft(
            created.draft.id,
            input(me, "Plans v2", vec![upload.id.parse().unwrap()]),
        )
        .await
        .unwrap();
    replica.flush().await.unwrap();
    let saved = server_drafts(&f).await;
    assert_eq!(saved.len(), 1, "a replayed save updates the same draft");
    assert!(saved[0].1.contains("Plans v2"));

    compose.delete_draft(created.draft.id).await.unwrap();
    assert!(compose.draft(created.draft.id).await.is_err());
    replica.flush().await.unwrap();
    assert!(server_drafts(&f).await.is_empty());
}

#[tokio::test]
async fn a_replayed_send_is_one_server_outbox_item() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    let compose = DesktopCompose(Arc::clone(&replica));
    let me = identity(&compose).await;
    let draft = compose
        .create_draft(input(me, "Report", Vec::new()))
        .await
        .unwrap()
        .draft;

    let queued = compose.send("send-1".into(), draft.id).await.unwrap();
    assert!(queued.created);
    let again = compose.send("send-1".into(), draft.id).await.unwrap();
    assert!(!again.created);
    assert_eq!(compose.outbox().await.unwrap().len(), 1);
    assert!(compose.draft(draft.id).await.unwrap().queued);
    assert!(
        compose
            .update_draft(draft.id, input(me, "x", Vec::new()))
            .await
            .is_err()
    );
    assert!(server_outbox(&f).await.is_empty());

    replica.flush().await.unwrap();
    let items = server_outbox(&f).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["opId"], "send-1");

    replica
        .durable()
        .conn()
        .execute("UPDATE outbox SET state = 'queued'", [])
        .unwrap();
    replica.flush().await.unwrap();
    let items = server_outbox(&f).await;
    assert_eq!(items.len(), 1, "the lost ack replays into the same item");
    assert_eq!(server_drafts(&f).await.len(), 1);
    let local = compose.outbox().await.unwrap();
    assert_eq!(local.len(), 1);
    let state: String = replica
        .durable()
        .conn()
        .query_row("SELECT state FROM outbox", [], |r| r.get(0))
        .unwrap();
    assert!(
        [OutboxState::Submitted, OutboxState::Failed]
            .iter()
            .any(|s| s.as_str() == state),
        "{state}"
    );
}

#[tokio::test]
async fn a_draft_without_recipients_is_not_sent() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    let compose = DesktopCompose(Arc::clone(&replica));
    let me = identity(&compose).await;
    let mut empty = input(me, "Nobody", Vec::new());
    empty.to.clear();
    let draft = compose.create_draft(empty).await.unwrap().draft;
    assert!(compose.send("send-2".into(), draft.id).await.is_err());
    assert!(compose.outbox().await.unwrap().is_empty());
}

async fn mark_sent_on_server(f: &Fixture, op_id: &'static str) {
    for _ in 0..200 {
        let changed = f
            .server
            .store()
            .config()
            .write(move |tx| {
                Ok(tx.execute(
                    "UPDATE outbox SET state = 'sent' WHERE op_id = ?1 AND state != 'sending'",
                    [op_id],
                )?)
            })
            .await
            .unwrap();
        if changed == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the server outbox item stays in flight");
}

async fn outbox_changes(replica: &Replica) -> Vec<(String, Option<String>)> {
    replica
        .store()
        .unwrap()
        .cache()
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT op, payload FROM change_log WHERE entity = 'outbox' ORDER BY seq",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_finished_send_is_published_as_sent() {
    let f = Fixture::start().await;
    let replica = f.replica().await;
    let compose = DesktopCompose(Arc::clone(&replica));
    let me = identity(&compose).await;
    let draft = compose
        .create_draft(input(me, "Done", Vec::new()))
        .await
        .unwrap()
        .draft;
    compose.send("send-3".into(), draft.id).await.unwrap();
    replica.flush().await.unwrap();
    mark_sent_on_server(&f, "send-3").await;
    replica
        .durable()
        .conn()
        .execute("UPDATE outbox SET state = 'submitted'", [])
        .unwrap();

    replica.flush().await.unwrap();

    assert!(compose.outbox().await.unwrap().is_empty());
    let changes = outbox_changes(&replica).await;
    let sent = changes
        .iter()
        .position(|(op, p)| {
            op == "upsert"
                && p.as_deref()
                    .is_some_and(|p| serde_json::from_str::<Value>(p).unwrap()["status"] == "sent")
        })
        .unwrap_or_else(|| panic!("no sent change in {changes:?}"));
    assert!(
        changes[sent + 1..].iter().all(|(op, _)| op == "delete"),
        "{changes:?}"
    );
}
