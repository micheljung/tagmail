//! The event editor's transforms (appointments plan §4 M-F): the fields of
//! a stored VEVENT, an edit applied to the parsed tree (so unknown
//! properties, parameters and VALARMs survive), and the REQUEST and CANCEL
//! an organizer sends.

use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarAction, ICalendarComponent, ICalendarComponentType, ICalendarDuration,
    ICalendarEntry, ICalendarMethod, ICalendarParameter, ICalendarParameterName,
    ICalendarParameterValue, ICalendarParticipationRole, ICalendarParticipationStatus,
    ICalendarProperty, ICalendarRelated, ICalendarScheduleAgentValue, ICalendarStatus,
    ICalendarValue, ICalendarValueType,
};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::error::{ItipError, ParseError};
use crate::itip::{
    SCHEDULING_PARAMETERS, add_missing_timezones, copy_tree, dtstamp, index, new_calendar,
    partstat_value, push, root_entries, set_param, set_property,
};
use crate::model::{Calendar, ParticipationStatus};
use crate::parse::{attendee, parse};

const PRODID: &str = "-//tagmail//tagmail//EN";
const VERSION: &str = "2.0";
const ROOT: usize = 0;
const UTC_ZONES: [&str; 3] = ["UTC", "Etc/UTC", "Z"];
const SECONDS_PER_MINUTE: i64 = 60;
const DEFAULT_ALARM_TEXT: &str = "Reminder";

/// A DTSTART or DTEND as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalTime {
    Date(Date),
    /// `zone`: `None` floating, `Some("UTC")` UTC, else a TZID.
    DateTime {
        local: PrimitiveDateTime,
        zone: Option<String>,
    },
}

/// An attendee the editor lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditAttendee {
    pub address: String,
    pub name: Option<String>,
    pub partstat: ParticipationStatus,
}

/// The editable fields of an event's master.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFields {
    pub summary: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub start: LocalTime,
    /// Exclusive for all-day events.
    pub end: LocalTime,
    pub attendees: Vec<EditAttendee>,
    /// Minutes before the start of each reminder relative to it.
    pub alarms: Vec<i64>,
}

/// The organizer a new invitation names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOrganizer {
    pub address: String,
    pub name: Option<String>,
}

/// An edited object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub ics: String,
    /// Something attendees see changed, so SEQUENCE went up.
    pub significant: bool,
    pub sequence: u32,
    /// Attendees the edit removed, for their CANCEL.
    pub removed: Vec<String>,
}

impl LocalTime {
    pub fn is_date(&self) -> bool {
        matches!(self, LocalTime::Date(_))
    }

    fn of(entry: &ICalendarEntry) -> Option<LocalTime> {
        let value = entry.values.first()?.as_partial_date_time()?;
        let date = Date::from_calendar_date(
            i32::from(value.year?),
            Month::try_from(value.month?).ok()?,
            value.day?,
        )
        .ok()?;
        if !value.has_time() {
            return Some(LocalTime::Date(date));
        }
        let time = Time::from_hms(
            value.hour.unwrap_or_default(),
            value.minute.unwrap_or_default(),
            value.second.unwrap_or_default(),
        )
        .ok()?;
        let utc = matches!((value.tz_hour, value.tz_minute), (Some(0), Some(0)));
        Some(LocalTime::DateTime {
            local: PrimitiveDateTime::new(date, time),
            zone: if utc {
                Some(UTC_ZONES[0].to_owned())
            } else {
                entry.tz_id().map(str::to_owned)
            },
        })
    }

    fn entry(&self, name: ICalendarProperty) -> ICalendarEntry {
        let (date, time, zone) = match self {
            LocalTime::Date(date) => (*date, None, None),
            LocalTime::DateTime { local, zone } => (local.date(), Some(local.time()), zone.clone()),
        };
        let utc = zone.as_deref().is_some_and(|z| UTC_ZONES.contains(&z));
        let value = PartialDateTime {
            year: u16::try_from(date.year()).ok(),
            month: Some(u8::from(date.month())),
            day: Some(date.day()),
            hour: time.map(|t| t.hour()),
            minute: time.map(|t| t.minute()),
            second: time.map(|t| t.second()),
            tz_hour: utc.then_some(0),
            tz_minute: utc.then_some(0),
            tz_minus: false,
        };
        let mut entry = ICalendarEntry::new(name).with_value(value);
        match (time, zone) {
            (None, _) => entry.params.push(ICalendarParameter::new(
                ICalendarParameterName::Value,
                ICalendarParameterValue::Value(ICalendarValueType::Date),
            )),
            (Some(_), Some(zone)) if !utc => entry.params.push(ICalendarParameter::new(
                ICalendarParameterName::Tzid,
                ICalendarParameterValue::Text(zone),
            )),
            _ => {}
        }
        entry
    }

