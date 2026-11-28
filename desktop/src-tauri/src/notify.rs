//! What to show for mail the replica pulled: one notification per message,
//! or one summary when many arrive together; and the calendar reminders
//! the server sent to this device.

use tagmail_api_core::calendar_dto::{REMINDER_GRACE_SECS, ReminderDto};
use tagmail_api_core::dto::{AddressDto, ChangeDto, ChangeOpDto, EntityDto};
use tagmail_replica::import::Arrival;

/// From this many messages in one pull on, a single summary is shown.
pub const SUMMARY_AT: usize = 4;
const SUMMARY_SENDERS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

fn sender(arrival: &Arrival) -> Option<String> {
    let from: Vec<AddressDto> = serde_json::from_str(arrival.from_json.as_deref()?).ok()?;
    let first = from.into_iter().next()?;
    Some(
        first
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(first.email),
    )
}

/// `content: false` leaves sender and subject out (a privacy setting).
pub fn plan(arrivals: &[Arrival], content: bool) -> Vec<Notification> {
    if arrivals.is_empty() {
        return Vec::new();
    }
    if arrivals.len() >= SUMMARY_AT {
        let mut senders: Vec<String> = Vec::new();
        for name in arrivals.iter().filter_map(sender) {
            if !senders.contains(&name) {
                senders.push(name);
            }
        }
        let body = if content {
            let more = senders.len().saturating_sub(SUMMARY_SENDERS);
            let mut shown = senders
                .into_iter()
                .take(SUMMARY_SENDERS)
                .collect::<Vec<_>>()
                .join(", ");
            if more > 0 {
                shown.push_str(&format!(" and {more} more"));
            }
            shown
        } else {
            String::new()
        };
        return vec![Notification {
            title: format!("{} new messages", arrivals.len()),
            body,
        }];
    }
    arrivals
        .iter()
        .map(|a| {
            if !content {
                return Notification {
                    title: "New mail".to_owned(),
                    body: String::new(),
                };
            }
            let subject = a
                .subject
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Notification {
                title: sender(a).unwrap_or_else(|| "New mail".to_owned()),
                body: subject.unwrap_or("(no subject)").to_owned(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderAction {
    Show {
        id: i32,
        notification: Notification,
    },
    /// Dismissed somewhere: close the notification shown as `id`.
    Close {
        id: i32,
    },
}

const ID_DIGITS: usize = 7;
const UNTITLED: &str = "Reminder";

/// The OS notification id of a reminder, from its alarm key.
pub fn reminder_id(alarm_key: &str) -> Option<i32> {
    i32::from_str_radix(alarm_key.get(..ID_DIGITS)?, 16).ok()
}

fn starts(start: i64, now: i64) -> String {
    match (start - now + 59) / 60 {
        ..=0 => "Now".to_owned(),
        1 => "In 1 minute".to_owned(),
        minutes => format!("In {minutes} minutes"),
    }
}

/// What the `reminder` changes among `changes` do on `device` at `now`: a
/// due one is shown unless it fired more than the grace ago (pulled after
/// a sleep), a dismissed one is closed. `None` shows none. Without
/// `content` the event's title stays hidden.
pub fn reminders(
    changes: &[ChangeDto],
    device: Option<i64>,
    now: i64,
    content: bool,
) -> Vec<ReminderAction> {
    changes
        .iter()
        .filter(|c| c.entity == EntityDto::Reminder)
        .filter_map(|c| {
            let r: ReminderDto = serde_json::from_value(c.payload.clone()?).ok()?;
            let id = reminder_id(&r.alarm_key)?;
            match c.op {
                ChangeOpDto::Delete => Some(ReminderAction::Close { id }),
                ChangeOpDto::Upsert
                    if device.is_some_and(|d| r.devices.contains(&d))
                        && r.fire_at >= now - REMINDER_GRACE_SECS =>
                {
                    let title = r.title.filter(|t| content && !t.trim().is_empty());
                    Some(ReminderAction::Show {
                        id,
                        notification: Notification {
                            title: title.unwrap_or_else(|| UNTITLED.to_owned()),
                            body: starts(r.start, now),
                        },
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(id: i64, from: &str, subject: Option<&str>) -> Arrival {
        Arrival {
            message_id: id,
            account_id: 1,
            thread_id: Some(id),
            subject: subject.map(str::to_owned),
            from_json: Some(from.to_owned()),
        }
    }

    #[test]
    fn one_notification_per_message_below_the_summary_threshold() {
        let arrivals = [
            arrival(
                1,
                r#"[{"name":"Ann","email":"ann@x.example"}]"#,
                Some("Lunch"),
            ),
            arrival(2, r#"[{"name":null,"email":"bob@x.example"}]"#, None),
            arrival(3, "not json", Some(" ")),
        ];
        assert_eq!(
            plan(&arrivals, true),
            [
                Notification {
                    title: "Ann".into(),
                    body: "Lunch".into()
                },
                Notification {
                    title: "bob@x.example".into(),
                    body: "(no subject)".into()
                },
                Notification {
                    title: "New mail".into(),
                    body: "(no subject)".into()
                },
            ]
        );
        assert!(
            plan(&arrivals, false)
                .iter()
                .all(|n| n.title == "New mail" && n.body.is_empty())
        );
        assert!(plan(&[], true).is_empty());
    }

    #[test]
    fn many_messages_coalesce_into_one_summary() {
        let names = ["Ann", "Bob", "Ann", "Cy", "Dee"];
        let arrivals: Vec<Arrival> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                arrival(
                    i as i64,
                    &format!(r#"[{{"name":"{n}","email":"x@x.example"}}]"#),
                    Some("s"),
                )
            })
            .collect();
        assert_eq!(
            plan(&arrivals[..SUMMARY_AT], true),
            [Notification {
                title: "4 new messages".into(),
                body: "Ann, Bob, Cy".into()
            }]
        );
        assert_eq!(plan(&arrivals, true)[0].body, "Ann, Bob, Cy and 1 more");
        assert_eq!(
            plan(&arrivals, false),
            [Notification {
                title: "5 new messages".into(),
                body: String::new()
            }]
        );
    }
}
