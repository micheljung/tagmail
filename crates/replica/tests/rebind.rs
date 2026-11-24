//! Pairing with another server keeps the earlier server's unsent work
//! local: nothing written for server A reaches server B.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::Fixture;
use serde_json::Value;
use tagmail_api_core::compose::{Compose, ComposeError, DraftInput};
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::writes::OpRequest;
use tagmail_core::ThreadId;
use tagmail_core::ops::OpKind;
use tagmail_replica::compose::{DesktopCompose, OutboxState};
use tagmail_replica::durable::FROM_OTHER_SERVER;
use tagmail_replica::intents::IntentState;
use tagmail_replica::{Binding, Durable, Layout, Replica, ServerClient, Settings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use url::Url;

fn input(identity_id: i64, subject: &str) -> DraftInput {
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
        attachment_ids: Vec::new(),
        reply: None,
        forward_of: None,
        save_now: true,
        client_id: None,
        forwarded: Vec::new(),
    }
}

fn url(f: &Fixture) -> Url {
    Url::parse(&format!("http://{}/", f.server.local_addr())).unwrap()
}

async fn open(on: &Fixture, layout_of: &Fixture) -> Arc<Replica> {
    let client = ServerClient::new(&url(on), Some(on.token.clone())).unwrap();
    let replica = Replica::open(
        Layout::new(layout_of.desktop.path()),
        client,
        Settings::default(),
    )
    .await
    .unwrap();
    replica.rebootstrap().await.unwrap();
    replica
}