    fn plus(&self, seconds: i64) -> LocalTime {
        let shift = time::Duration::seconds(seconds);
        match self {
            LocalTime::Date(date) => LocalTime::Date(
                date.checked_add(time::Duration::days(seconds / 86_400))
                    .unwrap_or(*date),
            ),
            LocalTime::DateTime { local, zone } => LocalTime::DateTime {
                local: local.checked_add(shift).unwrap_or(*local),
                zone: zone.clone(),
            },
        }
    }
}

/// The master (else first) VEVENT's component id.
fn master(calendar: &Calendar) -> Option<u32> {
    calendar
        .events
        .iter()
        .zip(&calendar.components)
        .find(|(e, _)| e.recurrence_id.is_none())
        .or_else(|| calendar.events.iter().zip(&calendar.components).next())
        .map(|(_, id)| *id)
}

fn duration_seconds(entry: &ICalendarEntry) -> Option<i64> {
    match entry.values.first()? {
        ICalendarValue::Duration(d) => Some(d.as_seconds()),
        _ => None,
    }
}

/// Minutes before the start of a VALARM relative to it, if it is one.
fn alarm_minutes(alarm: &ICalendarComponent) -> Option<i64> {
    let trigger = alarm.property(&ICalendarProperty::Trigger)?;
    let related_end = trigger
        .parameter(&ICalendarParameterName::Related)
        .is_some_and(|r| matches!(r, ICalendarParameterValue::Related(ICalendarRelated::End)));
    let seconds = duration_seconds(trigger).filter(|_| !related_end)?;
    (seconds % SECONDS_PER_MINUTE == 0).then_some(-seconds / SECONDS_PER_MINUTE)
}

fn alarms_of<'a>(
    raw: &'a ICalendar,
    event: &'a ICalendarComponent,
) -> impl Iterator<Item = (u32, Option<i64>)> + 'a {
    event
        .component_ids
        .iter()
        .filter(|&&id| raw.components[index(id)].component_type == ICalendarComponentType::VAlarm)
        .map(|&id| (id, alarm_minutes(&raw.components[index(id)])))
}

/// Moves the RRULE's UNTIL by `seconds`, keeping its form.
fn shift_until(component: &mut ICalendarComponent, seconds: i64) {
    for entry in &mut component.entries {
        if entry.name != ICalendarProperty::Rrule {
            continue;
        }
        for value in &mut entry.values {
            let ICalendarValue::RecurrenceRule(rule) = value else {
                continue;
            };
            let Some(until) = rule.until.as_ref() else {
                continue;
            };
            let Some(at) = until.to_timestamp() else {
                continue;
            };
            let at = at + seconds;
            let utc = matches!((until.tz_hour, until.tz_minute), (Some(0), Some(0)));
            rule.until = Some(if !until.has_time() {
                PartialDateTime::from_date_timestamp(at)
            } else if utc {
                PartialDateTime::from_utc_timestamp(at)
            } else {
                PartialDateTime::from_naive_timestamp(at)
            });
        }
    }
}

/// Whether the master repeats until a set end.
pub fn has_until(calendar: &Calendar) -> bool {
    calendar
        .events
        .iter()
        .any(|e| e.recurrence.as_ref().is_some_and(|r| r.until.is_some()))
}

