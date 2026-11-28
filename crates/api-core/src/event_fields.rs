//! The event editor's fields (plan §4 M-F) between their DTO and the
//! calendar crate, shared by `GET /api/calendar/events/{accountId}/{href}`
//! and the engine's edit ops.

use tagmail_calendar::{EditAttendee, EventFields, LocalTime, ParticipationStatus};
use time::{Date, Month, PrimitiveDateTime, Time};

use crate::calendar_dto::{
    CalEventDto, EventAttendeeDto, EventDetailDto, EventFieldsDto, InvitationOrganizerDto,
    PartstatDto,
};

const MAX_TEXT: usize = 8 * 1024;
const MAX_DESCRIPTION: usize = 64 * 1024;
const MAX_ADDRESS: usize = 254;
const MAX_ATTENDEES: usize = 200;
const MAX_ALARMS: usize = 10;
/// Four weeks.
const MAX_ALARM_MINUTES: i64 = 4 * 7 * 24 * 60;
const UTC: &str = "UTC";
const MAX_TZID: usize = 128;

/// A field of an edit that is not acceptable, and why.
pub type Invalid = (&'static str, String);

fn date_text(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn local_text(t: &LocalTime) -> String {
    match t {
        LocalTime::Date(d) => date_text(*d),
        LocalTime::DateTime { local, .. } => format!(
            "{}T{:02}:{:02}:{:02}",
            date_text(local.date()),
            local.hour(),
            local.minute(),
            local.second()
        ),
    }
}

fn partstat_dto(p: &ParticipationStatus) -> PartstatDto {
    match p {
        ParticipationStatus::NeedsAction => PartstatDto::NeedsAction,
        ParticipationStatus::Accepted => PartstatDto::Accepted,
        ParticipationStatus::Declined => PartstatDto::Declined,
        ParticipationStatus::Tentative => PartstatDto::Tentative,
        ParticipationStatus::Delegated => PartstatDto::Delegated,
        ParticipationStatus::Other(_) => PartstatDto::Other,
    }
}

pub fn fields_dto(f: &EventFields) -> EventFieldsDto {
    let tzid = match &f.start {
        LocalTime::Date(_) => None,
        LocalTime::DateTime { zone, .. } => zone.clone(),
    };
    EventFieldsDto {
        summary: f.summary.clone(),
        location: f.location.clone(),
        description: f.description.clone(),
        start: local_text(&f.start),
        end: local_text(&f.end),
        all_day: f.start.is_date(),
        tzid,
        attendees: f
            .attendees
            .iter()
            .map(|a| EventAttendeeDto {
                email: a.address.clone(),
                name: a.name.clone(),
                partstat: Some(partstat_dto(&a.partstat)),
            })
            .collect(),
        alarms: f.alarms.clone(),
    }
}

/// Whether the event's ORGANIZER is absent or one of `mine`.
pub fn organized_by(organizer: Option<&str>, mine: &[String]) -> bool {
    organizer.is_none_or(|o| mine.iter().any(|m| m.eq_ignore_ascii_case(o)))
}

/// The editor's view of a cached event; `None` when it does not parse.
pub fn detail(event: &CalEventDto, mine: &[String]) -> Option<EventDetailDto> {
    let calendar = tagmail_calendar::parse(event.ics.as_bytes()).ok()?;
    let fields = tagmail_calendar::fields(&calendar)?;
    let main = calendar
        .events
        .iter()
        .find(|e| e.recurrence_id.is_none())
        .or_else(|| calendar.events.first())?;
    Some(EventDetailDto {
        account_id: event.account_id,
        calendar_href: event.calendar_href.clone(),
        href: event.href.clone(),
        etag: event.etag.clone(),
        uid: event.uid.clone(),
        sequence: main.sequence,
        organizer: main.organizer.as_ref().map(|o| InvitationOrganizerDto {
            name: o.name.clone(),
            email: o.address.clone(),
            alt_email: o
                .email
                .clone()
                .filter(|e| !e.eq_ignore_ascii_case(&o.address)),
        }),
        organized_by_me: organized_by(main.organizer.as_ref().map(|o| o.address.as_str()), mine),
        recurring: main.recurrence.is_some(),
        fixed_times: tagmail_calendar::has_exceptions(&calendar),
        fields: fields_dto(&fields),
    })
}

fn text(
    field: &'static str,
    value: &Option<String>,
    max: usize,
) -> Result<Option<String>, Invalid> {
    let value = value
        .as_deref()
        .map(|v| v.replace("\r\n", "\n").replace('\r', "\n"));
    let value = value.as_deref().map(str::trim).filter(|v| !v.is_empty());
    match value {
        Some(v) if v.len() > max || v.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => {
            Err((
                field,
                format!("at most {max} bytes, without control characters"),
            ))
        }
        v => Ok(v.map(str::to_owned)),
    }
}

fn local(
    field: &'static str,
    value: &str,
    all_day: bool,
    zone: &Option<String>,
) -> Result<LocalTime, Invalid> {
    let bad = || {
        (
            field,
            "not a date (YYYY-MM-DD) or wall clock time (YYYY-MM-DDTHH:MM:SS)".to_owned(),
        )
    };
    let (day, clock) = match value.split_once('T') {
        Some((day, clock)) if !all_day => (day, Some(clock)),
        None if all_day => (value, None),
        _ => return Err(bad()),
    };
    let number = |s: Option<&str>, len: usize| {
        s.filter(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<i32>().ok())
    };
    let mut parts = day.split('-');
    let (Some(y), Some(m), Some(d), None) = (
        number(parts.next(), 4),
        number(parts.next(), 2),
        number(parts.next(), 2),
        parts.next(),
    ) else {
        return Err(bad());
    };
    let date = Month::try_from(u8::try_from(m).unwrap_or(0))
        .ok()
        .and_then(|m| Date::from_calendar_date(y, m, u8::try_from(d).ok()?).ok())
        .ok_or_else(bad)?;
    let Some(clock) = clock else {
        return Ok(LocalTime::Date(date));
    };
    let mut parts = clock.split(':');
    let (Some(h), Some(min), s, None) = (
        number(parts.next(), 2),
        number(parts.next(), 2),
        parts.next(),
        parts.next(),
    ) else {
        return Err(bad());
    };
    let s = match s {
        Some(s) => number(Some(s), 2).ok_or_else(bad)?,
        None => 0,
    };
    let time = Time::from_hms(
        u8::try_from(h).unwrap_or(99),
        u8::try_from(min).unwrap_or(99),
        u8::try_from(s).unwrap_or(99),
    )
    .map_err(|_| bad())?;
    Ok(LocalTime::DateTime {
        local: PrimitiveDateTime::new(date, time),
        zone: zone.clone(),
    })
}

fn address(value: &str) -> Result<String, Invalid> {
    let value = value.trim();
    let ok = value.len() <= MAX_ADDRESS
        && value
            .split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && !d.is_empty())
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>:;,\"".contains(c));
    ok.then(|| value.to_owned())
        .ok_or(("attendees", format!("{value:?} is not a mail address")))
}

