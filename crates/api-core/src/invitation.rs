//! `GET /api/messages/{id}/invitation` (plan §3.5): the invitation card of
//! a message, read from its calendar parts by the bounded extractor.

use crate::api::{Api, Response};
use crate::store::{Store, StoreError};

impl<S: Store> Api<S> {
    #[cfg(not(feature = "calendar"))]
    pub(crate) async fn invitation(&self, _id: &str) -> Result<Response, StoreError> {
        Ok(Response::error(
            404,
            "feature_disabled",
            "calendar support is not built in",
        ))
    }

    #[cfg(feature = "calendar")]
    pub(crate) async fn invitation(&self, id: &str) -> Result<Response, StoreError> {
        let record = match self.record(id).await? {
            Ok(r) => r,
            Err(resp) => return Ok(resp),
        };
        if !record.has_calendar {
            return Ok(card::no_invitation());
        }
        let source = match self.store.calendar_source(record.id).await? {
            Some(source) => source,
            None => {
                if let Err(resp) = self.fetch(&record).await? {
                    return Ok(resp);
                }
                match self.store.calendar_source(record.id).await? {
                    Some(source) => source,
                    None => return Ok(crate::messages::evicted()),
                }
            }
        };
        let card = crate::messages::blocking(move || card::invitation(&source)).await?;
        Ok(match card {
            Ok(dto) => Response::json(200, &dto),
            Err(resp) => resp,
        })
    }
}

#[cfg(feature = "calendar")]
pub(crate) mod card {
    use std::collections::HashMap;

    use mail_parser::{MessageParser, html_to_text};
    use tagmail_calendar::{
        Event, EventStatus, EventTime, Frequency, Method, ParticipationStatus, Recurrence,
    };
    use time::{Duration, OffsetDateTime, UtcOffset, Weekday};

    use crate::api::Response;
    use crate::calendar_dto::{
        AutoApplyDto, FrequencyDto, InvitationAttendeeDto, InvitationDto, InvitationEventDto,
        InvitationMethodDto, InvitationOrganizerDto, PartstatDto, RecurrenceDto, SenderAuthDto,
        WeekdayDto, WeekdayNumDto,
    };
    use crate::mime::reduce::CalendarParts;

    const FROM: &[u8] = b"from";

    pub(crate) fn no_invitation() -> Response {
        Response::error(404, "no_invitation", "the message has no invitation")
    }

    /// An event with the METHOD of the part it came from.
    struct Sourced {
        method: Option<Method>,
        event: Event,
    }

    impl Sourced {
        fn version(&self) -> (u32, Option<OffsetDateTime>) {
            (self.event.sequence, self.event.dtstamp)
        }
    }

    /// The card of `source`; until CalDAV is set up nothing is stored or
    /// applied, so only the sender check varies.
    pub(crate) fn invitation(source: &CalendarParts) -> Result<InvitationDto, Response> {
        if source.parts.is_empty() {
            return Err(no_invitation());
        }
        let too_large = source.parts.iter().any(|p| p.too_large);
        let mut methods = Vec::new();
        let mut events: Vec<Sourced> = Vec::new();
        let mut index = HashMap::new();
        for part in source.parts.iter().filter(|p| !p.too_large) {
            let calendar = match tagmail_calendar::parse(part.text.as_bytes()) {
                Ok(calendar) => calendar,
                Err(e) => {
                    tracing::debug!(part = part.id, error = %e, "unparsable calendar part");
                    continue;
                }
            };
            methods.push(calendar.method.clone());
            for event in calendar.events {
                let sourced = Sourced {
                    method: calendar.method.clone(),
                    event,
                };
                let key = (
                    sourced.event.uid.clone(),
                    sourced.event.recurrence_id.as_ref().map(|r| r.instant),
                );
                match index.get(&key) {
                    Some(&i) => {
                        let kept: &mut Sourced = &mut events[i];
                        if sourced.version() > kept.version() {
                            *kept = sourced;
                        }
                    }
                    None => {
                        index.insert(key, events.len());
                        events.push(sourced);
                    }
                }
            }
        }
        if events.is_empty() && !too_large {
            return Err(Response::error(
                422,
                "unparsable",
                "the invitation cannot be parsed",
            ));
        }
        let method = match methods.split_first() {
            Some((first, rest)) if rest.iter().all(|m| m == first) => method_dto(first.as_ref()),
            Some(_) => InvitationMethodDto::Other,
            None => InvitationMethodDto::None,
        };
        Ok(InvitationDto {
            method,
            sender_auth: sender_auth(&events, &source.headers),
            events: events.iter().map(event_dto).collect(),
            too_large,
            truncated: source.omitted > 0,
            stored: None,
            outdated: false,
            target: None,
            scheduling: None,
            auto_apply: AutoApplyDto::None,
        })
    }

    /// The single `From` mailbox; `None` for no `From`, several `From`
    /// fields, or a `From` with several mailboxes.
    fn from_address(headers: &[u8]) -> Option<String> {
        let from_fields = headers
            .split(|b| *b == b'\n')
            .filter(|line| {
                line.len() > FROM.len()
                    && line[..FROM.len()].eq_ignore_ascii_case(FROM)
                    && line[FROM.len()..].trim_ascii_start().starts_with(b":")
            })
            .count();
        if from_fields != 1 {
            return None;
        }
        let message = MessageParser::new().parse(headers)?;
        let [address] = crate::mime::addresses(message.from()).try_into().ok()?;
        Some(address.email)
    }