/// What the editor shows of the event: its master's fields.
pub fn fields(calendar: &Calendar) -> Option<EventFields> {
    let id = master(calendar)?;
    let position = calendar.components.iter().position(|c| *c == id)?;
    let event = &calendar.events[position];
    let component = &calendar.raw.components[index(id)];
    let start = LocalTime::of(component.property(&ICalendarProperty::Dtstart)?)?;
    let end = component
        .property(&ICalendarProperty::Dtend)
        .and_then(LocalTime::of)
        .or_else(|| {
            component
                .property(&ICalendarProperty::Duration)
                .and_then(duration_seconds)
                .map(|s| start.plus(s))
        })
        .unwrap_or_else(|| {
            if start.is_date() {
                start.plus(86_400)
            } else {
                start.clone()
            }
        });
    Some(EventFields {
        summary: event.summary.clone(),
        location: event.location.clone(),
        description: event.description.clone(),
        start,
        end,
        attendees: event
            .attendees
            .iter()
            .filter(|a| a.address_is_email)
            .map(|a| EditAttendee {
                address: a.address.clone(),
                name: a.name.clone(),
                partstat: a.partstat.clone(),
            })
            .collect(),
        alarms: alarms_of(&calendar.raw, component)
            .filter_map(|(_, m)| m)
            .collect(),
    })
}

/// Text as the editor compares it: line breaks as `\n`, trimmed, `None`
/// when empty.
fn normal(text: Option<&str>) -> Option<String> {
    let text = text?.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn text_entry(name: ICalendarProperty, value: &str) -> ICalendarEntry {
    ICalendarEntry::new(name).with_value(value.to_owned())
}

fn remove(component: &mut ICalendarComponent, name: &ICalendarProperty) {
    component.entries.retain(|e| &e.name != name);
}

/// Sets a text property, keeping the parameters it had but ALTREP, which
/// described the old value.
fn set_text(component: &mut ICalendarComponent, name: ICalendarProperty, value: Option<&str>) {
    let Some(value) = value else {
        remove(component, &name);
        return;
    };
    let mut entry = text_entry(name, value);
    if let Some(old) = component.property(&entry.name) {
        entry.params = old
            .params
            .iter()
            .filter(|p| p.name != ICalendarParameterName::Altrep)
            .cloned()
            .collect();
    }
    set_property(component, entry);
}

fn mailto(address: &str) -> String {
    format!("mailto:{address}")
}

fn new_attendee(a: &EditAttendee) -> ICalendarEntry {
    let mut entry = ICalendarEntry::new(ICalendarProperty::Attendee).with_value(mailto(&a.address));
    if let Some(name) = &a.name {
        entry.params.push(ICalendarParameter::new(
            ICalendarParameterName::Cn,
            ICalendarParameterValue::Text(name.clone()),
        ));
    }
    entry.params.extend([
        ICalendarParameter::new(
            ICalendarParameterName::Role,
            ICalendarParameterValue::Role(ICalendarParticipationRole::ReqParticipant),
        ),
        ICalendarParameter::new(
            ICalendarParameterName::Partstat,
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::NeedsAction),
        ),
        ICalendarParameter::new(
            ICalendarParameterName::Rsvp,
            ICalendarParameterValue::Bool(true),
        ),
    ]);
    entry
}

fn display_alarm(minutes: i64, summary: Option<&str>) -> ICalendarComponent {
    ICalendarComponent {
        component_type: ICalendarComponentType::VAlarm,
        entries: vec![
            ICalendarEntry::new(ICalendarProperty::Action)
                .with_value(ICalendarValue::Action(ICalendarAction::Display)),
            text_entry(
                ICalendarProperty::Description,
                summary.unwrap_or(DEFAULT_ALARM_TEXT),
            ),
            ICalendarEntry::new(ICalendarProperty::Trigger).with_value(
                ICalendarDuration::from_seconds(-minutes * SECONDS_PER_MINUTE),
            ),
        ],
        component_ids: Vec::new(),
    }
}

fn sequence_of(component: &ICalendarComponent) -> u32 {
    component
        .property(&ICalendarProperty::Sequence)
        .and_then(|e| e.values.first())
        .and_then(ICalendarValue::as_integer)
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or_default()
}

fn new_object(uid: &str) -> ICalendar {
    let mut raw = new_calendar(vec![
        text_entry(ICalendarProperty::Prodid, PRODID),
        text_entry(ICalendarProperty::Version, VERSION),
    ]);
    let event = push(
        &mut raw,
        ICalendarComponent {
            component_type: ICalendarComponentType::VEvent,
            entries: vec![
                text_entry(ICalendarProperty::Uid, uid),
                ICalendarEntry::new(ICalendarProperty::Sequence).with_value(0i64),
            ],
            component_ids: Vec::new(),
        },
    );
    raw.components[ROOT].component_ids.push(event);
    raw
}