async fn items(f: &Fixture, path: &str, key: &str) -> Vec<Value> {
    let v: Value = f
        .owner
        .get(f.url(path))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v[key].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn another_server_gets_nothing_written_for_the_first() {
    let a = Fixture::start().await;
    let b = Fixture::start().await;
    let on_a = a.replica().await;
    let compose = DesktopCompose(Arc::clone(&on_a));
    let me = compose.identities().await.unwrap()[0].id;
    let saved = compose
        .create_draft(input(me, "Saved on A"))
        .await
        .unwrap()
        .draft;
    on_a.flush().await.unwrap();
    let queued = compose
        .create_draft(input(me, "Queued on A"))
        .await
        .unwrap()
        .draft;
    compose.send("send-a".into(), queued.id).await.unwrap();
    on_a.submit_op(OpRequest {
        op_id: "op-a".into(),
        kind: OpKind::MarkRead,
        thread_ids: vec![ThreadId(2)],
        email_ids: Vec::new(),
        label: None,
        mailbox: None,
    })
    .await
    .unwrap();
    drop(compose);
    drop(on_a);

    let on_b = open(&b, &a).await;
    on_b.flush().await.unwrap();

    assert!(items(&b, "/api/outbox", "items").await.is_empty());
    assert!(b.server_ops().await.is_empty());
    let drafts = b
        .server
        .store()
        .config()
        .read(|c| Ok(c.query_row("SELECT count(*) FROM draft", [], |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    assert_eq!(drafts, 0);

    let intent = on_b.durable().intent("op-a").unwrap().unwrap();
    assert_eq!(intent.state, IntentState::Failed);
    assert_eq!(intent.error.as_deref(), Some(FROM_OTHER_SERVER));
    assert!(on_b.retry_op("op-a".into()).await.is_err());

    let compose = DesktopCompose(Arc::clone(&on_b));
    let outbox = compose.outbox().await.unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].error.as_deref(), Some(FROM_OTHER_SERVER));
    assert!(matches!(
        compose.retry(outbox[0].id).await,
        Err(ComposeError::Conflict(_))
    ));
    let kept = compose.draft(saved.id).await.unwrap();
    assert_eq!(kept.subject, "Saved on A");
    assert!(!kept.saved);
    assert!(matches!(
        compose.send("send-b".into(), saved.id).await,
        Err(ComposeError::Conflict(_))
    ));
    let state: String = on_b
        .durable()
        .conn()
        .query_row("SELECT state FROM outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, OutboxState::Failed.as_str());
}

#[tokio::test]
async fn the_same_server_keeps_everything() {
    let a = Fixture::start().await;
    let on_a = a.replica().await;
    let compose = DesktopCompose(Arc::clone(&on_a));
    let me = compose.identities().await.unwrap()[0].id;
    let queued = compose
        .create_draft(input(me, "Queued"))
        .await
        .unwrap()
        .draft;
    compose.send("send-a".into(), queued.id).await.unwrap();
    drop(compose);
    drop(on_a);

    let again = open(&a, &a).await;
    assert_eq!(
        again.durable().bind_server(&url(&a)).unwrap(),
        Binding::Bound
    );
    again.flush().await.unwrap();
    let outbox = items(&a, "/api/outbox", "items").await;
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0]["opId"], "send-a");
}

/// A proxy to `upstream` that holds requests while the gate is closed.
async fn gated(upstream: SocketAddr) -> (SocketAddr, watch::Sender<bool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (open, gate) = watch::channel(true);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let mut gate = gate.clone();
            tokio::spawn(async move {
                let server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                });
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = client_read.read(&mut buf).await {
                    if n == 0
                        || gate.wait_for(|open| *open).await.is_err()
                        || server_write.write_all(&buf[..n]).await.is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    (addr, open)
}

#[tokio::test]
async fn a_send_in_flight_while_pairing_elsewhere_stays_behind() {
    let a = Fixture::start().await;
    let b = Fixture::start().await;
    let (proxy, open_gate) = gated(a.server.local_addr()).await;
    let via_proxy = Url::parse(&format!("http://{proxy}/")).unwrap();
    let on_a = Replica::open(
        Layout::new(a.desktop.path()),
        ServerClient::new(&via_proxy, Some(a.token.clone())).unwrap(),
        Settings::default(),
    )
    .await
    .unwrap();
    on_a.rebootstrap().await.unwrap();
    let compose = DesktopCompose(Arc::clone(&on_a));
    let me = compose.identities().await.unwrap()[0].id;
    let queued = compose
        .create_draft(input(me, "Queued on A"))
        .await
        .unwrap()
        .draft;
    compose.send("send-a".into(), queued.id).await.unwrap();
    let b_identity = b
        .server
        .store()
        .config()
        .read(|c| Ok(c.query_row("SELECT min(id) FROM identity", [], |r| r.get::<_, i64>(0))?))
        .await
        .unwrap();
    let b_draft: Value = b
        .owner
        .post(b.url("/api/drafts"))
        .json(&serde_json::json!({
            "identityId": b_identity, "to": [], "cc": [], "bcc": [], "subject": "B's own",
            "text": "private", "attachmentIds": [], "saveNow": true,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(b_draft["id"].as_i64(), Some(queued.id));

    open_gate.send_replace(false);
    let flushing = tokio::spawn({
        let on_a = Arc::clone(&on_a);
        async move { on_a.flush().await }
    });
    let state = |r: &Replica| -> String {
        r.durable()
            .conn()
            .query_row("SELECT state FROM outbox", [], |r| r.get(0))
            .unwrap()
    };
    while state(&on_a) != OutboxState::Uploading.as_str() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rebound = Durable::open(&Layout::new(a.desktop.path()).durable()).unwrap();
    assert_eq!(rebound.bind_server(&url(&b)).unwrap(), Binding::Changed);
    open_gate.send_replace(true);
    let _ = flushing.await.unwrap();
    drop(compose);

    assert_eq!(state(&on_a), OutboxState::Failed.as_str());
    let server_id: Option<i64> = on_a
        .durable()
        .conn()
        .query_row("SELECT server_draft_id FROM draft", [], |r| r.get(0))
        .unwrap();
    assert_eq!(server_id, None);
    drop(on_a);

    let on_b = open(&b, &a).await;
    on_b.flush().await.unwrap();
    assert!(items(&b, "/api/outbox", "items").await.is_empty());
    assert_eq!(state(&on_b), OutboxState::Failed.as_str());

    let Fixture {
        server,
        data: _data,
        keys: _keys,
        desktop: _desktop,
        ..
    } = b;
    server.shutdown().await.unwrap();
    let compose = DesktopCompose(Arc::clone(&on_b));
    let outbox = compose.outbox().await.unwrap();
    compose.cancel(outbox[0].id).await.unwrap();
    assert!(compose.outbox().await.unwrap().is_empty());
}

#[tokio::test]
async fn stopping_does_not_wait_for_a_stalled_send() {
    let a = Fixture::start().await;
    let (proxy, open_gate) = gated(a.server.local_addr()).await;
    let via_proxy = Url::parse(&format!("http://{proxy}/")).unwrap();
    let on_a = Replica::open(
        Layout::new(a.desktop.path()),
        ServerClient::new(&via_proxy, Some(a.token.clone())).unwrap(),
        Settings::default(),
    )
    .await
    .unwrap();
    on_a.rebootstrap().await.unwrap();
    let compose = DesktopCompose(Arc::clone(&on_a));
    let me = compose.identities().await.unwrap()[0].id;
    let queued = compose
        .create_draft(input(me, "Stalled"))
        .await
        .unwrap()
        .draft;
    open_gate.send_replace(false);
    compose.send("send-a".into(), queued.id).await.unwrap();
    let (stop, stopped) = watch::channel(false);
    let run = tokio::spawn(Arc::clone(&on_a).run(stopped));
    let uploading = || -> bool {
        on_a.durable()
            .conn()
            .query_row("SELECT state FROM outbox", [], |r| r.get::<_, String>(0))
            .unwrap()
            == OutboxState::Uploading.as_str()
    };
    while !uploading() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("run stops promptly")
        .unwrap();
}
