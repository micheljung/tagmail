//! The reminder timer (appointments plan decision (d), §3.7): sleeps until
//! the next alarm, at most [`MAX_SLEEP`], and sends each due alarm once per
//! target: a push for every session with reminders on, a `reminder` change
//! for the paired devices.

use std::sync::Arc;
use std::time::Duration;

use tagmail_api_core::ReminderPush;
use tagmail_api_core::calendar_dto::{REMINDER_GRACE_SECS, ReminderDto};
use tagmail_store::alarms::{self, DueAlarm};
use tagmail_store::{ChangeOp, Entity, NewChange, changes};
use tokio::sync::{Notify, watch};

use crate::error::Result;
use crate::executor::now_secs;
use crate::supervisor::Stores;

/// An alarm reached later than this is not sent any more.
pub const GRACE_SECS: i64 = REMINDER_GRACE_SECS;
pub const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
/// Sent and dismissed records of alarms older than this are dropped.
pub const PRUNE_SECS: i64 = 30 * 24 * 60 * 60;
const SWEEP_SECS: i64 = 24 * 60 * 60;
const SESSION: &str = "session:";
const DEVICE: &str = "device:";

struct Fired {
    alarm: DueAlarm,
    sessions: Vec<String>,
    devices: Vec<i64>,
}

/// Sends the alarms due at `now` (Unix seconds) to the targets they did
/// not reach yet; returns when the next one fires.
pub async fn fire_due(stores: &Stores, now: i64) -> Result<Option<i64>> {
    let due = stores
        .cache
        .read(move |c| alarms::due_alarms(c, now - GRACE_SECS, now))
        .await?;
    if !due.is_empty() {
        let fired = stores
            .config
            .write(move |tx| {
                let mut fired = Vec::new();
                for alarm in due {
                    if alarms::acked(tx, &alarm.key)? {
                        continue;
                    }
                    let mut f = Fired {
                        sessions: Vec::new(),
                        devices: Vec::new(),
                        alarm,
                    };
                    for target in alarms::targets(tx, f.alarm.account)? {
                        if !alarms::mark_sent(tx, &f.alarm, &target, now)? {
                            continue;
                        }
                        if let Some(session) = target.strip_prefix(SESSION) {
                            f.sessions.push(session.to_owned());
                        } else if let Some(Ok(id)) = target.strip_prefix(DEVICE).map(str::parse) {
                            f.devices.push(id);
                        }
                    }
                    fired.push(f);
                }
                Ok(fired)
            })
            .await?;
        publish(stores, fired).await?;
    }
    Ok(stores
        .cache
        .read(move |c| alarms::next_alarm(c, now))
        .await?)
}

async fn publish(stores: &Stores, fired: Vec<Fired>) -> Result<()> {
    let mut log = Vec::new();
    for f in fired {
        if !f.devices.is_empty() {
            let dto = ReminderDto {
                alarm_key: f.alarm.key.clone(),
                account_id: f.alarm.account.0,
                title: f.alarm.title.clone(),
                start: f.alarm.start,
                fire_at: f.alarm.fire_at,
                devices: f.devices,
            };
            log.push(NewChange {
                entity: Entity::Reminder,
                entity_id: 0,
                op: ChangeOp::Upsert,
                payload: Some(serde_json::to_string(&dto).expect("DTOs serialize infallibly")),
            });
        }
        if !f.sessions.is_empty() {
            stores.feed.publish_reminder(ReminderPush {
                alarm_key: f.alarm.key,
                account_id: f.alarm.account.0,
                title: f.alarm.title,
                start: f.alarm.start,
                fire_at: f.alarm.fire_at,
                sessions: f.sessions,
            });
        }
    }
    if !log.is_empty() {
        let epoch = stores.cache.epoch();
        let head = stores
            .cache
            .write(move |tx| {
                changes::append_all(tx, &log)?;
                changes::head(tx, epoch)
            })
            .await?;
        stores.feed.publish(head);
    }
    Ok(())
}

/// Runs until `stop`; `wake` re-arms the timer after a sync.
pub(crate) async fn run(stores: Stores, wake: Arc<Notify>, mut stop: watch::Receiver<bool>) {
    let mut swept = None;
    while !*stop.borrow() {
        let now = now_secs();
        if swept.is_none_or(|at| now - at >= SWEEP_SECS) {
            swept = Some(now);
            let before = now - PRUNE_SECS;
            if let Err(e) = stores
                .config
                .write(move |tx| alarms::prune(tx, before))
                .await
            {
                tracing::warn!(error = %e, "cannot prune reminder records");
            }
        }
        let next = fire_due(&stores, now).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot send reminders");
            None
        });
        let wait = next.map_or(MAX_SLEEP, |at| {
            Duration::from_secs((at - now).max(0).unsigned_abs()).min(MAX_SLEEP)
        });
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}
