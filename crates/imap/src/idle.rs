//! IDLE (RFC 2177) with a renewal timer and DONE on demand.

use imap_next::client::Event;
use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::response::Data;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::conn::{Collected, ConnEvent};
use crate::error::{Error, Result};
use crate::session::TokioSession;
use crate::types::{IdleResult, IdleStop};

/// Whether an event during IDLE means the mailbox changed.
fn is_change(event: &ConnEvent) -> bool {
    match event {
        ConnEvent::Next(Event::DataReceived { data }) => matches!(
            data,
            Data::Exists(_) | Data::Expunge(_) | Data::Fetch { .. } | Data::Vanished { .. }
        ),
        ConnEvent::RawFetch(_) => true,
        _ => false,
    }
}

impl TokioSession {
    pub(crate) async fn idle_impl(&mut self, wake: &Notify) -> Result<IdleResult> {
        self.selected_state()?;
        if !self.caps.idle() {
            return Err(Error::Unsupported("IDLE"));
        }
        if self.conn.broken {
            return Err(self.conn.closed());
        }
        let result = self.idle_inner(wake).await;
        if result.as_ref().is_err_and(Error::is_fatal) {
            self.conn.broken = true;
        }
        result
    }

    async fn idle_inner(&mut self, wake: &Notify) -> Result<IdleResult> {
        let tag = self.conn.enqueue(CommandBody::Idle);
        let tag = tag.as_ref().to_owned();
        let timeout = self.conn.timeout;
        let mut out = Collected::default();

        let accepted = tokio::time::timeout(timeout, async {
            loop {
                let event = self.conn.next_event().await?;
                if matches!(event, ConnEvent::Next(Event::IdleAccepted { .. })) {
                    return Ok::<_, Error>(None);
                }
                if let Some(done) = self.conn.absorb(event, &tag, &mut out)? {
                    return Ok(Some(done));
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)??;
        if let Some(done) = accepted {
            self.split_events(out, |_| false);
            return Err(Error::No {
                code: done.code.as_ref().map(crate::session::code_name),
                text: format!("IDLE rejected: {}", done.text),
            });
        }

        let deadline = Instant::now() + self.idle_renew;
        let stop = loop {
            let step = tokio::select! {
                event = self.conn.next_event() => Some(event?),
                () = tokio::time::sleep_until(deadline) => None,
                () = wake.notified() => {
                    break IdleStop::Woken;
                }
            };
            let Some(event) = step else {
                break IdleStop::Renew;
            };
            let change = is_change(&event);
            if self.conn.absorb(event, &tag, &mut out)?.is_some() {
                return Err(Error::Protocol("IDLE ended by the server".into()));
            }
            if change {
                break IdleStop::Changed;
            }
        };

        if !self.conn.set_idle_done() {
            return Err(Error::Protocol("IDLE not active".into()));
        }
        let done = tokio::time::timeout(timeout, async {
            loop {
                let event = self.conn.next_event().await?;
                if let Some(done) = self.conn.absorb(event, &tag, &mut out)? {
                    return Ok::<_, Error>(done);
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)??;
        if done.kind != imap_next::imap_types::response::StatusKind::Ok {
            tracing::warn!(text = done.text, "IDLE completed with an error");
        }
        self.split_events(out, |_| false);
        Ok(IdleResult {
            stop,
            events: std::mem::take(&mut self.events),
        })
    }
}
