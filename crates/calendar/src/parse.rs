use calcard::Entry;
use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarFrequency,
    ICalendarParameterName, ICalendarProperty, ICalendarRecurrenceRule, ICalendarValue,
};
use chrono::{Duration, NaiveDateTime};
use time::OffsetDateTime;

use crate::error::{EventError, ParseError};
use crate::model::{
    Attendee, Calendar, Event, EventStatus, EventTime, Frequency, Method, Organizer,
    ParticipationRole, ParticipationStatus, Recurrence, WeekdayNum,
};
use crate::tz::{Zones, text, weekday};

const DEFAULT_INTERVAL: u16 = 1;
const BEGIN: &str = "BEGIN:";
const BEGIN_VCALENDAR: &str = "BEGIN:VCALENDAR";
const END_VCALENDAR: &str = "END:VCALENDAR";
const MAILTO: &str = "mailto";

/// Parses iTIP or plain `.ics` bytes. Non-UTF-8 bytes are replaced, unknown
/// properties are kept for [`Calendar::to_ics`], unusable values (e.g. a
/// garbled DTSTAMP) are dropped, and a VEVENT without UID or DTSTART is
/// skipped and listed in [`Calendar::skipped`]. Fails with
/// [`ParseError::NoEvents`] when no VEVENT is usable.
pub fn parse(input: &[u8]) -> Result<Calendar, ParseError> {
    let source = String::from_utf8_lossy(input);
    let raw = match ICalendar::parse(source.as_ref()) {
        Ok(raw) => raw,
        Err(Entry::InvalidLine(_)) if !contains(&source, BEGIN) => {
            return Err(ParseError::NotICalendar);
        }
        Err(Entry::InvalidLine(line)) => {
            return Err(ParseError::Malformed(format!("invalid line: {line}")));
        }
        Err(Entry::UnexpectedComponentEnd { expected, found }) => {
            return Err(ParseError::Malformed(format!(
                "END:{} where END:{} was expected",
                found.as_str(),
                expected.as_str()
            )));
        }
        Err(Entry::UnterminatedComponent(component)) => {
            return Err(ParseError::Malformed(format!("unterminated {component}")));
        }
        Err(Entry::TooManyComponents) => {
            return Err(ParseError::Malformed("too many components".to_string()));
        }
        Err(_) => return Err(ParseError::NotICalendar),
    };

    if contains(&source, BEGIN_VCALENDAR) && !contains(&source, END_VCALENDAR) {
        return Err(ParseError::Malformed("unterminated VCALENDAR".to_string()));
    }

    let zones = Zones::new(&raw);
    let method = raw
        .components
        .first()
        .and_then(|root| text(root, &ICalendarProperty::Method))
        .map(method);

    let mut events = Vec::new();
    let mut components = Vec::new();
    let mut skipped = Vec::new();
    let vevents = raw
        .components
        .iter()
        .enumerate()
        .filter(|(_, component)| component.component_type == ICalendarComponentType::VEvent);
    for (index, (id, component)) in vevents.enumerate() {
        match event(component, index, &zones) {
            Ok(event) => {
                events.push(event);
                components.push(u32::try_from(id).map_err(|_| ParseError::NotICalendar)?);
            }
            Err(error) => skipped.push(error),
        }
    }
    if events.is_empty() {
        return Err(ParseError::NoEvents);
    }
    Ok(Calendar {
        method,
        events,
        skipped,
        raw,
        components,
    })
}

fn contains(source: &str, needle: &str) -> bool {
    source
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn event(component: &ICalendarComponent, index: usize, zones: &Zones) -> Result<Event, EventError> {
    let uid = text(component, &ICalendarProperty::Uid)
        .filter(|uid| !uid.trim().is_empty())
        .ok_or(EventError::MissingUid { index })?
        .to_string();
    let recurrence_id = |default_tzid| {
        component
            .property(&ICalendarProperty::RecurrenceId)
            .and_then(|entry| time_of(entry, default_tzid, zones))
    };
    // An instance CANCEL may omit DTSTART (RFC 5546 §3.2.5); it starts at its RECURRENCE-ID.
    let start = component
        .property(&ICalendarProperty::Dtstart)
        .and_then(|entry| time_of(entry, None, zones))
        .or_else(|| recurrence_id(None));
    let Some(start) = start else {
        return Err(EventError::InvalidStart { uid });
    };
    let end = component
        .property(&ICalendarProperty::Dtend)
        .and_then(|entry| time_of(entry, None, zones))
        .or_else(|| end_from_duration(component, &start));
    let start_tzid = start.tzid.as_deref();

    Ok(Event {
        recurrence_id: recurrence_id(start_tzid),
        sequence: component
            .property(&ICalendarProperty::Sequence)
            .and_then(|entry| entry.values.first())
            .and_then(ICalendarValue::as_integer)
            .and_then(|sequence| u32::try_from(sequence).ok())
            .unwrap_or_default(),
        dtstamp: component
            .property(&ICalendarProperty::Dtstamp)
            .and_then(|entry| entry.values.first())
            .and_then(ICalendarValue::as_partial_date_time)
            .and_then(PartialDateTime::to_timestamp)
            .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok()),
        summary: owned_text(component, &ICalendarProperty::Summary),
        location: owned_text(component, &ICalendarProperty::Location),
        description: owned_text(component, &ICalendarProperty::Description),
        alt_description: owned_text(component, &ICalendarProperty::Other("X-ALT-DESC".into())),
        url: owned_text(component, &ICalendarProperty::Url),
        organizer: component
            .property(&ICalendarProperty::Organizer)
            .and_then(organizer),
        attendees: component
            .properties(&ICalendarProperty::Attendee)
            .filter_map(attendee)
            .collect(),
        status: text(component, &ICalendarProperty::Status).and_then(status),
        recurrence: recurrence(component, start_tzid, zones),
        uid,
        start,
        end,
    })
}

