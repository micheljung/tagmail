//! AC 21 on the desktop: the reminder the engine sends to this device is
//! shown unless pulled past the grace, and a dismissal elsewhere closes it.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Env, Owner, eventually, now};
use http::Method;
use serde_json::{Value, json};
use tagmail_api_core::desktop_dto::DesktopSettingsPatchDto;
use tagmail_api_core::dto::{ChangeDto, EntityDto};
use tagmail_core::AccountId;
use tagmail_desktop::backend::Backend;
use tagmail_desktop::desktop::Desktop;
use tagmail_desktop::events::{Emit, EventControl};
use tagmail_desktop::notify::{Notification, ReminderAction, reminder_id};
use tagmail_desktop::protocol::{Asset, Assets, Shell};
use tagmail_engine::Stores;
use tagmail_engine::alarms::fire_due;
use tagmail_replica::{Phase, ReplicaEvent};
use tagmail_store::alarms::{self, AlarmRow};
use tagmail_store::calendar::{self, EventRow};
use tokio::sync::broadcast;

const SYNC: Duration = Duration::from_secs(10);
const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\n\
     UID:standup\r\nDTSTAMP:20261101T120000Z\r\nDTSTART:20261201T090000Z\r\n\
     SUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

struct NoAssets;

impl Assets for NoAssets {
    fn get(&self, _path: &str) -> Option<Asset> {
        None
    }
}

struct Quiet;

impl Emit for Quiet {
    fn emit(&self, _event: &str, _payload: Value) {}
}

/// A desktop paired with `owner` and live.
async fn paired(env: &Env, owner: &Owner) -> Arc<Desktop> {
    let desktop = Desktop::open(env.desktop.path(), false, Arc::new(Quiet))
        .await
        .unwrap();
    let (control, _) = EventControl::new();
    let shell = Shell::new(Arc::clone(&desktop), Box::new(NoAssets), control);
    let code = owner.pairing_code().await;
    let body = json!({ "server": owner.url(), "code": code }).to_string();
    let req = http::Request::builder()
        .method(Method::POST)
        .uri("app://localhost/api/desktop/pair")
        .body(body.into_bytes())
        .unwrap();
    assert_eq!(shell.handle(&req).await.status(), 200);
    eventually("the live replica", SYNC, || async {
        desktop
            .replica()
            .is_some_and(|r| matches!(*r.phase().borrow(), Phase::Live))
    })
    .await;
    desktop
}

/// Caches an event with one alarm at `fire_at`, then runs the engine's
/// reminder timer at that time; returns the alarm key.
async fn remind(owner: &Owner, fire_at: i64) -> String {
    let server = owner.server();
    let store = server.store();
    let key = store
        .cache()
        .write(move |tx| {
            let id = calendar::upsert_calendar(tx, AccountId(1), "/cal/", None, None, true)?;
            let row = EventRow {
                href: "/cal/standup.ics".into(),
                uid: "standup".into(),
                ..EventRow::default()
            };
            let event = calendar::put_event(tx, id, &row, ICS.as_bytes(), &[])?;
            let alarm = AlarmRow {
                recurrence_id: None,
                valarm: "0:S-600".into(),
                fire_at,
                start: fire_at + 600,
                title: Some("Standup".into()),
            };
            alarms::replace_alarms(tx, event, &[alarm])?;
            Ok(alarms::due_alarms(tx, fire_at, fire_at)?.remove(0).key)
        })
        .await
        .unwrap();
    let stores = Stores {
        config: store.config().clone(),
        vault: store.vault().clone(),
        cache: store.cache().clone(),
        blobs: store.blobs().clone(),
        feed: server.feed().clone(),
        uploads: PathBuf::new(),
        outbox: PathBuf::new(),
    };
    fire_due(&stores, fire_at).await.unwrap();
    key
}

/// The next pulled changes that hold a reminder.
async fn pulled(events: &mut broadcast::Receiver<ReplicaEvent>) -> Vec<ChangeDto> {
    tokio::time::timeout(SYNC, async {
        loop {
            if let Ok(ReplicaEvent::Writes(writes)) = events.recv().await
                && writes.iter().any(|c| c.entity == EntityDto::Reminder)
            {
                return writes;
            }
        }
    })
    .await
    .expect("no reminder change pulled")
}

