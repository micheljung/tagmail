//! iTIP (RFC 5546) and CalDAV object transforms: the REPLY body, PARTSTAT
//! and SCHEDULE-AGENT edits, applying updates and cancels to a stored copy,
//! and splitting `.ics` files for import. Every transform edits the calcard
//! tree, so unknown properties, parameters and components survive.

use std::collections::{HashMap, HashSet};

use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarDuration, ICalendarEntry,
    ICalendarMethod, ICalendarParameter, ICalendarParameterName, ICalendarParameterValue,
    ICalendarParticipationStatus, ICalendarProperty, ICalendarScheduleAgentValue, ICalendarStatus,
    ICalendarValue,
};
use time::OffsetDateTime;

use crate::error::ItipError;
use crate::model::{Calendar, EventTime, ParticipationStatus};
use crate::parse::{attendee, organizer, parse};
use crate::tz::text;

const PRODID: &str = "-//tagmail//tagmail//EN";
const ROOT: usize = 0;

/// Parameters only a CalDAV server acts on; they never go out in iMIP.
pub(crate) const SCHEDULING_PARAMETERS: [ICalendarParameterName; 3] = [
    ICalendarParameterName::ScheduleAgent,
    ICalendarParameterName::ScheduleForceSend,
    ICalendarParameterName::ScheduleStatus,
];

/// Properties the owner sets on their own copy rather than the organizer.
const LOCAL_PROPERTIES: [ICalendarProperty; 2] =
    [ICalendarProperty::Categories, ICalendarProperty::Color];

/// Properties a CANCEL sets on the one instance it names.
const INSTANCE_PROPERTIES: [ICalendarProperty; 6] = [
    ICalendarProperty::RecurrenceId,
    ICalendarProperty::Dtstart,
    ICalendarProperty::Dtend,
    ICalendarProperty::Duration,
    ICalendarProperty::Dtstamp,
    ICalendarProperty::Sequence,
];

/// Properties that describe the series, so a new override never inherits them.
const SERIES_PROPERTIES: [ICalendarProperty; 9] = [
    ICalendarProperty::Rrule,
    ICalendarProperty::Rdate,
    ICalendarProperty::Exdate,
    ICalendarProperty::Exrule,
    ICalendarProperty::Dtstart,
    ICalendarProperty::Dtend,
    ICalendarProperty::Duration,
    ICalendarProperty::RecurrenceId,
    ICalendarProperty::Status,
];

/// The METHOD:REPLY object for my answer to the VEVENT `uid` /
/// `recurrence_id` of `calendar`: that VEVENT with only my ATTENDEE (new
/// PARTSTAT, no RSVP), a DTSTAMP of `now`, no VALARMs and no CalDAV
/// scheduling parameters, plus the VTIMEZONEs it references.
pub fn reply(
    calendar: &Calendar,
    uid: &str,
    recurrence_id: Option<&EventTime>,
    me: &str,
    partstat: &ParticipationStatus,
    now: OffsetDateTime,
) -> Result<String, ItipError> {
    let key = recurrence_id.map(|time| time.instant);
    let id = calendar
        .events
        .iter()
        .zip(&calendar.components)
        .find(|(event, _)| event.uid == uid && instance(event.recurrence_id.as_ref()) == key)
        .map(|(_, id)| *id)
        .ok_or_else(|| ItipError::EventNotFound {
            uid: uid.to_string(),
        })?;
    let source = &calendar.raw;
    let mut found = false;
    let mut entries = Vec::new();
    for entry in &source.components[index(id)].entries {
        match entry.name {
            ICalendarProperty::Attendee if is_me(entry, me) => {
                found = true;
                let mut entry = entry.clone();
                set_param(
                    &mut entry,
                    ICalendarParameterName::Partstat,
                    partstat_value(partstat),
                );
                entry.params.retain(|param| {
                    param.name != ICalendarParameterName::Rsvp
                        && !SCHEDULING_PARAMETERS.contains(&param.name)
                });
                entries.push(entry);
            }
            ICalendarProperty::Attendee => {}
            ICalendarProperty::Organizer => {
                let mut entry = entry.clone();
                entry
                    .params
                    .retain(|param| !SCHEDULING_PARAMETERS.contains(&param.name));
                entries.push(entry);
            }
            ICalendarProperty::Dtstamp => entries.push(dtstamp(now)),
            _ => entries.push(entry.clone()),
        }
    }
    if !found {
        return Err(ItipError::NotAttendee {
            address: me.to_string(),
        });
    }
    if !entries
        .iter()
        .any(|entry| entry.name == ICalendarProperty::Dtstamp)
    {
        entries.push(dtstamp(now));
    }

    let mut root: Vec<ICalendarEntry> = root_entries(source)
        .filter(|entry| entry.name != ICalendarProperty::Prodid)
        .cloned()
        .collect();
    root.insert(
        0,
        ICalendarEntry::new(ICalendarProperty::Prodid).with_value(PRODID.to_string()),
    );
    root.push(
        ICalendarEntry::new(ICalendarProperty::Method)
            .with_value(ICalendarValue::Method(ICalendarMethod::Reply)),
    );
    let mut out = new_calendar(root);
    let event = push(
        &mut out,
        ICalendarComponent {
            component_type: ICalendarComponentType::VEvent,
            entries,
            component_ids: Vec::new(),
        },
    );
    out.components[ROOT].component_ids.push(event);
    add_missing_timezones(&mut out, &[source]);
    Ok(out.to_string())
}

