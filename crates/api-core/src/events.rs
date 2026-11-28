//! Push channel (plan §3.6): the engine publishes the change-log head after
//! each commit; every SSE client (or desktop `emit` loop) runs an
//! [`EventSession`] that turns head moves into coalesced `changes` events.

use std::sync::Arc;
use std::time::Duration;

use tagmail_core::{ChangeCursor, ChangeFilter};
use tokio::sync::{broadcast, watch};

use crate::api::{change_page_dto, gone_error};
use crate::dto::{AddressDto, ApiErrorDto, ChangesDto};
use crate::store::{ChangePage, Store, StoreResult};

/// Changes arriving within this window go out as one event.
pub const COALESCE: Duration = Duration::from_millis(100);
/// Changes per event; a larger backlog is sent as several events.
pub const EVENT_PAGE: usize = 500;

/// Committed arrival batches buffered for a slow subscriber; beyond this
/// it gets `RecvError::Lagged`.
pub const ARRIVALS_CAPACITY: usize = 64;

/// A newly arrived message that may warrant a notification (contract §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub account_id: i64,
    pub message_id: i64,
    pub thread_id: Option<i64>,
    pub from: Option<AddressDto>,
    pub subject: Option<String>,
    /// Unix seconds the server received it (INTERNALDATE), not the
    /// sender-chosen `Date` header.
    pub date: i64,
}

/// A due reminder for the push subscriptions of `sessions` (session hashes
/// in lowercase hex), each of which it reaches once (plan §3.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderPush {
    pub alarm_key: String,
    pub account_id: i64,
    pub title: Option<String>,
    pub start: i64,
    /// Unix seconds; a push still undelivered
    /// [`crate::calendar_dto::REMINDER_GRACE_SECS`] later is dropped.
    pub fire_at: i64,
    pub sessions: Vec<String>,
}

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
    arrivals: broadcast::Sender<Arc<[Arrival]>>,
    reminders: broadcast::Sender<Arc<ReminderPush>>,
}

impl ChangeFeed {
    pub fn new(head: ChangeCursor) -> ChangeFeed {
        ChangeFeed {
            tx: watch::Sender::new(head),
            arrivals: broadcast::Sender::new(ARRIVALS_CAPACITY),
            reminders: broadcast::Sender::new(ARRIVALS_CAPACITY),
        }
    }

    pub fn publish(&self, head: ChangeCursor) {
        self.tx.send_replace(head);
    }

    /// Called after the commit that inserted `arrivals`.
    pub fn publish_arrivals(&self, arrivals: Vec<Arrival>) {
        if arrivals.is_empty() {
            return;
        }
        // No subscriber is not an error: nothing wants notifications.
        let _ = self.arrivals.send(arrivals.into());
    }

    pub fn subscribe_arrivals(&self) -> broadcast::Receiver<Arc<[Arrival]>> {
        self.arrivals.subscribe()
    }

    /// Called after the commit that recorded the reminder as sent.
    pub fn publish_reminder(&self, reminder: ReminderPush) {
        let _ = self.reminders.send(reminder.into());
    }

    pub fn subscribe_reminders(&self) -> broadcast::Receiver<Arc<ReminderPush>> {
        self.reminders.subscribe()
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
    filter: ChangeFilter,
    pending: Option<ChangesDto>,
    ended: bool,
}

impl<S: Store> EventSession<S> {
    /// Starts after `resume` (the client's `Last-Event-ID`), or at the
    /// current head; sends only the changes `filter` lets through.
    pub async fn open(
        store: Arc<S>,
        source: &dyn EventSource,
        resume: Option<ChangeCursor>,
        filter: ChangeFilter,
    ) -> StoreResult<Result<EventSession<S>, OpenError>> {
        let head = source.subscribe();
        let mut session = EventSession {
            store,
            head,
            cursor: ChangeCursor {
                epoch: tagmail_core::CacheEpoch(0),
                seq: 0,
            },
            filter,
            pending: None,
            ended: false,
        };
        match resume {
            None => session.cursor = session.store.head().await?,
            Some(cursor) => match session
                .store
                .changes_since(cursor, EVENT_PAGE, filter)
                .await?
            {
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
            match self
                .store
                .changes_since(self.cursor, EVENT_PAGE, self.filter)
                .await?
            {
                ChangePage::Gone => return Ok(Some(self.reset())),
                ChangePage::Page {
                    changes,
                    next,
                    more,
                } => {
                    self.cursor = next;
                    if !changes.is_empty() {
                        return Ok(Some(Event::Changes(change_page_dto(changes, next, more))));
                    }
                }
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

#[cfg(test)]
mod tests {
    use tagmail_core::CacheEpoch;

    use super::*;

    fn arrival(message_id: i64) -> Arrival {
        Arrival {
            account_id: 1,
            message_id,
            thread_id: Some(message_id),
            from: Some(AddressDto {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
            }),
            subject: Some("Hi".into()),
            date: 0,
        }
    }

    #[tokio::test]
    async fn arrivals_reach_every_subscriber() {
        let feed = ChangeFeed::new(ChangeCursor {
            epoch: CacheEpoch(1),
            seq: 0,
        });
        feed.publish_arrivals(vec![arrival(1)]);
        let mut a = feed.subscribe_arrivals();
        let mut b = feed.subscribe_arrivals();
        feed.publish_arrivals(Vec::new());
        feed.publish_arrivals(vec![arrival(2), arrival(3)]);
        for rx in [&mut a, &mut b] {
            assert_eq!(&*rx.recv().await.unwrap(), [arrival(2), arrival(3)]);
            assert!(rx.try_recv().is_err(), "empty batch published");
        }
    }
}
