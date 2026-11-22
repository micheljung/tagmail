//! Push channel (plan §3.6): the engine publishes the change-log head after
//! each commit; every SSE client (or desktop `emit` loop) runs an
//! [`EventSession`] that turns head moves into coalesced `changes` events.

use std::sync::Arc;
use std::time::Duration;

use tagmail_core::ChangeCursor;
use tokio::sync::watch;

use crate::api::{change_page_dto, gone_error};
use crate::dto::{ApiErrorDto, ChangesDto};
use crate::store::{ChangePage, Store, StoreResult};

/// Changes arriving within this window go out as one event.
pub const COALESCE: Duration = Duration::from_millis(100);
/// Changes per event; a larger backlog is sent as several events.
pub const EVENT_PAGE: usize = 500;

/// Source of change-log head updates.
pub trait EventSource: Send + Sync + 'static {
    /// Yields the head after every commit that appended changes. A new
    /// epoch means the cache was rebuilt.
    fn subscribe(&self) -> watch::Receiver<ChangeCursor>;
}

/// The in-process [`EventSource`]: the engine calls [`ChangeFeed::publish`]
/// after each write transaction.
#[derive(Debug)]
pub struct ChangeFeed {
    tx: watch::Sender<ChangeCursor>,
}

impl ChangeFeed {
    pub fn new(head: ChangeCursor) -> ChangeFeed {
        ChangeFeed {
            tx: watch::Sender::new(head),
        }
    }

    pub fn publish(&self, head: ChangeCursor) {
        self.tx.send_replace(head);
    }
}

impl EventSource for ChangeFeed {
    fn subscribe(&self) -> watch::Receiver<ChangeCursor> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// SSE `id` is `next`, so `Last-Event-ID` resumes after this page.
    Changes(ChangesDto),
    /// The cache epoch changed: refetch everything (web) or re-bootstrap
    /// (replica). The session ends after this event.
    Reset(ApiErrorDto),
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Changes(_) => "changes",
            Event::Reset(_) => "reset",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Event::Changes(c) => Some(&c.next),
            Event::Reset(_) => None,
        }
    }

    pub fn data(&self) -> String {
        let json = match self {
            Event::Changes(c) => serde_json::to_string(c),
            Event::Reset(e) => serde_json::to_string(e),
        };
        json.expect("DTOs serialize infallibly")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// `Last-Event-ID` is from another epoch or below the trim floor: 410.
    Gone,
}

pub struct EventSession<S> {
    store: Arc<S>,
    head: watch::Receiver<ChangeCursor>,
    cursor: ChangeCursor,
    pending: Option<ChangesDto>,
    ended: bool,
}

impl<S: Store> EventSession<S> {
    /// Starts after `resume` (the client's `Last-Event-ID`), or at the
    /// current head.
    pub async fn open(
        store: Arc<S>,
        source: &dyn EventSource,
        resume: Option<ChangeCursor>,
    ) -> StoreResult<Result<EventSession<S>, OpenError>> {
        let head = source.subscribe();
        let mut session = EventSession {
            store,
            head,
            cursor: ChangeCursor {
                epoch: tagmail_core::CacheEpoch(0),
                seq: 0,
            },
            pending: None,
            ended: false,
        };
        match resume {
            None => session.cursor = session.store.head().await?,
            Some(cursor) => match session.store.changes_since(cursor, EVENT_PAGE).await? {
                ChangePage::Gone => return Ok(Err(OpenError::Gone)),
                ChangePage::Page {
                    changes,
                    next,
                    more,
                } => {
                    session.cursor = next;
                    if !changes.is_empty() {
                        session.pending = Some(change_page_dto(changes, next, more));
                    }
                }
            },
        }
        Ok(Ok(session))
    }

    /// The next event, or `None` once the session ended (after a reset, or
    /// when the event source is gone).
    pub async fn next(&mut self) -> StoreResult<Option<Event>> {
        if self.ended {
            return Ok(None);
        }
        if let Some(page) = self.pending.take() {
            return Ok(Some(Event::Changes(page)));
        }
        loop {
            if self.head.borrow_and_update().epoch != self.cursor.epoch {
                return Ok(Some(self.reset()));
            }
            match self.store.changes_since(self.cursor, EVENT_PAGE).await? {
                ChangePage::Gone => return Ok(Some(self.reset())),
                ChangePage::Page {
                    changes,
                    next,
                    more,
                } if !changes.is_empty() => {
                    self.cursor = next;
                    return Ok(Some(Event::Changes(change_page_dto(changes, next, more))));
                }
                ChangePage::Page { .. } => {}
            }
            if self.head.changed().await.is_err() {
                self.ended = true;
                return Ok(None);
            }
            tokio::time::sleep(COALESCE).await;
        }
    }

    fn reset(&mut self) -> Event {
        self.ended = true;
        Event::Reset(gone_error())
    }
}