/// Checks an edit's fields. `known_zone` says whether a TZID can be
/// written; the stored event's own TZID is always kept.
pub fn fields_of(
    dto: &EventFieldsDto,
    known_zone: impl Fn(&str) -> bool,
) -> Result<EventFields, Invalid> {
    let zone = match dto.tzid.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(z) if z.eq_ignore_ascii_case(UTC) => Some(UTC.to_owned()),
        Some(z) if z.len() <= MAX_TZID && known_zone(z) => Some(z.to_owned()),
        Some(z) => return Err(("tzid", format!("unknown time zone {z:?}"))),
    };
    let start = local("start", &dto.start, dto.all_day, &zone)?;
    let end = local("end", &dto.end, dto.all_day, &zone)?;
    let ordered = match (&start, &end) {
        (LocalTime::Date(s), LocalTime::Date(e)) => e > s,
        (LocalTime::DateTime { local: s, .. }, LocalTime::DateTime { local: e, .. }) => e >= s,
        _ => false,
    };
    if !ordered {
        return Err(("end", "the end must not be before the start".to_owned()));
    }
    if dto.attendees.len() > MAX_ATTENDEES {
        return Err(("attendees", format!("at most {MAX_ATTENDEES}")));
    }
    let mut attendees: Vec<EditAttendee> = Vec::new();
    for a in &dto.attendees {
        let email = address(&a.email)?;
        if attendees
            .iter()
            .any(|x| x.address.eq_ignore_ascii_case(&email))
        {
            continue;
        }
        attendees.push(EditAttendee {
            address: email,
            name: text("attendees", &a.name, MAX_TEXT)?,
            partstat: ParticipationStatus::NeedsAction,
        });
    }
    if dto.alarms.len() > MAX_ALARMS
        || dto
            .alarms
            .iter()
            .any(|m| !(0..=MAX_ALARM_MINUTES).contains(m))
    {
        return Err((
            "alarms",
            format!("at most {MAX_ALARMS}, each 0 to {MAX_ALARM_MINUTES} minutes before"),
        ));
    }
    Ok(EventFields {
        summary: text("summary", &dto.summary, MAX_TEXT)?,
        location: text("location", &dto.location, MAX_TEXT)?,
        description: text("description", &dto.description, MAX_DESCRIPTION)?,
        start,
        end,
        attendees,
        alarms: dto.alarms.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto() -> EventFieldsDto {
        EventFieldsDto {
            summary: Some(" Lunch ".into()),
            location: None,
            description: Some(String::new()),
            start: "2026-12-01T12:00:00".into(),
            end: "2026-12-01T13:00".into(),
            all_day: false,
            tzid: Some("Europe/Zurich".into()),
            attendees: vec![
                EventAttendeeDto {
                    email: "ann@example.com".into(),
                    name: None,
                    partstat: None,
                },
                EventAttendeeDto {
                    email: "ANN@example.com".into(),
                    name: None,
                    partstat: None,
                },
            ],
            alarms: vec![10],
        }
    }

    #[test]
    fn fields_are_trimmed_checked_and_round_trip() {
        let fields = fields_of(&dto(), |z| z == "Europe/Zurich").unwrap();
        assert_eq!(fields.summary.as_deref(), Some("Lunch"));
        assert_eq!(fields.description, None);
        assert_eq!(fields.attendees.len(), 1);
        let back = fields_dto(&fields);
        assert_eq!(back.end, "2026-12-01T13:00:00");
        assert_eq!(back.tzid.as_deref(), Some("Europe/Zurich"));
    }

    #[test]
    fn bad_fields_name_the_field() {
        let with = |f: fn(&mut EventFieldsDto)| {
            let mut d = dto();
            f(&mut d);
            fields_of(&d, |_| true).map(drop).unwrap_err().0
        };
        assert_eq!(with(|d| d.end = "2026-12-01T11:00:00".into()), "end");
        assert_eq!(with(|d| d.start = "tomorrow".into()), "start");
        assert_eq!(
            with(|d| d.attendees[0].email = "nobody".into()),
            "attendees"
        );
        assert_eq!(with(|d| d.alarms = vec![-5]), "alarms");
        assert_eq!(
            fields_of(&dto(), |_| false).map(drop).unwrap_err().0,
            "tzid"
        );
        let mut all_day = dto();
        all_day.all_day = true;
        all_day.start = "2026-12-01".into();
        all_day.end = "2026-12-01".into();
        assert_eq!(
            fields_of(&all_day, |_| true).map(drop).unwrap_err().0,
            "end"
        );
    }
}
