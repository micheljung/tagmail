//! The push dispatcher (contract §5): one task turns arrivals into at most
//! one push per subscription every [`COOLDOWN`], batching what arrives in
//! between into a count. It holds no client while idle; each send builds a
//! pinned one and drops it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{StatusCode, header};
use tagmail_api_core::calendar_dto::REMINDER_GRACE_SECS;
use tagmail_api_core::push_dto::PushPayloadDto;
use tagmail_api_core::{Arrival, ReminderPush};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use url::Url;

use super::store::{self, Target};
use super::{Push, ece, payload};
use crate::outbound::{self, PinError};
use crate::session;

/// A push goes out this long after the first arrival it carries.
pub const BATCH_DELAY: Duration = Duration::from_secs(2);
/// Minimum time between two pushes to one subscription.
pub const COOLDOWN: Duration = Duration::from_secs(30);
pub const MAX_CONCURRENT_SENDS: usize = 4;
/// Per send, from resolving the endpoint to reading the answer.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Response bytes of a refusing push service kept for the debug log.
pub const MAX_LOGGED_BODY: usize = 256;
/// The push service keeps an undelivered message this long.
pub const TTL_SECS: u32 = 12 * 60 * 60;
/// A reminder push that failed (5xx, timeout) is tried once more this
/// much later, if that is still within its grace.
pub const REMINDER_RETRY: Duration = Duration::from_secs(2);
/// Longest honoured `Retry-After` of a push service.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);
const USER_AGENT: &str = "Tagmail-Push";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Delivered,
    /// The subscription expired or was unsubscribed.
    Gone,
    RetryAfter(Duration),
    Failed,
    /// A 5xx answer or no answer: worth one more try.
    Transient,
    /// This server could not send: no fault of the subscription.
    LocalFault,
}

struct Sent {
    id: i64,
    endpoint: String,
    origin: String,
    outcome: Outcome,
    reminder: Option<Reminder>,
}

/// A reminder push, to try again while `deadline` allows.
#[derive(Clone)]
struct Reminder {
    target: Target,
    payload: PushPayloadDto,
    deadline: Instant,
    retried: bool,
}

/// How a push is sent: TTL seconds and Urgency.
#[derive(Clone, Copy)]
struct Delivery {
    ttl: u32,
    urgency: &'static str,
}

const MAIL: Delivery = Delivery {
    ttl: TTL_SECS,
    urgency: "normal",
};

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sent")
            .field("id", &self.id)
            .field("origin", &self.origin)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// Arrivals not yet pushed to one subscription.
struct Pending {
    count: u32,
    newest: Option<Arrival>,
    /// Arrivals were missed, so neither count nor newest are known.
    lagged: bool,
    origin: String,
    since: Instant,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("count", &self.count)
            .field("origin", &self.origin)
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

impl Pending {
    fn payload(&self, preview: bool) -> PushPayloadDto {
        match &self.newest {
            Some(newest) if !self.lagged => payload::new_mail(newest, self.count, preview),
            _ => payload::unknown_new_mail(),
        }
    }
}

fn origin_of(endpoint: &str) -> String {
    Url::parse(endpoint)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default()
}

fn host_of(endpoint: &str) -> String {
    Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

fn from_own(arrival: &Arrival, own: &BTreeSet<String>) -> bool {
    arrival
        .from
        .as_ref()
        .is_some_and(|a| own.contains(&a.email.to_lowercase()))
}

struct Dispatcher {
    push: Arc<Push>,
    permits: Arc<Semaphore>,
    /// Keyed by endpoint: row ids are reused after a delete.
    pending: HashMap<String, Pending>,
    last_sent: HashMap<String, Instant>,
    /// Push service origins that answered 429, until when.
    backoff: HashMap<String, Instant>,
    /// Reminder pushes waiting for a retry or the end of a backoff.
    retries: Vec<(Instant, Reminder)>,
    sends: JoinSet<Sent>,
}

/// Runs the dispatcher until `stop` turns true; in-flight sends are
/// abandoned then.
pub fn spawn(
    push: Arc<Push>,
    arrivals: broadcast::Receiver<Arc<[Arrival]>>,
    reminders: broadcast::Receiver<Arc<ReminderPush>>,
    tests: mpsc::Receiver<i64>,
    stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let dispatcher = Dispatcher {
        push,
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS)),
        pending: HashMap::new(),
        last_sent: HashMap::new(),
        backoff: HashMap::new(),
        retries: Vec::new(),
        sends: JoinSet::new(),
    };
    tokio::spawn(dispatcher.run(arrivals, reminders, tests, stop))
}