/// Applies `edit` to the master of `stored` (a new object for `uid` when
/// `None`), changing only what differs. Anything attendees see changing
/// raises SEQUENCE and DTSTAMP; reminders are mine alone. `organizer` is
/// written when attendees are added to an event without one, and
/// `schedule_agent_client` marks attendees for client scheduling (§3.3).
pub fn edit(
    stored: Option<&[u8]>,
    uid: &str,
    edit: &EventFields,
    organizer: Option<&EditOrganizer>,
    schedule_agent_client: Option<bool>,
    bump: bool,
    now: OffsetDateTime,
) -> Result<Edited, ItipError> {
    let (mut raw, id, before, old_start) = match stored {
        Some(ics) => {
            let calendar = parse(ics)?;
            let id = master(&calendar).ok_or(ParseError::NoEvents)?;
            let before = fields(&calendar);
            let start = calendar
                .events
                .iter()
                .zip(&calendar.components)
                .find(|(_, c)| **c == id)
                .map(|(e, _)| e.start.instant);
            (calendar.raw, id, before, start)
        }
        None => (new_object(uid), 1, None, None),
    };
    let moves = before.as_ref().is_some_and(|b| b.start != edit.start);
    let summary = edit.summary.as_deref();
    let mut significant = before.is_none();
    let mut removed = Vec::new();
    {
        let component = &mut raw.components[index(id)];
        let differs = |pick: fn(&EventFields) -> Option<&str>| {
            before
                .as_ref()
                .is_none_or(|b| normal(pick(b)) != normal(pick(edit)))
        };
        for (name, pick) in [
            (
                ICalendarProperty::Summary,
                (|f: &EventFields| f.summary.as_deref()) as fn(&EventFields) -> Option<&str>,
            ),
            (ICalendarProperty::Location, |f| f.location.as_deref()),
            (ICalendarProperty::Description, |f| f.description.as_deref()),
        ] {
            if differs(pick) {
                significant = true;
                set_text(component, name, pick(edit));
            }
        }
        if before
            .as_ref()
            .is_none_or(|b| b.start != edit.start || b.end != edit.end)
        {
            significant = true;
            set_property(component, edit.start.entry(ICalendarProperty::Dtstart));
            remove(component, &ICalendarProperty::Duration);
            set_property(component, edit.end.entry(ICalendarProperty::Dtend));
        }

        let kept: Vec<&EditAttendee> = edit.attendees.iter().collect();
        let mut present = Vec::new();
        component.entries.retain(|entry| {
            if entry.name != ICalendarProperty::Attendee {
                return true;
            }
            let Some(a) = attendee(entry).filter(|a| a.address_is_email) else {
                return true;
            };
            let keep = kept.iter().any(|k| a.is_address(&k.address));
            if keep {
                present.push(a.address.to_ascii_lowercase());
            } else {
                removed.push(a.address);
            }
            keep
        });
        let added: Vec<ICalendarEntry> = kept
            .iter()
            .filter(|k| !present.contains(&k.address.to_ascii_lowercase()))
            .map(|k| new_attendee(k))
            .collect();
        significant |= !removed.is_empty() || !added.is_empty();
        component.entries.extend(added);
        if !edit.attendees.is_empty()
            && component.property(&ICalendarProperty::Organizer).is_none()
            && let Some(organizer) = organizer
        {
            let mut entry = ICalendarEntry::new(ICalendarProperty::Organizer)
                .with_value(mailto(&organizer.address));
            if let Some(name) = &organizer.name {
                entry.params.push(ICalendarParameter::new(
                    ICalendarParameterName::Cn,
                    ICalendarParameterValue::Text(name.clone()),
                ));
            }
            component.entries.push(entry);
        }
        for entry in &mut component.entries {
            match schedule_agent_client {
                _ if entry.name != ICalendarProperty::Attendee => {}
                Some(true) => set_param(
                    entry,
                    ICalendarParameterName::ScheduleAgent,
                    ICalendarParameterValue::ScheduleAgent(ICalendarScheduleAgentValue::Client),
                ),
                Some(false) => entry
                    .params
                    .retain(|p| p.name != ICalendarParameterName::ScheduleAgent),
                None => {}
            }
        }
    }

    let current: Vec<(u32, Option<i64>)> = alarms_of(&raw, &raw.components[index(id)]).collect();
    let mut wanted = edit.alarms.clone();
    let mut children = Vec::new();
    for &child in &raw.components[index(id)].component_ids {
        match current.iter().find(|(c, _)| *c == child) {
            Some((_, Some(minutes))) => {
                if let Some(at) = wanted.iter().position(|w| w == minutes) {
                    wanted.remove(at);
                    children.push(child);
                }
            }
            _ => children.push(child),
        }
    }
    for minutes in wanted {
        children.push(push(&mut raw, display_alarm(minutes, summary)));
    }
    raw.components[index(id)].component_ids = children;

    let component = &mut raw.components[index(id)];
    let mut sequence = sequence_of(component);
    if significant && bump {
        if stored.is_some() {
            sequence += 1;
            set_property(
                component,
                ICalendarEntry::new(ICalendarProperty::Sequence).with_value(i64::from(sequence)),
            );
        }
        set_property(component, dtstamp(now));
    }
    raw.add_missing_timezones();
    if let Some(old) = old_start.filter(|_| moves) {
        let text = raw.to_string();
        let new = parse(text.as_bytes()).ok().and_then(|c| {
            master(&c).and_then(|m| {
                c.components
                    .iter()
                    .position(|x| *x == m)
                    .map(|i| c.events[i].start.instant)
            })
        });
        if let Some(new) = new {
            shift_until(&mut raw.components[index(id)], (new - old).whole_seconds());
        }
    }
    Ok(Edited {
        ics: raw.to_string(),
        significant,
        sequence,
        removed,
    })
}

