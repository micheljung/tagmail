use std::collections::HashMap;
use std::str::FromStr;

use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendarAction, ICalendarComponent, ICalendarComponentType, ICalendarDuration,
    ICalendarParameterName, ICalendarProperty, ICalendarValue,
};
use chrono::{DateTime, NaiveDateTime, TimeZone};
use time::{Duration, OffsetDateTime};

use crate::model::Calendar;
use crate::tz::{Zones, text};
use crate::window::Window;

const DAY_SECS: i64 = 86_400;
/// What an [`Alarm`] of a recurring series' absolute TRIGGER is keyed by.
const MASTER: Option<String> = None;

/// One DISPLAY alarm of an instance; times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    /// `None` for an event that does not recur, and for an absolute
    /// TRIGGER, which fires once for the whole series.
    pub recurrence_id: Option<String>,
    /// The VALARM's UID, else its index in the event and its TRIGGER.
    pub id: String,
    pub fire_at: i64,
    pub start: i64,
    pub title: Option<String>,
}

/// Whether `name` is an IANA time zone, e.g. `Europe/Zurich`.
pub fn is_time_zone(name: &str) -> bool {
    owner_zone(Some(name)).is_some()
}

fn owner_zone(name: Option<&str>) -> Option<Tz> {
    let name = name?;
    let zone = Tz::from_str(name).ok()?;
    (zone.name().as_deref() == Some(name) && !zone.is_floating()).then_some(zone)
}

/// A duration TRIGGER: days and weeks move along the wall clock, the rest
/// is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Offset {
    days: i64,
    secs: i64,
}

impl Offset {
    fn of(d: &ICalendarDuration) -> Offset {
        let sign = if d.neg { -1 } else { 1 };
        Offset {
            days: sign * (i64::from(d.weeks) * 7 + i64::from(d.days)),
            secs: sign
                * (i64::from(d.hours) * 3600 + i64::from(d.minutes) * 60 + i64::from(d.seconds)),
        }
    }

    fn total(self) -> i64 {
        self.days * DAY_SECS + self.secs
    }
}

#[derive(Debug, Clone, Copy)]
enum Trigger {
    Start(Offset),
    End(Offset),
    At(i64),
}

fn trigger(alarm: &ICalendarComponent) -> Option<Trigger> {
    let entry = alarm.property(&ICalendarProperty::Trigger)?;
    match entry.values.first()? {
        ICalendarValue::Duration(d) => {
            let end = entry
                .parameter(&ICalendarParameterName::Related)
                .and_then(|v| v.as_text())
                .is_some_and(|r| r.eq_ignore_ascii_case("END"));
            let offset = Offset::of(d);
            Some(if end {
                Trigger::End(offset)
            } else {
                Trigger::Start(offset)
            })
        }
        ICalendarValue::PartialDateTime(at) => at.to_timestamp().map(Trigger::At),
        _ => None,
    }
}

fn is_display(alarm: &ICalendarComponent) -> bool {
    alarm
        .property(&ICalendarProperty::Action)
        .and_then(|e| e.values.first())
        .is_some_and(|v| match v {
            ICalendarValue::Action(a) => *a == ICalendarAction::Display,
            v => v
                .as_text()
                .is_some_and(|t| t.eq_ignore_ascii_case("DISPLAY")),
        })
}

/// `local` in `zone`; a time a DST gap skips moves past the gap.
fn to_utc(local: NaiveDateTime, zone: Tz) -> Option<i64> {
    zone.from_local_datetime(&local)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(local + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.timestamp())
}

fn naive(utc: i64) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(utc, 0).map(|t| t.naive_utc())
}

/// An instance edge in Unix seconds. Floating and DATE values are wall
/// clock read as UTC by the expansion; they belong to the owner's zone.
fn resolve(t: OffsetDateTime, floating: bool, zone: Option<Tz>) -> i64 {
    let utc = t.unix_timestamp();
    match zone {
        Some(zone) if floating => naive(utc).and_then(|l| to_utc(l, zone)).unwrap_or(utc),
        _ => utc,
    }
}

/// The wall clock a TRIGGER's days move on (RFC 5545 nominal days).
enum Clock<'a> {
    /// A UTC or fixed-offset value.
    Utc,
    /// A floating or all-day value: the owner's zone.
    Owner(Tz),
    /// The event's TZID.
    Event(&'a Zones, &'a str),
}

impl Clock<'_> {
    fn move_days(&self, at: i64, days: i64) -> Option<i64> {
        match self {
            Clock::Utc => None,
            Clock::Owner(zone) => DateTime::from_timestamp(at, 0)
                .map(|t| t.with_timezone(zone).naive_local() + chrono::Duration::days(days))
                .and_then(|local| to_utc(local, *zone)),
            Clock::Event(zones, tzid) => {
                let local = zones.to_local(tzid, naive(at)?)? + chrono::Duration::days(days);
                zones.to_utc(tzid, local).map(|t| t.and_utc().timestamp())
            }
        }
    }
}