impl Dispatcher {
    async fn run(
        mut self,
        mut arrivals: broadcast::Receiver<Arc<[Arrival]>>,
        mut reminders: broadcast::Receiver<Arc<ReminderPush>>,
        mut tests: mpsc::Receiver<i64>,
        mut stop: watch::Receiver<bool>,
    ) {
        while !*stop.borrow() {
            let due = self.next_due();
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                received = arrivals.recv() => match received {
                    Ok(batch) => self.collect(&batch).await,
                    Err(RecvError::Lagged(missed)) => self.lagged(missed).await,
                    Err(RecvError::Closed) => break,
                },
                received = reminders.recv() => match received {
                    Ok(due) => self.send_reminder(&due).await,
                    Err(RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "push dispatcher lagged behind reminders");
                    }
                    Err(RecvError::Closed) => break,
                },
                Some(id) = tests.recv() => self.send_test(id).await,
                Some(done) = self.sends.join_next() => match done {
                    Ok(sent) => self.record(sent).await,
                    Err(e) if e.is_panic() => tracing::error!(error = %e, "push send panicked"),
                    Err(_) => {}
                },
                _ = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.flush().await;
                    self.retry_due();
                }
            }
        }
    }

    /// When the earliest pending push may go out.
    fn next_due(&self) -> Option<Instant> {
        let retries = self.retries.iter().map(|(at, _)| *at);
        self.pending
            .iter()
            .map(|(e, p)| self.due(e, p))
            .chain(retries)
            .min()
    }

    fn due(&self, endpoint: &str, pending: &Pending) -> Instant {
        let mut due = pending.since + BATCH_DELAY;
        if let Some(sent) = self.last_sent.get(endpoint) {
            due = due.max(*sent + COOLDOWN);
        }
        if let Some(until) = self.backoff.get(&pending.origin) {
            due = due.max(*until);
        }
        due
    }

    async fn targets(&self) -> Option<(Vec<Target>, BTreeSet<String>)> {
        match store::targets(&self.push.db, self.push.session_idle).await {
            Ok(found) => Some(found),
            Err(e) => {
                tracing::error!(error = %e, "loading push subscriptions");
                None
            }
        }
    }

    fn pending_for(&mut self, target: &Target) -> &mut Pending {
        self.pending
            .entry(target.endpoint.clone())
            .or_insert_with(|| Pending {
                count: 0,
                newest: None,
                lagged: false,
                origin: origin_of(&target.endpoint),
                since: Instant::now(),
            })
    }

    async fn collect(&mut self, batch: &[Arrival]) {
        let Some((targets, own)) = self.targets().await else {
            return;
        };
        for arrival in batch.iter().filter(|a| !from_own(a, &own)) {
            for target in targets.iter().filter(|t| t.wants(arrival.account_id)) {
                let pending = self.pending_for(target);
                pending.count = pending.count.saturating_add(1);
                if pending
                    .newest
                    .as_ref()
                    .is_none_or(|n| arrival.date >= n.date)
                {
                    pending.newest = Some(arrival.clone());
                }
            }
        }
    }

    /// Batches of arrivals were missed: every subscription gets a generic
    /// push, since whose mail and how much is unknown.
    async fn lagged(&mut self, missed: u64) {
        tracing::warn!(missed, "push dispatcher lagged behind arrivals");
        let Some((targets, _)) = self.targets().await else {
            return;
        };
        for target in &targets {
            self.pending_for(target).lagged = true;
        }
    }

    async fn flush(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(e, p)| self.due(e, p) <= now)
            .map(|(e, _)| e.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        let Some((targets, _)) = self.targets().await else {
            for endpoint in due {
                self.pending.remove(&endpoint);
            }
            return;
        };
        self.last_sent.retain(|_, sent| now < *sent + COOLDOWN);
        for endpoint in due {
            let Some(pending) = self.pending.remove(&endpoint) else {
                continue;
            };
            let Some(target) = targets.iter().find(|t| t.endpoint == endpoint) else {
                continue;
            };
            let payload = pending.payload(target.preview);
            let topic = payload.account_id.map(|a| format!("inbox-{a}"));
            self.last_sent.insert(endpoint, now);
            self.spawn_send(target.clone(), payload, topic, MAIL, None);
        }
    }

    fn retry_due(&mut self) {
        let now = Instant::now();
        let (due, later) = std::mem::take(&mut self.retries)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.retries = later;
        for (_, reminder) in due {
            self.send_reminder_to(reminder);
        }
    }

    /// Sends now, or once the push service's backoff ends if that is
    /// within the grace; dropped otherwise.
    fn send_reminder_to(&mut self, reminder: Reminder) {
        let now = Instant::now();
        let origin = origin_of(&reminder.target.endpoint);
        if let Some(until) = self.backoff.get(&origin).copied().filter(|u| *u > now) {
            if until < reminder.deadline {
                self.retries.push((until, reminder));
            } else {
                tracing::warn!(
                    subscription = reminder.target.id,
                    "reminder dropped: push service backoff"
                );
            }
            return;
        }
        let Some(left) = reminder.deadline.checked_duration_since(now) else {
            return;
        };
        let delivery = Delivery {
            ttl: u32::try_from(left.as_secs()).unwrap_or(u32::MAX).max(1),
            urgency: "high",
        };
        self.spawn_send(
            reminder.target.clone(),
            reminder.payload.clone(),
            None,
            delivery,
            Some(reminder),
        );
    }

    /// Goes out at once, outside the mail cooldown, with a TTL of what is
    /// left of the grace.
    async fn send_reminder(&mut self, due: &ReminderPush) {
        let left = due.fire_at + REMINDER_GRACE_SECS - session::now();
        let Ok(left) = u64::try_from(left) else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(left);
        let Some((targets, _)) = self.targets().await else {
            return;
        };
        for target in targets
            .into_iter()
            .filter(|t| t.reminders && due.sessions.contains(&t.session))
        {
            let payload = payload::reminder(due, target.preview);
            self.send_reminder_to(Reminder {
                target,
                payload,
                deadline,
                retried: false,
            });
        }
    }

    async fn send_test(&mut self, id: i64) {
        match store::target_by_id(&self.push.db, id, self.push.session_idle).await {
            Ok(Some(target)) => self.spawn_send(target, payload::test(), None, MAIL, None),
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "loading push subscription"),
        }
    }

    fn spawn_send(
        &mut self,
        target: Target,
        payload: PushPayloadDto,
        topic: Option<String>,
        delivery: Delivery,
        reminder: Option<Reminder>,
    ) {
        let push = self.push.clone();
        let permits = self.permits.clone();
        self.sends.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let origin = origin_of(&target.endpoint);
            let host = host_of(&target.endpoint);
            let outcome = tokio::time::timeout(
                SEND_TIMEOUT,
                send(&push, &target, &payload, topic.as_deref(), delivery),
            )
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(subscription = target.id, %host, "push send timed out");
                Outcome::Transient
            });
            Sent {
                id: target.id,
                endpoint: target.endpoint,
                origin,
                outcome,
                reminder,
            }
        });
    }

    async fn record(&mut self, sent: Sent) {
        if let Some(mut reminder) = sent.reminder.clone().filter(|r| !r.retried) {
            reminder.retried = true;
            match sent.outcome {
                Outcome::Transient => {
                    let at = Instant::now() + REMINDER_RETRY;
                    if at < reminder.deadline {
                        self.retries.push((at, reminder));
                    }
                }
                Outcome::RetryAfter(wait) => {
                    let at = Instant::now() + wait.min(MAX_BACKOFF);
                    if at < reminder.deadline {
                        self.retries.push((at, reminder));
                    }
                }
                _ => {}
            }
        }
        let db = &self.push.db;
        let result = match sent.outcome {
            Outcome::Delivered => store::record_success(db, sent.id, sent.endpoint).await,
            Outcome::Gone => {
                self.pending.remove(&sent.endpoint);
                store::delete_gone(db, sent.id, sent.endpoint).await
            }
            Outcome::RetryAfter(wait) => {
                self.backoff
                    .insert(sent.origin, Instant::now() + wait.min(MAX_BACKOFF));
                Ok(())
            }
            Outcome::Failed | Outcome::Transient => {
                store::record_failure(db, sent.id, sent.endpoint.clone())
                    .await
                    .map(|dropped| {
                        if dropped {
                            self.pending.remove(&sent.endpoint);
                            tracing::warn!(
                                subscription = sent.id,
                                "dropping push subscription after repeated failures"
                            );
                        }
                    })
            }
            Outcome::LocalFault => Ok(()),
        };
        if let Err(e) = result {
            tracing::error!(subscription = sent.id, error = %e, "recording push result");
        }
        let now = Instant::now();
        self.backoff.retain(|_, until| now < *until);
    }
}