/// A copy of `stored` for iMIP under `method`: no VALARMs, no CalDAV
/// scheduling parameters, and the VTIMEZONEs it references.
fn imip(
    stored: &Calendar,
    method: ICalendarMethod,
    mut each: impl FnMut(&mut ICalendarComponent),
) -> String {
    let mut root: Vec<ICalendarEntry> = root_entries(&stored.raw)
        .filter(|e| e.name != ICalendarProperty::Prodid)
        .cloned()
        .collect();
    root.insert(0, text_entry(ICalendarProperty::Prodid, PRODID));
    root.push(
        ICalendarEntry::new(ICalendarProperty::Method).with_value(ICalendarValue::Method(method)),
    );
    let mut out = new_calendar(root);
    for &id in &stored.components {
        let source = &stored.raw.components[index(id)];
        let children: Vec<u32> = source
            .component_ids
            .iter()
            .filter(|&&c| {
                stored.raw.components[index(c)].component_type != ICalendarComponentType::VAlarm
            })
            .map(|&c| copy_tree(&mut out, &stored.raw, c))
            .collect();
        let mut component = ICalendarComponent {
            component_type: ICalendarComponentType::VEvent,
            entries: source.entries.clone(),
            component_ids: children,
        };
        for entry in &mut component.entries {
            entry
                .params
                .retain(|p| !SCHEDULING_PARAMETERS.contains(&p.name));
        }
        each(&mut component);
        let copy = push(&mut out, component);
        out.components[ROOT].component_ids.push(copy);
    }
    add_missing_timezones(&mut out, &[&stored.raw]);
    out.to_string()
}

/// The METHOD:REQUEST an organizer sends for the stored object.
pub fn request(stored_ics: &[u8]) -> Result<String, ItipError> {
    Ok(imip(&parse(stored_ics)?, ICalendarMethod::Request, |_| {}))
}