/// `at` moved by `offset`: its days on `clock`, so a `-P1D` alarm keeps
/// its time of day across a DST change.
fn shift(at: i64, offset: Offset, clock: &Clock<'_>) -> i64 {
    let day_moved = (offset.days != 0)
        .then(|| clock.move_days(at, offset.days))
        .flatten();
    day_moved.unwrap_or(at + offset.days * DAY_SECS) + offset.secs
}

impl Calendar {
    /// The DISPLAY alarms of the instances in `window` that fire from the
    /// day before its middle (today) on. `zone` is the owner's IANA time
    /// zone, which floating and all-day instances start in; UTC without
    /// one.
    /// The days of a TRIGGER move on the event's TZID, else for floating
    /// and all-day events on the owner's zone, else in UTC.
    pub fn alarms(&self, window: &Window, zone: Option<&str>) -> Vec<Alarm> {
        let zone = owner_zone(zone);
        let per_event: Vec<Vec<(String, Trigger)>> = self
            .components
            .iter()
            .map(|id| {
                let event = &self.raw.components[*id as usize];
                event
                    .component_ids
                    .iter()
                    .map(|c| &self.raw.components[*c as usize])
                    .filter(|c| c.component_type == ICalendarComponentType::VAlarm)
                    .enumerate()
                    .filter(|(_, a)| is_display(a))
                    .filter_map(|(index, a)| {
                        let trigger = trigger(a)?;
                        let id = text(a, &ICalendarProperty::Uid).map_or_else(
                            || match trigger {
                                Trigger::Start(d) => format!("{index}:S{}", d.total()),
                                Trigger::End(d) => format!("{index}:E{}", d.total()),
                                Trigger::At(at) => format!("{index}:@{at}"),
                            },
                            str::to_owned,
                        );
                        Some((id, trigger))
                    })
                    .collect()
            })
            .collect();
        if per_event.iter().all(Vec::is_empty) {
            return Vec::new();
        }
        let from = (window.start + Duration::days(crate::WINDOW_DAYS - 1)).unix_timestamp();
        let expansion = self.expand(window.start..window.end);
        let zones = Zones::new(&self.raw);
        let mut alarms = Vec::new();
        // An absolute TRIGGER fires once; it shows the first instance that
        // starts at or after it, else the last one before.
        let mut absolute: HashMap<(usize, &str), (i64, i64)> = HashMap::new();
        for (o, recurrence_id) in expansion.occurrences.iter().zip(expansion.recurrence_ids) {
            let event_start = &self.events[o.event].start;
            let floating = event_start.floating;
            let start = resolve(o.start, floating, zone);
            let end = resolve(o.end, floating, zone);
            let clock = match (&event_start.tzid, zone) {
                (Some(tzid), _) if !floating => Clock::Event(&zones, tzid),
                (_, Some(zone)) if floating => Clock::Owner(zone),
                _ => Clock::Utc,
            };
            for (id, trigger) in &per_event[o.event] {
                let fire_at = match *trigger {
                    Trigger::Start(d) => shift(start, d, &clock),
                    Trigger::End(d) => shift(end, d, &clock),
                    Trigger::At(at) => {
                        let shown = absolute.entry((o.event, id)).or_insert((at, start));
                        if shown.1 < at {
                            shown.1 = start;
                        }
                        continue;
                    }
                };
                if fire_at >= from {
                    alarms.push(Alarm {
                        recurrence_id: recurrence_id.clone(),
                        id: id.clone(),
                        fire_at,
                        start,
                        title: self.events[o.event].summary.clone(),
                    });
                }
            }
        }
        for ((event, id), (fire_at, start)) in absolute {
            if fire_at >= from {
                alarms.push(Alarm {
                    recurrence_id: MASTER,
                    id: id.to_owned(),
                    fire_at,
                    start,
                    title: self.events[event].summary.clone(),
                });
            }
        }
        alarms.sort_by(|a, b| (a.fire_at, &a.id).cmp(&(b.fire_at, &b.id)));
        alarms
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::parse;

    fn ts(t: OffsetDateTime) -> i64 {
        t.unix_timestamp()
    }

    /// The window around 2027-01-10.
    fn window() -> Window {
        Window::around(time::macros::date!(2027 - 01 - 10))
    }

    fn calendar(body: &str) -> Calendar {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\n{}END:VCALENDAR\r\n",
            body.replace('\n', "\r\n")
        );
        parse(ics.as_bytes()).unwrap()
    }