    /// Plan §3.4: only iTIP methods that change a calendar are checked
    /// against `From`. No authserv-id can be trusted before the owner
    /// confirms one, so a matching sender is still only `untrusted_config`.
    fn sender_auth(events: &[Sourced], headers: &[u8]) -> SenderAuthDto {
        let checked: Vec<&Sourced> = events
            .iter()
            .filter(|s| {
                matches!(
                    s.method,
                    Some(Method::Request | Method::Cancel | Method::Reply)
                )
            })
            .collect();
        if checked.is_empty() {
            return SenderAuthDto::UntrustedConfig;
        }
        let Some(from) = from_address(headers) else {
            return SenderAuthDto::AddressMismatch;
        };
        let matches = |s: &&Sourced| match s.method {
            Some(Method::Reply) => s.event.attendees.iter().any(|a| a.matches(&from)),
            _ => s.event.organizer.as_ref().is_some_and(|o| o.matches(&from)),
        };
        if checked.iter().all(matches) {
            SenderAuthDto::UntrustedConfig
        } else {
            SenderAuthDto::AddressMismatch
        }
    }

    fn method_dto(method: Option<&Method>) -> InvitationMethodDto {
        match method {
            Some(Method::Request) => InvitationMethodDto::Request,
            Some(Method::Cancel) => InvitationMethodDto::Cancel,
            Some(Method::Reply) => InvitationMethodDto::Reply,
            Some(Method::Publish) => InvitationMethodDto::Publish,
            Some(_) => InvitationMethodDto::Other,
            None => InvitationMethodDto::None,
        }
    }

    fn wall_clock(t: OffsetDateTime) -> String {
        format!(
            "{}T{:02}:{:02}:{:02}",
            t.date(),
            t.hour(),
            t.minute(),
            t.second()
        )
    }

    fn time_text(t: &EventTime) -> String {
        if t.all_day {
            t.instant.date().to_string()
        } else if t.floating {
            wall_clock(t.instant)
        } else {
            format!("{}Z", wall_clock(t.instant.to_offset(UtcOffset::UTC)))
        }
    }

    /// RFC 5545 §3.6.1: without DTEND or DURATION an all-day event lasts
    /// one day and a timed one ends when it starts.
    fn end_text(event: &Event) -> String {
        match &event.end {
            Some(end) => time_text(end),
            None if event.start.all_day => time_text(&EventTime {
                instant: event.start.instant + Duration::DAY,
                ..event.start.clone()
            }),
            None => time_text(&event.start),
        }
    }

    fn partstat_dto(partstat: &ParticipationStatus) -> PartstatDto {
        match partstat {
            ParticipationStatus::NeedsAction => PartstatDto::NeedsAction,
            ParticipationStatus::Accepted => PartstatDto::Accepted,
            ParticipationStatus::Declined => PartstatDto::Declined,
            ParticipationStatus::Tentative => PartstatDto::Tentative,
            ParticipationStatus::Delegated => PartstatDto::Delegated,
            ParticipationStatus::Other(_) => PartstatDto::Other,
        }
    }

    fn weekday_dto(weekday: Weekday) -> WeekdayDto {
        match weekday {
            Weekday::Monday => WeekdayDto::Mo,
            Weekday::Tuesday => WeekdayDto::Tu,
            Weekday::Wednesday => WeekdayDto::We,
            Weekday::Thursday => WeekdayDto::Th,
            Weekday::Friday => WeekdayDto::Fr,
            Weekday::Saturday => WeekdayDto::Sa,
            Weekday::Sunday => WeekdayDto::Su,
        }
    }

    fn frequency_dto(frequency: Frequency) -> FrequencyDto {
        match frequency {
            Frequency::Secondly => FrequencyDto::Secondly,
            Frequency::Minutely => FrequencyDto::Minutely,
            Frequency::Hourly => FrequencyDto::Hourly,
            Frequency::Daily => FrequencyDto::Daily,
            Frequency::Weekly => FrequencyDto::Weekly,
            Frequency::Monthly => FrequencyDto::Monthly,
            Frequency::Yearly => FrequencyDto::Yearly,
        }
    }

    fn recurrence_dto(r: &Recurrence) -> RecurrenceDto {
        RecurrenceDto {
            frequency: frequency_dto(r.frequency),
            interval: r.interval,
            count: r.count,
            until: r.until.as_ref().map(time_text),
            by_day: r
                .by_day
                .iter()
                .map(|d| WeekdayNumDto {
                    ordinal: d.ordinal,
                    weekday: weekday_dto(d.weekday),
                })
                .collect(),
            by_month_day: r.by_month_day.clone(),
            by_month: r.by_month.clone(),
            exdates: r.exdates.iter().map(time_text).collect(),
        }
    }

    fn event_dto(s: &Sourced) -> InvitationEventDto {
        let e = &s.event;
        InvitationEventDto {
            uid: e.uid.clone(),
            recurrence_id: e.recurrence_id.as_ref().map(time_text),
            summary: e.summary.clone(),
            start: time_text(&e.start),
            end: end_text(e),
            all_day: e.start.all_day,
            tzid: e.start.tzid.clone(),
            location: e.location.clone(),
            description: e
                .description
                .clone()
                .or_else(|| e.alt_description.as_deref().map(html_to_text)),
            url: e.url.clone(),
            organizer: e.organizer.as_ref().map(|o| InvitationOrganizerDto {
                name: o.name.clone(),
                email: o.address.clone(),
                alt_email: o
                    .email
                    .clone()
                    .filter(|alt| !alt.eq_ignore_ascii_case(&o.address)),
            }),
            attendees: e
                .attendees
                .iter()
                .map(|a| InvitationAttendeeDto {
                    name: a.name.clone(),
                    email: a.address.clone(),
                    partstat: partstat_dto(&a.partstat),
                })
                .collect(),
            recurrence: e.recurrence.as_ref().map(recurrence_dto),
            cancelled: s.method == Some(Method::Cancel) || e.status == Some(EventStatus::Cancelled),
        }
    }
}