fn time_of(entry: &ICalendarEntry, default_tzid: Option<&str>, zones: &Zones) -> Option<EventTime> {
    let value = entry.values.first()?.as_partial_date_time()?;
    let tzid = entry.tz_id().or(default_tzid);
    event_time(value, tzid, zones)
}

fn event_time(value: &PartialDateTime, tzid: Option<&str>, zones: &Zones) -> Option<EventTime> {
    let parsed = value.to_date_time()?;
    let local = parsed.date_time;
    if !value.has_time() {
        return Some(EventTime {
            instant: utc(local)?,
            tzid: None,
            all_day: true,
            floating: true,
        });
    }
    if let Some(offset) = parsed.offset {
        return Some(EventTime {
            instant: utc(local - Duration::seconds(i64::from(offset.local_minus_utc())))?,
            tzid: None,
            all_day: false,
            floating: false,
        });
    }
    let resolved = tzid.and_then(|tzid| zones.to_utc(tzid, local));
    Some(EventTime {
        instant: utc(resolved.unwrap_or(local))?,
        tzid: tzid.map(str::to_string),
        all_day: false,
        floating: resolved.is_none(),
    })
}

fn utc(naive: NaiveDateTime) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(naive.and_utc().timestamp()).ok()
}

fn end_from_duration(component: &ICalendarComponent, start: &EventTime) -> Option<EventTime> {
    let duration = component
        .property(&ICalendarProperty::Duration)
        .and_then(|entry| entry.values.first())
        .and_then(|value| match value {
            ICalendarValue::Duration(duration) => Some(duration.as_seconds()),
            _ => None,
        })?;
    Some(EventTime {
        instant: start
            .instant
            .checked_add(time::Duration::seconds(duration))?,
        ..start.clone()
    })
}

fn owned_text(component: &ICalendarComponent, name: &ICalendarProperty) -> Option<String> {
    text(component, name).map(str::to_string)
}

fn parameter<'a>(entry: &'a ICalendarEntry, name: &ICalendarParameterName) -> Option<&'a str> {
    entry.parameter(name).and_then(|value| value.as_text())
}

/// The calendar address, stripped of `mailto:`, and whether it is an email
/// address (`mailto:` or no URI scheme at all).
fn address(entry: &ICalendarEntry) -> Option<(String, bool)> {
    let value = entry.values.first()?.as_text()?.trim();
    let (address, is_email) = match uri_scheme(value) {
        None => (value, true),
        Some(scheme) if scheme.eq_ignore_ascii_case(MAILTO) => {
            (value[scheme.len() + 1..].trim(), true)
        }
        Some(_) => (value, false),
    };
    (!address.is_empty()).then(|| (address.to_string(), is_email))
}

/// The scheme of `value` if it is a URI; a `:` after the `@` (e.g. a port)
/// does not start one.
fn uri_scheme(value: &str) -> Option<&str> {
    let colon = value.find(':')?;
    let before_at = value.find('@').is_none_or(|at| colon < at);
    before_at.then(|| &value[..colon])
}

pub(crate) fn organizer(entry: &ICalendarEntry) -> Option<Organizer> {
    let (address, address_is_email) = address(entry)?;
    Some(Organizer {
        address,
        address_is_email,
        name: parameter(entry, &ICalendarParameterName::Cn).map(str::to_string),
        email: parameter(entry, &ICalendarParameterName::Email).map(str::to_string),
    })
}