fn retry_after(headers: &header::HeaderMap) -> Duration {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_BACKOFF, Duration::from_secs)
}

async fn send(
    push: &Push,
    target: &Target,
    payload: &PushPayloadDto,
    topic: Option<&str>,
    delivery: Delivery,
) -> Outcome {
    let id = target.id;
    let host = host_of(&target.endpoint);
    let url = match push.policy.validate(&target.endpoint) {
        Ok(url) => url,
        Err(why) => {
            tracing::warn!(subscription = id, %host, why, "push endpoint refused");
            return Outcome::Failed;
        }
    };
    let addr = match push.policy.pin(&url, &*push.resolver).await {
        Ok(addr) => addr,
        Err(e @ PinError::Unresolved(_)) => {
            tracing::warn!(subscription = id, %host, error = ?e, "push endpoint not resolved");
            return Outcome::LocalFault;
        }
        Err(e @ PinError::NoSuchHost(_)) => {
            tracing::warn!(subscription = id, %host, error = ?e, "push endpoint host is gone");
            return Outcome::Failed;
        }
        Err(e) => {
            tracing::warn!(subscription = id, %host, error = ?e, "push endpoint refused");
            return Outcome::Failed;
        }
    };
    let vapid = match push.vapid.get().await {
        Ok(vapid) => vapid,
        Err(e) => {
            tracing::error!(error = %e, "loading the VAPID key");
            return Outcome::LocalFault;
        }
    };
    let body = match payload::encode(payload)
        .and_then(|plain| Ok(ece::encrypt(&target.p256dh, &target.auth, &plain)?))
    {
        Ok(body) => body,
        Err(e) => {
            tracing::error!(subscription = id, error = %e, "encrypting push payload");
            return Outcome::LocalFault;
        }
    };
    let client =
        match outbound::pinned_client(push.tls.clone(), &url, addr, SEND_TIMEOUT, USER_AGENT) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(error = %e, "building push client");
                return Outcome::LocalFault;
            }
        };
    let mut request = client
        .post(url.clone())
        .header("ttl", delivery.ttl.to_string())
        .header("urgency", delivery.urgency)
        .header(header::CONTENT_ENCODING, "aes128gcm")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::AUTHORIZATION,
            vapid.authorization(&url, session::now()),
        )
        .body(body);
    if let Some(topic) = topic {
        request = request.header("topic", topic);
    }
    let mut resp = match request.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(subscription = id, %host, timeout = e.is_timeout(), "push send failed");
            return Outcome::Transient;
        }
    };
    let status = resp.status();
    let wait = retry_after(resp.headers());
    let mut answer = Vec::new();
    while answer.len() < MAX_LOGGED_BODY {
        match resp.chunk().await {
            Ok(Some(chunk)) => answer.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let answer = logged(&answer);
    match status {
        s if s.is_success() => {
            tracing::debug!(subscription = id, %host, status = s.as_u16(), "push delivered");
            Outcome::Delivered
        }
        StatusCode::NOT_FOUND | StatusCode::GONE => {
            tracing::info!(subscription = id, %host, status = status.as_u16(), "push subscription gone");
            Outcome::Gone
        }
        StatusCode::TOO_MANY_REQUESTS => {
            tracing::warn!(subscription = id, %host, retry_after = wait.as_secs(), "push service rate limit");
            Outcome::RetryAfter(wait)
        }
        StatusCode::PAYLOAD_TOO_LARGE => {
            tracing::error!(subscription = id, %host, "push payload too large");
            tracing::debug!(subscription = id, %answer, "push service answer");
            Outcome::Failed
        }
        s => {
            tracing::warn!(subscription = id, %host, status = s.as_u16(), "push refused");
            tracing::debug!(subscription = id, %answer, "push service answer");
            if s.is_server_error() {
                Outcome::Transient
            } else {
                Outcome::Failed
            }
        }
    }
}

/// A push service's answer, capped and escaped so it cannot forge log lines.
fn logged(answer: &[u8]) -> String {
    let capped = &answer[..answer.len().min(MAX_LOGGED_BODY)];
    String::from_utf8_lossy(capped).escape_debug().to_string()
}

#[cfg(test)]
mod tests {
    use tagmail_api_core::dto::AddressDto;

    use super::*;

    #[test]
    fn logged_answers_are_capped_and_escaped() {
        const FORGED: &str = "ok\n2026 INFO forged\u{1b}[31m";
        let hostile = format!("{FORGED}{}", "x".repeat(MAX_LOGGED_BODY * 2));
        let logged = logged(hostile.as_bytes());
        assert!(logged.starts_with("ok\\n2026 INFO forged\\u{1b}[31m"));
        assert!(!logged.contains(['\n', '\u{1b}']));
        assert_eq!(logged.matches('x').count(), MAX_LOGGED_BODY - FORGED.len());
    }

    #[test]
    fn debug_output_hides_endpoints_and_mail() {
        const SECRET: &str = "capability-token";
        let sent = Sent {
            id: 1,
            endpoint: format!("https://push.example/{SECRET}"),
            origin: "https://push.example".into(),
            outcome: Outcome::Delivered,
            reminder: None,
        };
        let pending = Pending {
            count: 1,
            lagged: false,
            newest: Some(Arrival {
                account_id: 1,
                message_id: 1,
                thread_id: None,
                from: Some(AddressDto {
                    name: Some(SECRET.into()),
                    email: format!("{SECRET}@example.org"),
                }),
                subject: Some(SECRET.into()),
                date: 0,
            }),
            origin: "https://push.example".into(),
            since: Instant::now(),
        };
        for debug in [format!("{sent:?}"), format!("{pending:?}")] {
            assert!(!debug.contains(SECRET), "{debug}");
        }
    }
}