/// Sets PARTSTAT on my ATTENDEE in every VEVENT of a stored object that
/// lists me. With `schedule_agent_client`, also marks my ATTENDEE and the
/// ORGANIZER `SCHEDULE-AGENT=CLIENT` so the server sends nothing (§3.3);
/// without, removes their SCHEDULE-AGENT so the server schedules again.
pub fn set_my_partstat(
    stored_ics: &[u8],
    me: &str,
    partstat: &ParticipationStatus,
    schedule_agent_client: bool,
) -> Result<String, ItipError> {
    let mut raw = parse(stored_ics)?.raw;
    let mut found = false;
    for component in events_mut(&mut raw) {
        for entry in &mut component.entries {
            let mine = entry.name == ICalendarProperty::Attendee && is_me(entry, me);
            if mine {
                found = true;
                set_param(
                    entry,
                    ICalendarParameterName::Partstat,
                    partstat_value(partstat),
                );
            }
            if mine || entry.name == ICalendarProperty::Organizer {
                if schedule_agent_client {
                    set_param(
                        entry,
                        ICalendarParameterName::ScheduleAgent,
                        ICalendarParameterValue::ScheduleAgent(ICalendarScheduleAgentValue::Client),
                    );
                } else {
                    entry
                        .params
                        .retain(|param| param.name != ICalendarParameterName::ScheduleAgent);
                }
            }
        }
    }
    if !found {
        return Err(ItipError::NotAttendee {
            address: me.to_string(),
        });
    }
    Ok(raw.to_string())
}

/// The stored object with each VEVENT of the incoming REQUEST in place of
/// the stored one for the same RECURRENCE-ID (or added, for a new override).
/// Each replaced VEVENT keeps my PARTSTAT and SCHEDULE-AGENT, the ORGANIZER's
/// SCHEDULE-AGENT, my VALARMs, and the stored X- and personal properties the
/// request does not set. Stored overrides the request leaves out are kept.
pub fn apply_update(
    stored_ics: &[u8],
    incoming_request: &[u8],
    me: &str,
) -> Result<String, ItipError> {
    let stored = parse(stored_ics)?;
    let incoming = parse(incoming_request)?;
    let uid = stored_uid(&stored);
    let updates: Vec<(Option<OffsetDateTime>, u32)> = instances(&incoming, uid).collect();
    if updates.is_empty() {
        return Err(ItipError::EventNotFound {
            uid: uid.to_string(),
        });
    }
    let stored_instances: HashMap<u32, Option<OffsetDateTime>> =
        instances(&stored, uid).map(|(key, id)| (id, key)).collect();
    let master = instances(&stored, uid)
        .find(|(key, _)| key.is_none())
        .map(|(_, id)| id);

    let mut out = new_calendar(root_entries(&stored.raw).cloned().collect());
    let mut applied = HashSet::new();
    let mut ids = Vec::new();
    for &id in &stored.raw.components[ROOT].component_ids {
        let component = &stored.raw.components[index(id)];
        if component.component_type == ICalendarComponentType::VTimezone {
            continue;
        }
        let update = stored_instances
            .get(&id)
            .and_then(|key| updates.iter().find(|(incoming, _)| incoming == key));
        ids.push(match update {
            Some(&(key, incoming_id)) => {
                applied.insert(key);
                merge(
                    &mut out,
                    &incoming.raw,
                    incoming_id,
                    &stored.raw,
                    Some(id),
                    me,
                )
            }
            None => copy_tree(&mut out, &stored.raw, id),
        });
    }
    for &(key, incoming_id) in &updates {
        if !applied.contains(&key) {
            ids.push(merge(
                &mut out,
                &incoming.raw,
                incoming_id,
                &stored.raw,
                master,
                me,
            ));
        }
    }
    out.components[ROOT].component_ids = ids;
    add_missing_timezones(&mut out, &[&incoming.raw, &stored.raw]);
    Ok(out.to_string())
}

