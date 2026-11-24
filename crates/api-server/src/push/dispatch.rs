//! The push dispatcher (contract §5): one task turns arrivals into at most
//! one push per subscription every [`COOLDOWN`], batching what arrives in
//! between into a count. It holds no client while idle; each send builds a
//! pinned one and drops it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{StatusCode, header};
use tagmail_api_core::Arrival;
use tagmail_api_core::push_dto::PushPayloadDto;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use url::Url;

use super::store::{self, Target};
use super::{Push, ece, payload};
use crate::outbound;
use crate::session;

/// A push goes out this long after the first arrival it carries.
pub const BATCH_DELAY: Duration = Duration::from_secs(2);
/// Minimum time between two pushes to one subscription.
pub const COOLDOWN: Duration = Duration::from_secs(30);
pub const MAX_CONCURRENT_SENDS: usize = 4;
/// Per send, from resolving the endpoint to reading the answer.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Response bytes read for the log.
pub const MAX_LOGGED_BODY: usize = 4096;
/// The push service keeps an undelivered message this long.
pub const TTL_SECS: u32 = 12 * 60 * 60;
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
}

#[derive(Debug)]
struct Sent {
    id: i64,
    endpoint: String,
    origin: String,
    outcome: Outcome,
}

/// Arrivals not yet pushed to one subscription.
#[derive(Debug)]
struct Pending {
    count: u32,
    newest: Option<Arrival>,
    origin: String,
    since: Instant,
}

impl Pending {
    fn payload(&self, preview: bool) -> PushPayloadDto {
        match &self.newest {
            Some(newest) => payload::new_mail(newest, self.count, preview),
            None => payload::count_only(None, self.count),
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
    sends: JoinSet<Sent>,
}

/// Runs the dispatcher until `stop` turns true; in-flight sends are
/// abandoned then.
pub fn spawn(
    push: Arc<Push>,
    arrivals: broadcast::Receiver<Arc<[Arrival]>>,
    tests: mpsc::Receiver<i64>,
    stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let dispatcher = Dispatcher {
        push,
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS)),
        pending: HashMap::new(),
        last_sent: HashMap::new(),
        backoff: HashMap::new(),
        sends: JoinSet::new(),
    };
    tokio::spawn(dispatcher.run(arrivals, tests, stop))
}

impl Dispatcher {
    async fn run(
        mut self,
        mut arrivals: broadcast::Receiver<Arc<[Arrival]>>,
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
                Some(id) = tests.recv() => self.send_test(id).await,
                Some(done) = self.sends.join_next() => match done {
                    Ok(sent) => self.record(sent).await,
                    Err(e) if e.is_panic() => tracing::error!(error = %e, "push send panicked"),
                    Err(_) => {}
                },
                _ = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.flush().await;
                }
            }
        }
    }

    /// When the earliest pending push may go out.
    fn next_due(&self) -> Option<Instant> {
        self.pending.iter().map(|(e, p)| self.due(e, p)).min()
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
        match store::targets(&self.push.db).await {
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

    /// Arrivals were missed: every subscription gets a count-only push.
    async fn lagged(&mut self, missed: u64) {
        tracing::warn!(missed, "push dispatcher lagged behind arrivals");
        let Some((targets, _)) = self.targets().await else {
            return;
        };
        let missed = u32::try_from(missed).unwrap_or(u32::MAX);
        for target in &targets {
            let pending = self.pending_for(target);
            pending.count = pending.count.saturating_add(missed);
            pending.newest = None;
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
            self.spawn_send(target.clone(), payload, topic);
        }
    }

    async fn send_test(&mut self, id: i64) {
        match store::target_by_id(&self.push.db, id).await {
            Ok(Some(target)) => self.spawn_send(target, payload::test(), None),
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "loading push subscription"),
        }
    }

    fn spawn_send(&mut self, target: Target, payload: PushPayloadDto, topic: Option<String>) {
        let push = self.push.clone();
        let permits = self.permits.clone();
        self.sends.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let origin = origin_of(&target.endpoint);
            let host = host_of(&target.endpoint);
            let outcome = tokio::time::timeout(
                SEND_TIMEOUT,
                send(&push, &target, &payload, topic.as_deref()),
            )
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(subscription = target.id, %host, "push send timed out");
                Outcome::Failed
            });
            Sent {
                id: target.id,
                endpoint: target.endpoint,
                origin,
                outcome,
            }
        });
    }

    async fn record(&mut self, sent: Sent) {
        let db = &self.push.db;
        let result = match sent.outcome {
            Outcome::Delivered => store::record_success(db, sent.id).await,
            Outcome::Gone => {
                self.pending.remove(&sent.endpoint);
                store::delete(db, sent.id).await.map(|_| ())
            }
            Outcome::RetryAfter(wait) => {
                self.backoff
                    .insert(sent.origin, Instant::now() + wait.min(MAX_BACKOFF));
                Ok(())
            }
            Outcome::Failed => store::record_failure(db, sent.id).await.map(|dropped| {
                if dropped {
                    self.pending.remove(&sent.endpoint);
                    tracing::warn!(
                        subscription = sent.id,
                        "dropping push subscription after repeated failures"
                    );
                }
            }),
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
        Err(e) => {
            tracing::warn!(subscription = id, %host, error = ?e, "push endpoint not reachable");
            return Outcome::Failed;
        }
    };
    let vapid = match push.vapid.get().await {
        Ok(vapid) => vapid,
        Err(e) => {
            tracing::error!(error = %e, "loading the VAPID key");
            return Outcome::Failed;
        }
    };
    let body = match payload::encode(payload)
        .and_then(|plain| Ok(ece::encrypt(&target.p256dh, &target.auth, &plain)?))
    {
        Ok(body) => body,
        Err(e) => {
            tracing::error!(subscription = id, error = %e, "encrypting push payload");
            return Outcome::Failed;
        }
    };
    let client =
        match outbound::pinned_client(push.tls.clone(), &url, addr, SEND_TIMEOUT, USER_AGENT) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(error = %e, "building push client");
                return Outcome::Failed;
            }
        };
    let mut request = client
        .post(url.clone())
        .header("ttl", TTL_SECS.to_string())
        .header("urgency", "normal")
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
            return Outcome::Failed;
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
    answer.truncate(MAX_LOGGED_BODY);
    let answer = String::from_utf8_lossy(&answer);
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
            tracing::error!(subscription = id, %host, %answer, "push payload too large");
            Outcome::Failed
        }
        s => {
            tracing::warn!(subscription = id, %host, status = s.as_u16(), %answer, "push refused");
            Outcome::Failed
        }
    }
}