    fn event(uid: &str, props: &str, alarms: &str) -> String {
        format!(
            "BEGIN:VEVENT\nUID:{uid}\nDTSTAMP:20261110T120000Z\nSUMMARY:{uid}\n{props}{alarms}END:VEVENT\n"
        )
    }

    fn valarm(lines: &str) -> String {
        format!("BEGIN:VALARM\nACTION:DISPLAY\nDESCRIPTION:x\n{lines}END:VALARM\n")
    }

    fn fires(cal: &Calendar, zone: Option<&str>) -> Vec<i64> {
        cal.alarms(&window(), zone)
            .iter()
            .map(|a| a.fire_at)
            .collect()
    }

    #[test]
    fn all_day_alarms_fire_on_the_owner_wall_clock() {
        let trigger = valarm("TRIGGER:-PT15H\n");
        let cal = calendar(&format!(
            "{}{}",
            event("winter", "DTSTART;VALUE=DATE:20270115\n", &trigger),
            event("summer", "DTSTART;VALUE=DATE:20270715\n", &trigger)
        ));
        assert_eq!(
            fires(&cal, Some("Europe/Zurich")),
            [
                ts(datetime!(2027-01-14 08:00 UTC)),
                ts(datetime!(2027-07-14 07:00 UTC))
            ],
            "09:00 local the day before, CET and CEST"
        );
        let starts: Vec<i64> = cal
            .alarms(&window(), Some("Europe/Zurich"))
            .iter()
            .map(|a| a.start)
            .collect();
        assert_eq!(
            starts,
            [
                ts(datetime!(2027-01-14 23:00 UTC)),
                ts(datetime!(2027-07-14 22:00 UTC))
            ]
        );
        assert_eq!(
            fires(&cal, None),
            [
                ts(datetime!(2027-01-14 09:00 UTC)),
                ts(datetime!(2027-07-14 09:00 UTC))
            ],
            "UTC without an owner zone"
        );
    }

    #[test]
    fn floating_times_start_in_the_owner_zone_and_zoned_ones_do_not() {
        let trigger = valarm("TRIGGER:-PT10M\n");
        let cal = calendar(&format!(
            "{}{}",
            event(
                "floating",
                "DTSTART:20270120T090000\nDURATION:PT1H\n",
                &trigger
            ),
            event("utc", "DTSTART:20270121T090000Z\nDURATION:PT1H\n", &trigger)
        ));
        assert_eq!(
            fires(&cal, Some("America/New_York")),
            [
                ts(datetime!(2027-01-20 13:50 UTC)),
                ts(datetime!(2027-01-21 08:50 UTC))
            ]
        );
    }

    #[test]
    fn floating_day_parts_keep_the_owner_time_of_day_across_dst() {
        // Zurich switches to CEST on 2027-03-28.
        let cal = calendar(&event(
            "e",
            "DTSTART:20270328T120000\nDURATION:PT1H\n",
            &valarm("TRIGGER:-P1DT1H\n"),
        ));
        assert_eq!(
            fires(&cal, Some("Europe/Zurich")),
            [ts(datetime!(2027-03-27 10:00 UTC))],
            "12:00 CEST is 12:00 CET the day before, then an hour earlier"
        );
        assert_eq!(fires(&cal, None), [ts(datetime!(2027-03-27 11:00 UTC))]);
    }

    #[test]
    fn zoned_day_parts_move_on_the_event_zone_not_the_owner_one() {
        // New York switches to EDT on 2027-03-14; Zurich only on 03-28.
        let cal = calendar(&event(
            "e",
            "DTSTART;TZID=America/New_York:20270314T090000\nDURATION:PT1H\n",
            &valarm("TRIGGER:-P1D\n"),
        ));
        assert_eq!(
            fires(&cal, Some("Europe/Zurich")),
            [ts(datetime!(2027-03-13 14:00 UTC))],
            "09:00 EDT is 09:00 EST the day before, not 24 hours earlier"
        );
        let utc = calendar(&event(
            "u",
            "DTSTART:20270314T130000Z\nDURATION:PT1H\n",
            &valarm("TRIGGER:-P1D\n"),
        ));
        assert_eq!(
            fires(&utc, Some("Europe/Zurich")),
            [ts(datetime!(2027-03-13 13:00 UTC))],
            "a UTC event's days are 24 hours"
        );
    }