#[tokio::test(flavor = "multi_thread")]
async fn reminder_dismiss_closes_across_devices() {
    let env = Env::new();
    let owner = env.start().await;
    let desktop = paired(&env, &owner).await;
    let mut events = desktop.subscribe();

    let key = remind(&owner, now()).await;
    let id = reminder_id(&key).unwrap();
    let due = pulled(&mut events).await;
    assert_eq!(
        desktop.reminders(&due).await,
        [ReminderAction::Show {
            id,
            notification: Notification {
                title: "Standup".into(),
                body: "In 10 minutes".into()
            }
        }]
    );

    assert_eq!(owner.dismiss(&key).await, 204);
    let dismissed = pulled(&mut events).await;
    assert_eq!(
        desktop.reminders(&dismissed).await,
        [ReminderAction::Close { id }]
    );
    assert_eq!(owner.dismiss(&key).await, 204, "dismissing twice is fine");
    assert_eq!(owner.dismiss("0000000000000000").await, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_reminder_past_grace_is_not_shown() {
    let env = Env::new();
    let owner = env.start().await;
    let desktop = paired(&env, &owner).await;
    let mut events = desktop.subscribe();

    remind(&owner, now() - 20 * 60).await;
    let late = pulled(&mut events).await;
    assert!(desktop.reminders(&late).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_off_or_content_hidden_shape_reminders() {
    let env = Env::new();
    let owner = env.start().await;
    let desktop = paired(&env, &owner).await;
    let mut events = desktop.subscribe();

    let key = remind(&owner, now()).await;
    let due = pulled(&mut events).await;
    desktop
        .update_settings(DesktopSettingsPatchDto {
            notify_content: Some(false),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        desktop.reminders(&due).await,
        [ReminderAction::Show {
            id: reminder_id(&key).unwrap(),
            notification: Notification {
                title: "Reminder".into(),
                body: "In 10 minutes".into()
            }
        }]
    );
    desktop
        .update_settings(DesktopSettingsPatchDto {
            notify: Some(false),
            ..Default::default()
        })
        .unwrap();
    assert!(desktop.reminders(&due).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reminders_setting_reaches_the_server() {
    let env = Env::new();
    let owner = env.start().await;
    let desktop = paired(&env, &owner).await;
    let reminders = || async { owner.devices().await[0]["reminders"].clone() };
    assert_eq!(reminders().await, true);

    desktop
        .update_settings(DesktopSettingsPatchDto {
            reminders: Some(false),
            ..Default::default()
        })
        .unwrap();
    eventually("the device's reminders off", SYNC, || async {
        reminders().await == false
    })
    .await;
    let mut events = desktop.subscribe();
    remind(&owner, now()).await;
    tokio::time::timeout(Duration::from_secs(3), pulled(&mut events))
        .await
        .expect_err("no reminder for a device that turned them off");

    for on in [true, false, true, false, true] {
        desktop
            .update_settings(DesktopSettingsPatchDto {
                reminders: Some(on),
                ..Default::default()
            })
            .unwrap();
    }
    eventually("the device's reminders on", SYNC, || async {
        reminders().await == true
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(reminders().await, true, "the newest toggle wins");
}

#[tokio::test(flavor = "multi_thread")]
async fn reminder_settings_are_passed_on_to_the_server() {
    let env = Env::new();
    let owner = env.start().await;
    let desktop = paired(&env, &owner).await;
    let shell = Shell::new(
        Arc::clone(&desktop),
        Box::new(NoAssets),
        EventControl::new().0,
    );
    let req = |method: Method, body: &str| {
        http::Request::builder()
            .method(method)
            .uri("app://localhost/api/reminders/settings")
            .body(body.as_bytes().to_vec())
            .unwrap()
    };
    let set = shell
        .handle(&req(
            Method::PATCH,
            r#"{"timeZone":"Europe/Zurich","ifUnset":true}"#,
        ))
        .await;
    assert_eq!(set.status(), 200);
    let got = shell.handle(&req(Method::GET, "")).await;
    assert_eq!(
        serde_json::from_slice::<Value>(got.body()).unwrap(),
        json!({ "timeZone": "Europe/Zurich" })
    );
}