/// The stored object with `STATUS:CANCELLED` on every VEVENT the CANCEL
/// names: the whole series when it has no RECURRENCE-ID, otherwise only
/// that instance, added as an override built from the master if needed.
/// SEQUENCE of each cancelled VEVENT is raised to the CANCEL's so an older
/// REQUEST reads as outdated.
pub fn apply_cancel(stored_ics: &[u8], incoming_cancel: &[u8]) -> Result<String, ItipError> {
    let stored = parse(stored_ics)?;
    let incoming = parse(incoming_cancel)?;
    let uid = stored_uid(&stored);
    let cancels: Vec<(Option<OffsetDateTime>, u32)> = instances(&incoming, uid).collect();
    if cancels.is_empty() {
        return Err(ItipError::EventNotFound {
            uid: uid.to_string(),
        });
    }
    let stored_instances: Vec<(Option<OffsetDateTime>, u32)> = instances(&stored, uid).collect();
    let mut raw = stored.raw.clone();
    for &(key, incoming_id) in &cancels {
        let incoming_component = &incoming.raw.components[index(incoming_id)];
        let sequence = sequence_of(incoming_component);
        let targets: Vec<u32> = stored_instances
            .iter()
            .filter(|(stored_key, _)| key.is_none() || *stored_key == key)
            .map(|(_, id)| *id)
            .collect();
        if targets.is_empty() {
            let master = stored_instances
                .iter()
                .find(|(stored_key, _)| stored_key.is_none())
                .map(|(_, id)| *id);
            let override_component = cancelled_override(&stored, master, incoming_component);
            let id = push(&mut raw, override_component);
            raw.components[ROOT].component_ids.push(id);
        }
        for id in targets {
            let component = &mut raw.components[index(id)];
            set_property(component, cancelled());
            if sequence > sequence_of(component) {
                set_property(component, sequence_entry(sequence));
            }
        }
    }
    add_missing_timezones(&mut raw, &[&incoming.raw]);
    Ok(raw.to_string())
}

/// One CalDAV object per UID of a PUBLISH or plain `.ics` file: its master,
/// overrides and the VTIMEZONEs they reference, without METHOD (RFC 4791
/// §4.1 forbids it in stored objects). UIDs keep their source order.
pub fn for_import(ics: &[u8]) -> Result<Vec<(String, String)>, ItipError> {
    let calendar = parse(ics)?;
    let mut uids: Vec<&str> = Vec::new();
    for event in &calendar.events {
        if !uids.contains(&event.uid.as_str()) {
            uids.push(&event.uid);
        }
    }
    let root: Vec<ICalendarEntry> = root_entries(&calendar.raw).cloned().collect();
    Ok(uids
        .into_iter()
        .map(|uid| {
            let mut out = new_calendar(root.clone());
            for (_, id) in instances(&calendar, uid) {
                let copy = copy_tree(&mut out, &calendar.raw, id);
                out.components[ROOT].component_ids.push(copy);
            }
            add_missing_timezones(&mut out, &[&calendar.raw]);
            (uid.to_string(), out.to_string())
        })
        .collect())
}