/// The METHOD:CANCEL of the stored object, to `only` these attendees
/// when given (removed from the event), else to all, with SEQUENCE
/// `sequence` and a DTSTAMP of `now`.
pub fn cancel(
    stored_ics: &[u8],
    only: Option<&[String]>,
    sequence: u32,
    now: OffsetDateTime,
) -> Result<String, ItipError> {
    let stored = parse(stored_ics)?;
    let master_id = master(&stored);
    let mut stored = stored;
    let keep: Vec<usize> = stored
        .components
        .iter()
        .enumerate()
        .filter(|(_, id)| Some(**id) == master_id)
        .map(|(i, _)| i)
        .collect();
    stored.components = keep.iter().map(|&i| stored.components[i]).collect();
    Ok(imip(&stored, ICalendarMethod::Cancel, |component| {
        if let Some(only) = only {
            component.entries.retain(|e| {
                e.name != ICalendarProperty::Attendee
                    || attendee(e).is_some_and(|a| only.iter().any(|o| a.is_address(o)))
            });
            for address in only {
                if !component
                    .properties(&ICalendarProperty::Attendee)
                    .any(|e| attendee(e).is_some_and(|a| a.is_address(address)))
                {
                    component.entries.push(
                        ICalendarEntry::new(ICalendarProperty::Attendee)
                            .with_value(mailto(address)),
                    );
                }
            }
        }
        set_property(
            component,
            ICalendarEntry::new(ICalendarProperty::Status)
                .with_value(ICalendarValue::Status(ICalendarStatus::Cancelled)),
        );
        set_property(
            component,
            ICalendarEntry::new(ICalendarProperty::Sequence).with_value(i64::from(sequence)),
        );
        set_property(component, dtstamp(now));
    }))
}

/// The stored component of the instance at `recurrence_id`: its override,
/// else the master.
/// The stored component for the instance at `recurrence_id`, whether it is
/// that instance's own override, and whether the instance is certain (an
/// expansion cut short cannot tell).
fn instance_of(
    stored: &Calendar,
    recurrence_id: Option<OffsetDateTime>,
) -> Option<(u32, bool, bool)> {
    let id = recurrence_id.and_then(|rid| {
        stored
            .events
            .iter()
            .zip(&stored.components)
            .find(|(e, _)| e.recurrence_id.as_ref().is_some_and(|r| r.instant == rid))
            .map(|(_, id)| *id)
    });
    match (id, recurrence_id) {
        (Some(id), _) => Some((id, true, true)),
        (None, Some(rid)) => {
            let next = rid.checked_add(time::Duration::SECOND)?;
            let expansion = stored.expand(rid..next);
            let instance = expansion.occurrences.iter().any(|o| o.start == rid);
            (instance || expansion.truncated)
                .then(|| master(stored).map(|id| (id, false, instance)))
                .flatten()
        }
        (None, None) => master(stored).map(|id| (id, false, true)),
    }
}

/// Master properties an override of one instance does not take.
const SERIES_TIMES: [ICalendarProperty; 8] = [
    ICalendarProperty::Rrule,
    ICalendarProperty::Rdate,
    ICalendarProperty::Exdate,
    ICalendarProperty::Exrule,
    ICalendarProperty::Dtstart,
    ICalendarProperty::Dtend,
    ICalendarProperty::Duration,
    ICalendarProperty::RecurrenceId,
];

/// An override of the master `id` for the instance at `recurrence_id`
/// (the REPLY's RECURRENCE-ID entry): the master's properties, starting
/// there and lasting as long as the master.
fn reply_override(
    out: &mut ICalendar,
    stored: &Calendar,
    id: u32,
    recurrence_id: &ICalendarEntry,
) -> ICalendarComponent {
    let master = &stored.raw.components[index(id)];
    let mut entries: Vec<ICalendarEntry> = master
        .entries
        .iter()
        .filter(|e| !SERIES_TIMES.contains(&e.name))
        .cloned()
        .collect();
    let mut rid = recurrence_id.clone();
    rid.params
        .retain(|p| p.name != ICalendarParameterName::Range);
    let mut start = rid.clone();
    start.name = ICalendarProperty::Dtstart;
    entries.push(rid);
    entries.push(start);
    let length = stored
        .components
        .iter()
        .position(|c| *c == id)
        .map(|i| &stored.events[i])
        .and_then(|e| Some((e.end.as_ref()?.instant - e.start.instant).whole_seconds()));
    if let Some(seconds) = length {
        entries.push(
            ICalendarEntry::new(ICalendarProperty::Duration)
                .with_value(ICalendarDuration::from_seconds(seconds)),
        );
    }
    let component_ids = master
        .component_ids
        .iter()
        .filter(|&&c| {
            stored.raw.components[index(c)].component_type == ICalendarComponentType::VAlarm
        })
        .map(|&c| copy_tree(out, &stored.raw, c))
        .collect();
    ICalendarComponent {
        component_type: ICalendarComponentType::VEvent,
        entries,
        component_ids,
    }
}