    #[test]
    fn related_end_and_positive_durations() {
        let cal = calendar(&event(
            "e",
            "DTSTART:20270120T090000Z\nDTEND:20270120T100000Z\n",
            &format!(
                "{}{}",
                valarm("TRIGGER;RELATED=END:-PT5M\n"),
                valarm("TRIGGER:PT15M\n")
            ),
        ));
        assert_eq!(
            fires(&cal, None),
            [
                ts(datetime!(2027-01-20 09:15 UTC)),
                ts(datetime!(2027-01-20 09:55 UTC))
            ]
        );
    }

    #[test]
    fn an_absolute_trigger_fires_once_for_a_series() {
        let cal = calendar(&event(
            "s",
            "DTSTART:20270101T090000Z\nDURATION:PT1H\nRRULE:FREQ=WEEKLY\n",
            &valarm("TRIGGER;VALUE=DATE-TIME:20270110T120000Z\n"),
        ));
        let alarms = cal.alarms(&window(), None);
        assert_eq!(
            alarms,
            [Alarm {
                recurrence_id: None,
                id: format!("0:@{}", ts(datetime!(2027-01-10 12:00 UTC))),
                fire_at: ts(datetime!(2027-01-10 12:00 UTC)),
                start: ts(datetime!(2027-01-15 09:00 UTC)),
                title: Some("s".into()),
            }],
            "keyed by the series, showing the next instance"
        );
        let later = cal.alarms(&Window::around(time::macros::date!(2027 - 01 - 11)), None);
        assert_eq!(later, alarms, "the same alarm once the window moves");
    }

    #[test]
    fn an_override_replaces_the_master_alarms() {
        let cal = calendar(&format!(
            "{}{}",
            event(
                "s",
                "DTSTART:20270111T090000Z\nDURATION:PT1H\nRRULE:FREQ=DAILY;COUNT=3\n",
                &valarm("TRIGGER:-PT10M\n"),
            ),
            event(
                "s",
                "RECURRENCE-ID:20270112T090000Z\nDTSTART:20270112T140000Z\nDURATION:PT1H\n",
                &valarm("TRIGGER:-PT30M\n"),
            )
        ));
        let alarms = cal.alarms(&window(), None);
        let got: Vec<(Option<&str>, i64)> = alarms
            .iter()
            .map(|a| (a.recurrence_id.as_deref(), a.fire_at))
            .collect();
        assert_eq!(
            got,
            [
                (
                    Some("20270111T090000Z"),
                    ts(datetime!(2027-01-11 08:50 UTC))
                ),
                (
                    Some("20270112T090000Z"),
                    ts(datetime!(2027-01-12 13:30 UTC))
                ),
                (
                    Some("20270113T090000Z"),
                    ts(datetime!(2027-01-13 08:50 UTC))
                ),
            ]
        );
    }

    #[test]
    fn ids_are_the_uid_else_index_and_trigger() {
        let cal = calendar(&event(
            "e",
            "DTSTART:20270120T090000Z\nDURATION:PT1H\n",
            &format!(
                "{}{}",
                valarm("UID:alarm-1\nTRIGGER:-PT10M\n"),
                valarm("TRIGGER;RELATED=END:-P1D\n")
            ),
        ));
        let mut ids: Vec<String> = cal
            .alarms(&window(), None)
            .into_iter()
            .map(|a| a.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["1:E-86400", "alarm-1"]);
    }

    #[test]
    fn alarms_before_yesterday_and_non_display_ones_are_left_out() {
        let cal = calendar(&format!(
            "{}{}",
            event(
                "old",
                "DTSTART:20270108T235900Z\nDURATION:PT1H\n",
                &valarm("TRIGGER:-PT1M\n")
            ),
            event(
                "audio",
                "DTSTART:20270120T090000Z\nDURATION:PT1H\n",
                "BEGIN:VALARM\nACTION:AUDIO\nTRIGGER:-PT1M\nEND:VALARM\n\
                 BEGIN:VALARM\nACTION:EMAIL\nTRIGGER:-PT1M\nSUMMARY:x\nDESCRIPTION:x\n\
                 ATTENDEE:mailto:a@example.com\nEND:VALARM\n"
            )
        ));
        assert!(fires(&cal, None).is_empty());
        let edge = calendar(&event(
            "edge",
            "DTSTART:20270109T000100Z\nDURATION:PT1H\n",
            &valarm("TRIGGER:-PT1M\n"),
        ));
        assert_eq!(
            fires(&edge, None),
            [ts(datetime!(2027-01-09 00:00 UTC))],
            "the window cutoff is midnight UTC the day before"
        );
    }

    #[test]
    fn owner_zones_are_iana_names() {
        assert!(is_time_zone("Europe/Zurich"));
        assert!(is_time_zone("America/New_York"));
        assert!(!is_time_zone("W. Europe Standard Time"));
        assert!(!is_time_zone("Mars/Olympus"));
        assert!(!is_time_zone(""));
    }
}