/// The SCHEDULE-STATUS (RFC 6638 §3.2.9) the server wrote on `organizer`'s
/// ORGANIZER property in my stored copy, from the master if there is one.
/// Several statuses are joined with `,`; `None` when there is none.
pub fn schedule_status(
    stored_ics: &[u8],
    organizer_address: &str,
) -> Result<Option<String>, ItipError> {
    let stored = parse(stored_ics)?;
    let id = stored
        .events
        .iter()
        .zip(&stored.components)
        .find(|(event, _)| event.recurrence_id.is_none())
        .or_else(|| stored.events.iter().zip(&stored.components).next())
        .map(|(_, id)| *id);
    let Some(id) = id else {
        return Ok(None);
    };
    let statuses: Vec<&str> = stored.raw.components[index(id)]
        .properties(&ICalendarProperty::Organizer)
        .filter(|entry| organizer(entry).is_some_and(|o| o.is_address(organizer_address)))
        .flat_map(|entry| entry.parameters(&ICalendarParameterName::ScheduleStatus))
        .filter_map(ICalendarParameterValue::as_text)
        .collect();
    Ok((!statuses.is_empty()).then(|| statuses.join(",")))
}

pub(crate) fn index(id: u32) -> usize {
    usize::try_from(id).expect("component ids index a Vec")
}

fn instance(recurrence_id: Option<&EventTime>) -> Option<OffsetDateTime> {
    recurrence_id.map(|time| time.instant)
}

fn is_me(entry: &ICalendarEntry, me: &str) -> bool {
    attendee(entry).is_some_and(|attendee| attendee.is_address(me))
}

fn stored_uid(stored: &Calendar) -> &str {
    &stored.events[0].uid
}

/// (RECURRENCE-ID instant, component id) of every usable VEVENT of `uid`.
fn instances<'a>(
    calendar: &'a Calendar,
    uid: &'a str,
) -> impl Iterator<Item = (Option<OffsetDateTime>, u32)> + 'a {
    calendar
        .events
        .iter()
        .zip(&calendar.components)
        .filter(move |(event, _)| event.uid == uid)
        .map(|(event, id)| (instance(event.recurrence_id.as_ref()), *id))
}

pub(crate) fn root_entries(raw: &ICalendar) -> impl Iterator<Item = &ICalendarEntry> {
    raw.components[ROOT]
        .entries
        .iter()
        .filter(|entry| entry.name != ICalendarProperty::Method)
}

fn events_mut(raw: &mut ICalendar) -> impl Iterator<Item = &mut ICalendarComponent> {
    raw.components
        .iter_mut()
        .filter(|component| component.component_type == ICalendarComponentType::VEvent)
}

pub(crate) fn new_calendar(root: Vec<ICalendarEntry>) -> ICalendar {
    ICalendar {
        components: vec![ICalendarComponent {
            component_type: ICalendarComponentType::VCalendar,
            entries: root,
            component_ids: Vec::new(),
        }],
    }
}

/// Appends `component` unattached and returns its id.
pub(crate) fn push(out: &mut ICalendar, component: ICalendarComponent) -> u32 {
    out.components.push(component);
    u32::try_from(out.components.len() - 1).expect("calcard caps the component count")
}

/// Copies component `id` of `source` and its subcomponents into `out`, unattached.
pub(crate) fn copy_tree(out: &mut ICalendar, source: &ICalendar, id: u32) -> u32 {
    let component = &source.components[index(id)];
    let children = component
        .component_ids
        .iter()
        .map(|&child| copy_tree(out, source, child))
        .collect();
    push(
        out,
        ICalendarComponent {
            component_type: component.component_type.clone(),
            entries: component.entries.clone(),
            component_ids: children,
        },
    )
}

