//! The desktop push channel: the SPA asks for events through
//! `GET /api/desktop/events/start?since=`, the shell runs an api-core
//! [`EventSession`] over the local change feed and `emit`s its events.
//! A new start (page reload) replaces the running session.

use std::sync::Arc;

use serde_json::Value;
use tagmail_api_core::api::gone_error;
use tagmail_api_core::dto::ApiErrorDto;
use tagmail_api_core::events::{Event, EventSession, OpenError};
use tagmail_core::{ChangeCursor, ChangeFilter};
use tokio::sync::watch;

use crate::backend::Backend;

pub const CHANGES: &str = "api:changes";
pub const RESET: &str = "api:reset";

pub trait Emit: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    pub since: Option<ChangeCursor>,
    generation: u64,
}

#[derive(Debug, Clone)]
pub struct EventControl {
    tx: watch::Sender<Option<Start>>,
}

pub type Starts = watch::Receiver<Option<Start>>;

impl EventControl {
    pub fn new() -> (EventControl, Starts) {
        let (tx, rx) = watch::channel(None);
        (EventControl { tx }, rx)
    }

    pub fn start(&self, since: Option<ChangeCursor>) {
        self.tx.send_modify(|current| {
            let generation = current.map_or(0, |s| s.generation + 1);
            *current = Some(Start { since, generation });
        });
    }
}

fn reset_payload(error: &ApiErrorDto) -> Value {
    serde_json::to_value(error).expect("DTOs serialize infallibly")
}

fn internal_error() -> ApiErrorDto {
    ApiErrorDto {
        error: "internal".to_owned(),
        message: "reading local changes failed; reload".to_owned(),
        field: None,
    }
}

/// Serves starts until the [`EventControl`] is dropped. Without a local
/// store yet a start waits for the next one; the replica emits a reset once
/// its first bootstrap is in.
pub async fn run<B: Backend>(backend: Arc<B>, mut starts: Starts, emit: impl Emit) {
    let mut restart = false;
    loop {
        if !restart && starts.changed().await.is_err() {
            return;
        }
        restart = false;
        let Some(start) = *starts.borrow_and_update() else {
            continue;
        };
        let Some((store, source)) = backend.events() else {
            continue;
        };
        let mut session = match EventSession::open(
            store,
            &*source,
            start.since,
            ChangeFilter::WithCalendar,
        )
        .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(OpenError::Gone)) => {
                emit.emit(RESET, reset_payload(&gone_error()));
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "opening the event session failed");
                emit.emit(RESET, reset_payload(&internal_error()));
                continue;
            }
        };
        loop {
            tokio::select! {
                event = session.next() => match event {
                    Ok(Some(Event::Changes(changes))) => emit.emit(
                        CHANGES,
                        serde_json::to_value(&changes).expect("DTOs serialize infallibly"),
                    ),
                    Ok(Some(Event::Reset(error))) => {
                        emit.emit(RESET, reset_payload(&error));
                        break;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading local changes failed");
                        emit.emit(RESET, reset_payload(&internal_error()));
                        break;
                    }
                },
                changed = starts.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    restart = true;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tagmail_core::CacheEpoch;
    use tagmail_store::{ChangeOp, Entity, NewChange, changes};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::backend::LocalBackend;
    use crate::state::{self, Local};

    struct Channel(Mutex<mpsc::UnboundedSender<(String, Value)>>);

    impl Emit for Channel {
        fn emit(&self, event: &str, payload: Value) {
            let _ = self.0.lock().unwrap().send((event.to_owned(), payload));
        }
    }

    async fn append(local: &Local, id: i64) -> ChangeCursor {
        let epoch = local.store.cache().epoch();
        let head = local
            .store
            .cache()
            .write(move |tx| {
                changes::append(
                    tx,
                    &NewChange {
                        entity: Entity::Thread,
                        entity_id: id,
                        op: ChangeOp::Upsert,
                        payload: None,
                    },
                )?;
                changes::head(tx, epoch)
            })
            .await
            .unwrap();
        local.feed.publish(head);
        head
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<(String, Value)>) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event in time")
            .unwrap()
    }

    fn ids(payload: &Value) -> Vec<i64> {
        payload["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_i64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn emits_changes_after_start_and_restarts_on_a_new_start() {
        let dir = TempDir::new().unwrap();
        let local = state::open(dir.path()).await.unwrap();
        let (control, starts) = EventControl::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = Arc::new(LocalBackend::new(local));
        tokio::spawn(run(Arc::clone(&backend), starts, Channel(Mutex::new(tx))));
        let local = backend.local();

        let before = append(local, 1).await;
        control.start(Some(before));
        append(local, 2).await;
        let (name, payload) = next(&mut rx).await;
        assert_eq!(name, CHANGES);
        assert_eq!(ids(&payload), [2]);

        control.start(Some(ChangeCursor {
            epoch: before.epoch,
            seq: 0,
        }));
        let (_, replay) = next(&mut rx).await;
        assert_eq!(ids(&replay), [1, 2]);

        control.start(Some(ChangeCursor {
            epoch: CacheEpoch(before.epoch.0 ^ 1),
            seq: 0,
        }));
        let (name, payload) = next(&mut rx).await;
        assert_eq!(name, RESET);
        assert_eq!(payload["error"], "gone");
    }
}