fn partstat_in(component: &ICalendarComponent, address: &str) -> Option<ParticipationStatus> {
    component
        .properties(&ICalendarProperty::Attendee)
        .filter_map(attendee)
        .find(|a| a.is_address(address))
        .map(|a| a.partstat)
}

/// `address`'s PARTSTAT for the instance at `recurrence_id` (the series
/// for `None`) in the stored object: its override's, else the master's;
/// and whether that instance is known to exist (else only an explicit
/// apply should write it).
pub fn attendee_partstat(
    stored_ics: &[u8],
    address: &str,
    recurrence_id: Option<OffsetDateTime>,
) -> Option<(ParticipationStatus, bool)> {
    let stored = parse(stored_ics).ok()?;
    let (id, _, certain) = instance_of(&stored, recurrence_id)?;
    Some((
        partstat_in(&stored.raw.components[index(id)], address)?,
        certain,
    ))
}

/// The stored object with `address`'s PARTSTAT as its REPLY gives it, on
/// the instance the REPLY names: its override, one made from the master
/// when there is none, or the master for the series. `None` when it is
/// stored already.
pub fn apply_reply(
    stored_ics: &[u8],
    reply_ics: &[u8],
    address: &str,
) -> Result<Option<String>, ItipError> {
    let stored = parse(stored_ics)?;
    let reply = parse(reply_ics)?;
    let not_attendee = || ItipError::NotAttendee {
        address: address.to_owned(),
    };
    let (answer, recurrence_id, reply_id) = reply
        .events
        .iter()
        .zip(&reply.components)
        .find_map(|(e, id)| {
            let a = e.attendees.iter().find(|a| a.is_address(address))?;
            Some((
                a.partstat.clone(),
                e.recurrence_id.as_ref().map(|r| r.instant),
                *id,
            ))
        })
        .ok_or_else(not_attendee)?;
    let (id, exact) = instance_of(&stored, recurrence_id)
        .map(|(id, exact, _)| (id, exact))
        .ok_or_else(|| ItipError::EventNotFound {
            uid: stored.events[0].uid.clone(),
        })?;
    let held = partstat_in(&stored.raw.components[index(id)], address).ok_or_else(not_attendee)?;
    if held == answer {
        return Ok(None);
    }
    let mut raw = stored.raw.clone();
    let target = if recurrence_id.is_some() && !exact {
        let rid = reply.raw.components[index(reply_id)]
            .property(&ICalendarProperty::RecurrenceId)
            .ok_or(ParseError::NoEvents)?;
        let component = reply_override(&mut raw, &stored, id, rid);
        let id = push(&mut raw, component);
        raw.components[ROOT].component_ids.push(id);
        id
    } else {
        id
    };
    for entry in &mut raw.components[index(target)].entries {
        if entry.name == ICalendarProperty::Attendee
            && attendee(entry).is_some_and(|a| a.is_address(address))
        {
            set_param(
                entry,
                ICalendarParameterName::Partstat,
                partstat_value(&answer),
            );
        }
    }
    add_missing_timezones(&mut raw, &[&reply.raw]);
    Ok(Some(raw.to_string()))
}

/// A repeating master with exceptions (EXDATE or overrides) whose times
/// an edit must not move until they can be moved with it.
pub fn has_exceptions(calendar: &Calendar) -> bool {
    calendar.events.iter().any(|e| e.recurrence_id.is_some())
        || calendar
            .events
            .iter()
            .filter_map(|e| e.recurrence.as_ref())
            .any(|r| !r.exdates.is_empty())
}

/// Whether `tzid` names a zone a VTIMEZONE can be generated for.
pub fn is_known_zone(tzid: &str) -> bool {
    matches!(
        tzid.parse::<calcard::common::timezone::Tz>(),
        Ok(calcard::common::timezone::Tz::Tz(_))
    )
}