/// The incoming VEVENT with my answer and local additions taken from
/// `stored_id`, if there is a stored counterpart.
fn merge(
    out: &mut ICalendar,
    incoming: &ICalendar,
    incoming_id: u32,
    stored: &ICalendar,
    stored_id: Option<u32>,
    me: &str,
) -> u32 {
    let Some(stored_id) = stored_id else {
        return copy_tree(out, incoming, incoming_id);
    };
    let source = &incoming.components[index(incoming_id)];
    let kept = &stored.components[index(stored_id)];
    let mut entries = source.entries.clone();

    let my_stored = kept
        .entries
        .iter()
        .find(|entry| entry.name == ICalendarProperty::Attendee && is_me(entry, me));
    let organizer_agent = kept
        .property(&ICalendarProperty::Organizer)
        .and_then(|entry| entry.parameter(&ICalendarParameterName::ScheduleAgent));
    for entry in &mut entries {
        if let Some(my_stored) =
            my_stored.filter(|_| entry.name == ICalendarProperty::Attendee && is_me(entry, me))
        {
            for name in [
                ICalendarParameterName::Partstat,
                ICalendarParameterName::ScheduleAgent,
            ] {
                if let Some(value) = my_stored.parameter(&name) {
                    set_param(entry, name, value.clone());
                }
            }
        }
        if let Some(agent) = organizer_agent.filter(|_| entry.name == ICalendarProperty::Organizer)
        {
            set_param(entry, ICalendarParameterName::ScheduleAgent, agent.clone());
        }
    }

    let incoming_names: HashSet<&ICalendarProperty> =
        source.entries.iter().map(|entry| &entry.name).collect();
    let local: Vec<ICalendarEntry> = kept
        .entries
        .iter()
        .filter(|entry| is_local(&entry.name) && !incoming_names.contains(&entry.name))
        .cloned()
        .collect();
    entries.extend(local);

    let children = kept
        .component_ids
        .iter()
        .map(|&child| copy_tree(out, stored, child))
        .collect();
    push(
        out,
        ICalendarComponent {
            component_type: source.component_type.clone(),
            entries,
            component_ids: children,
        },
    )
}

fn is_local(name: &ICalendarProperty) -> bool {
    LOCAL_PROPERTIES.contains(name)
        || matches!(name, ICalendarProperty::Other(other) if other.to_ascii_uppercase().starts_with("X-"))
}

/// A cancelled override for an instance the stored object has none for:
/// my copy of the master (my PARTSTAT, local additions) with the CANCEL's
/// instance properties, starting at the RECURRENCE-ID when the CANCEL has
/// no DTSTART. Without a master it is the CANCEL's VEVENT itself.
fn cancelled_override(
    stored: &Calendar,
    master: Option<u32>,
    incoming: &ICalendarComponent,
) -> ICalendarComponent {
    let mut component = instance_override(stored, master, incoming);
    set_property(&mut component, cancelled());
    component
}

/// An override for the instance `incoming` names: my copy of the master
/// with its instance properties.
fn instance_override(
    stored: &Calendar,
    master: Option<u32>,
    incoming: &ICalendarComponent,
) -> ICalendarComponent {
    let mut entries: Vec<ICalendarEntry> = match master {
        Some(master) => stored.raw.components[index(master)]
            .entries
            .iter()
            .filter(|entry| {
                !SERIES_PROPERTIES.contains(&entry.name)
                    && !INSTANCE_PROPERTIES.contains(&entry.name)
            })
            .chain(
                incoming
                    .entries
                    .iter()
                    .filter(|entry| INSTANCE_PROPERTIES.contains(&entry.name)),
            )
            .cloned()
            .collect(),
        None => incoming.entries.clone(),
    };
    let has = |entries: &[ICalendarEntry], name: &ICalendarProperty| {
        entries.iter().any(|entry| &entry.name == name)
    };
    if !has(&entries, &ICalendarProperty::Dtstart)
        && let Some(recurrence_id) = incoming.property(&ICalendarProperty::RecurrenceId)
    {
        let mut start = recurrence_id.clone();
        start.name = ICalendarProperty::Dtstart;
        start
            .params
            .retain(|param| param.name != ICalendarParameterName::Range);
        entries.push(start);
    }
    let length = master
        .and_then(|master| stored.components.iter().position(|&id| id == master))
        .map(|event| &stored.events[event])
        .and_then(|event| {
            Some((event.end.as_ref()?.instant - event.start.instant).whole_seconds())
        });
    if let Some(seconds) = length.filter(|_| {
        !has(&entries, &ICalendarProperty::Dtend) && !has(&entries, &ICalendarProperty::Duration)
    }) {
        entries.push(
            ICalendarEntry::new(ICalendarProperty::Duration)
                .with_value(ICalendarDuration::from_seconds(seconds)),
        );
    }
    ICalendarComponent {
        component_type: ICalendarComponentType::VEvent,
        entries,
        component_ids: Vec::new(),
    }
}