pub(crate) fn attendee(entry: &ICalendarEntry) -> Option<Attendee> {
    let (address, address_is_email) = address(entry)?;
    Some(Attendee {
        address,
        address_is_email,
        name: parameter(entry, &ICalendarParameterName::Cn).map(str::to_string),
        email: parameter(entry, &ICalendarParameterName::Email).map(str::to_string),
        partstat: parameter(entry, &ICalendarParameterName::Partstat)
            .map_or(ParticipationStatus::NeedsAction, partstat),
        role: parameter(entry, &ICalendarParameterName::Role)
            .map_or(ParticipationRole::ReqParticipant, role),
        rsvp: parameter(entry, &ICalendarParameterName::Rsvp)
            .is_some_and(|rsvp| rsvp.eq_ignore_ascii_case("TRUE")),
    })
}

fn method(value: &str) -> Method {
    match value.to_ascii_uppercase().as_str() {
        "PUBLISH" => Method::Publish,
        "REQUEST" => Method::Request,
        "REPLY" => Method::Reply,
        "ADD" => Method::Add,
        "CANCEL" => Method::Cancel,
        "REFRESH" => Method::Refresh,
        "COUNTER" => Method::Counter,
        "DECLINECOUNTER" => Method::DeclineCounter,
        _ => Method::Other(value.to_string()),
    }
}

fn partstat(value: &str) -> ParticipationStatus {
    match value.to_ascii_uppercase().as_str() {
        "NEEDS-ACTION" => ParticipationStatus::NeedsAction,
        "ACCEPTED" => ParticipationStatus::Accepted,
        "DECLINED" => ParticipationStatus::Declined,
        "TENTATIVE" => ParticipationStatus::Tentative,
        "DELEGATED" => ParticipationStatus::Delegated,
        _ => ParticipationStatus::Other(value.to_string()),
    }
}

fn role(value: &str) -> ParticipationRole {
    match value.to_ascii_uppercase().as_str() {
        "CHAIR" => ParticipationRole::Chair,
        "REQ-PARTICIPANT" => ParticipationRole::ReqParticipant,
        "OPT-PARTICIPANT" => ParticipationRole::OptParticipant,
        "NON-PARTICIPANT" => ParticipationRole::NonParticipant,
        _ => ParticipationRole::Other(value.to_string()),
    }
}

fn status(value: &str) -> Option<EventStatus> {
    match value.to_ascii_uppercase().as_str() {
        "TENTATIVE" => Some(EventStatus::Tentative),
        "CONFIRMED" => Some(EventStatus::Confirmed),
        "CANCELLED" => Some(EventStatus::Cancelled),
        _ => None,
    }
}

fn recurrence(
    component: &ICalendarComponent,
    start_tzid: Option<&str>,
    zones: &Zones,
) -> Option<Recurrence> {
    let rule: &ICalendarRecurrenceRule = component
        .property(&ICalendarProperty::Rrule)
        .and_then(|entry| entry.values.first())
        .and_then(|value| match value {
            ICalendarValue::RecurrenceRule(rule) => Some(rule.as_ref()),
            _ => None,
        })?;
    let exdates = component
        .properties(&ICalendarProperty::Exdate)
        .flat_map(|entry| {
            let tzid = entry.tz_id().or(start_tzid);
            entry
                .values
                .iter()
                .filter_map(ICalendarValue::as_partial_date_time)
                .filter_map(move |value| event_time(value, tzid, zones))
        })
        .collect();
    Some(Recurrence {
        frequency: frequency(rule.freq),
        interval: rule.interval.unwrap_or(DEFAULT_INTERVAL),
        count: rule.count,
        until: rule
            .until
            .as_ref()
            .and_then(|until| event_time(until, start_tzid, zones)),
        by_day: rule
            .byday
            .iter()
            .map(|day| WeekdayNum {
                ordinal: day.ordwk,
                weekday: time_weekday(weekday(day.weekday)),
            })
            .collect(),
        by_month_day: rule.bymonthday.clone(),
        by_month: rule.bymonth.iter().map(|month| month.month()).collect(),
        exdates,
    })
}

fn frequency(freq: ICalendarFrequency) -> Frequency {
    match freq {
        ICalendarFrequency::Secondly => Frequency::Secondly,
        ICalendarFrequency::Minutely => Frequency::Minutely,
        ICalendarFrequency::Hourly => Frequency::Hourly,
        ICalendarFrequency::Daily => Frequency::Daily,
        ICalendarFrequency::Weekly => Frequency::Weekly,
        ICalendarFrequency::Monthly => Frequency::Monthly,
        ICalendarFrequency::Yearly => Frequency::Yearly,
    }
}

fn time_weekday(day: chrono::Weekday) -> time::Weekday {
    match day {
        chrono::Weekday::Mon => time::Weekday::Monday,
        chrono::Weekday::Tue => time::Weekday::Tuesday,
        chrono::Weekday::Wed => time::Weekday::Wednesday,
        chrono::Weekday::Thu => time::Weekday::Thursday,
        chrono::Weekday::Fri => time::Weekday::Friday,
        chrono::Weekday::Sat => time::Weekday::Saturday,
        chrono::Weekday::Sun => time::Weekday::Sunday,
    }
}