fn cancelled() -> ICalendarEntry {
    ICalendarEntry::new(ICalendarProperty::Status)
        .with_value(ICalendarValue::Status(ICalendarStatus::Cancelled))
}

fn sequence_entry(sequence: i64) -> ICalendarEntry {
    ICalendarEntry::new(ICalendarProperty::Sequence).with_value(ICalendarValue::Integer(sequence))
}

fn sequence_of(component: &ICalendarComponent) -> i64 {
    component
        .property(&ICalendarProperty::Sequence)
        .and_then(|entry| entry.values.first())
        .and_then(ICalendarValue::as_integer)
        .unwrap_or_default()
}

pub(crate) fn dtstamp(now: OffsetDateTime) -> ICalendarEntry {
    ICalendarEntry::new(ICalendarProperty::Dtstamp)
        .with_value(PartialDateTime::from_utc_timestamp(now.unix_timestamp()))
}

/// Replaces the first property of the entry's name in place, or appends it.
pub(crate) fn set_property(component: &mut ICalendarComponent, entry: ICalendarEntry) {
    match component
        .entries
        .iter()
        .position(|existing| existing.name == entry.name)
    {
        Some(position) => {
            let name = entry.name.clone();
            component.entries[position] = entry;
            let mut seen = false;
            component.entries.retain(|existing| {
                if existing.name != name {
                    return true;
                }
                let first = !seen;
                seen = true;
                first
            });
        }
        None => component.entries.push(entry),
    }
}

/// Replaces every `name` parameter with one `value`, where the first was.
pub(crate) fn set_param(
    entry: &mut ICalendarEntry,
    name: ICalendarParameterName,
    value: ICalendarParameterValue,
) {
    let position = entry.params.iter().position(|param| param.name == name);
    entry.params.retain(|param| param.name != name);
    let param = ICalendarParameter::new(name, value);
    match position {
        Some(position) => entry.params.insert(position.min(entry.params.len()), param),
        None => entry.params.push(param),
    }
}

pub(crate) fn partstat_value(partstat: &ParticipationStatus) -> ICalendarParameterValue {
    match partstat {
        ParticipationStatus::NeedsAction => {
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::NeedsAction)
        }
        ParticipationStatus::Accepted => {
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::Accepted)
        }
        ParticipationStatus::Declined => {
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::Declined)
        }
        ParticipationStatus::Tentative => {
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::Tentative)
        }
        ParticipationStatus::Delegated => {
            ICalendarParameterValue::Partstat(ICalendarParticipationStatus::Delegated)
        }
        ParticipationStatus::Other(value) => ICalendarParameterValue::Text(value.clone()),
    }
}

/// Adds, ahead of everything else, each VTIMEZONE that a component of `out`
/// references by TZID and `out` lacks, from the first of `sources` that has it.
pub(crate) fn add_missing_timezones(out: &mut ICalendar, sources: &[&ICalendar]) {
    let present: HashSet<String> = out
        .components
        .iter()
        .filter(|component| component.component_type == ICalendarComponentType::VTimezone)
        .filter_map(|component| text(component, &ICalendarProperty::Tzid))
        .map(str::to_string)
        .collect();
    let mut wanted: Vec<String> = Vec::new();
    for component in &out.components {
        if component.component_type == ICalendarComponentType::VTimezone {
            continue;
        }
        for tzid in component.entries.iter().filter_map(ICalendarEntry::tz_id) {
            if !present.contains(tzid) && !wanted.iter().any(|known| known == tzid) {
                wanted.push(tzid.to_string());
            }
        }
    }
    let mut added = Vec::new();
    for tzid in wanted {
        let found = sources.iter().find_map(|source| {
            source
                .components
                .iter()
                .position(|component| {
                    component.component_type == ICalendarComponentType::VTimezone
                        && text(component, &ICalendarProperty::Tzid) == Some(tzid.as_str())
                })
                .map(|position| (*source, position))
        });
        if let Some((source, position)) = found {
            let id = u32::try_from(position).expect("calcard caps the component count");
            added.push(copy_tree(out, source, id));
        }
    }
    out.components[ROOT].component_ids.splice(0..0, added);
}
