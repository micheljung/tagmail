//! `GET /api/messages/{id}/invitation` (plan §3.5): the invitation card of
//! a message, read from its calendar parts by the bounded extractor.

use crate::api::{Api, Response};
use crate::store::{Store, StoreError};

impl<S: Store> Api<S> {
    #[cfg(not(feature = "calendar"))]
    pub(crate) async fn invitation(&self, _id: &str) -> Result<Response, StoreError> {
        Ok(crate::caldav::feature_disabled())
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
        let source = match self.calendar_source_of(&record).await? {
            Ok(source) => source,
            Err(resp) => return Ok(resp),
        };
        let card = crate::messages::blocking(move || card::invitation(&source)).await?;
        let (mut dto, versions, sender) = match card {
            Ok(card) => card,
            Err(resp) => return Ok(resp),
        };
        let mut uids: Vec<String> = versions.iter().map(|v| v.uid.clone()).collect();
        uids.dedup();
        let context = self
            .store
            .invitation_context(record.account_id, record.id, uids)
            .await?;
        card::apply_context(&mut dto, &versions, &sender, context);
        Ok(Response::json(200, &dto))
    }
}

impl<S: Store> Api<S> {
    /// The calendar parts of `record`, fetching the raw message first when
    /// it is not cached.
    #[cfg(feature = "calendar")]
    pub(crate) async fn calendar_source_of(
        &self,
        record: &crate::store::MessageRecord,
    ) -> Result<Result<crate::mime::reduce::CalendarParts, Response>, StoreError> {
        if let Some(source) = self.store.calendar_source(record.id).await? {
            return Ok(Ok(source));
        }
        if let Err(resp) = self.fetch(record).await? {
            return Ok(Err(resp));
        }
        Ok(self
            .store
            .calendar_source(record.id)
            .await?
            .ok_or_else(crate::messages::evicted))
    }
}

#[cfg(feature = "calendar")]
pub(crate) mod card {
    use std::collections::HashMap;

    use mail_parser::html_to_text;
    use tagmail_calendar::{
        Event, EventStatus, EventTime, Frequency, Method, ParticipationStatus, Recurrence,
    };
    use time::{Duration, OffsetDateTime, UtcOffset, Weekday};

    use crate::api::Response;
    use crate::calendar_dto::{
        AutoApplyDto, EventStatusDto, FrequencyDto, InvitationAttendeeDto, InvitationDto,
        InvitationEventDto, InvitationMethodDto, InvitationOrganizerDto, PartstatDto,
        RecurrenceDto, SenderAuthDto, WeekdayDto, WeekdayNumDto,
    };
    use crate::itip_auth::{self, IncomingVersion, StoredVersion};
    use crate::mime::reduce::CalendarParts;
    use crate::store::{InvitationContext, StoredCopy};

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

    /// One VEVENT's place in its series' history.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Version {
        pub uid: String,
        /// Not an override (no RECURRENCE-ID).
        pub master: bool,
        pub sequence: u32,
        /// Unix seconds.
        pub dtstamp: Option<i64>,
        pub method: Option<Method>,
    }

    /// Who sent the invitation, for [`itip_auth::sender_auth`] once the
    /// trusted authserv-id is known.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Sender {
        pub headers: Vec<u8>,
        /// `None` when no event's method changes a calendar; else whether
        /// `From` is the organizer (attendee of a REPLY) of every such
        /// event, leniently and strictly (see [`itip_auth::from_matches`]).
        pub matches: Option<(bool, bool)>,
        /// The first UID's ORGANIZER.
        pub organizer: Option<tagmail_calendar::Organizer>,
        /// Every VEVENT of the first UID is an override.
        pub overrides_only: bool,
        /// When the first UID's last instance ends, Unix seconds; `None`
        /// for a recurring event.
        pub last_end: Option<i64>,
    }

    /// The card of `source` with each event's version; what is stored
    /// comes from [`apply_context`].
    pub(crate) fn invitation(
        source: &CalendarParts,
    ) -> Result<(InvitationDto, Vec<Version>, Sender), Response> {
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
        let versions = events
            .iter()
            .map(|s| Version {
                uid: s.event.uid.clone(),
                master: s.event.recurrence_id.is_none(),
                sequence: s.event.sequence,
                dtstamp: s.event.dtstamp.map(OffsetDateTime::unix_timestamp),
                method: s.method.clone(),
            })
            .collect();
        let checked: Vec<&Sourced> = events.iter().filter(|s| changes_calendar(s)).collect();
        let from = itip_auth::from_address(&source.headers);
        let check = |strict: bool| {
            from.as_deref().is_some_and(|from| {
                checked
                    .iter()
                    .all(|s| itip_auth::from_matches(s.method.as_ref(), &s.event, from, strict))
            })
        };
        let first: Vec<&Event> = events
            .first()
            .map(|f| {
                events
                    .iter()
                    .filter(|s| s.event.uid == f.event.uid)
                    .map(|s| &s.event)
                    .collect()
            })
            .unwrap_or_default();
        let sender = Sender {
            headers: source.headers.clone(),
            matches: (!checked.is_empty()).then(|| (check(false), check(true))),
            organizer: itip_auth::uid_organizer(&first).cloned(),
            overrides_only: !first.is_empty() && first.iter().all(|e| e.recurrence_id.is_some()),
            last_end: (!first.iter().any(|e| e.recurrence.is_some()))
                .then(|| {
                    first
                        .iter()
                        .map(|e| e.end.as_ref().unwrap_or(&e.start).instant.unix_timestamp())
                        .max()
                })
                .flatten(),
        };
        let dto = InvitationDto {
            method,
            sender_auth: SenderAuthDto::UntrustedConfig,
            events: events.iter().map(event_dto).collect(),
            too_large,
            truncated: source.omitted > 0,
            stored: None,
            outdated: false,
            target: None,
            scheduling: None,
            auto_apply: AutoApplyDto::None,
            organizer_changed: false,
        };
        Ok((dto, versions, sender))
    }

    /// Fills in the stored copy (in the target calendar when it has one)
    /// and whether the invitation is older than it.
    pub(crate) fn apply_context(
        dto: &mut InvitationDto,
        versions: &[Version],
        sender: &Sender,
        context: InvitationContext,
    ) {
        dto.sender_auth = itip_auth::sender_auth_of(
            sender.matches,
            &sender.headers,
            context.authserv_id.as_deref(),
        );
        let in_target = |copy: &&StoredCopy| {
            context.target.as_ref().is_some_and(|t| {
                t.account_id == copy.stored.account_id
                    && t.calendar_href == copy.stored.calendar_href
            })
        };
        let stored_of = |uid: &str| {
            let of_uid = || context.stored.iter().filter(move |c| c.uid == uid);
            of_uid().find(in_target).or_else(|| of_uid().next())
        };
        let stored = versions.first().and_then(|v| stored_of(&v.uid));
        dto.outdated = versions
            .iter()
            .filter(|v| v.master)
            .filter(|v| {
                !versions.iter().any(|other| {
                    other.master
                        && other.uid == v.uid
                        && (other.sequence, other.dtstamp) > (v.sequence, v.dtstamp)
                })
            })
            .any(|newest| stored_of(&newest.uid).is_some_and(|copy| older(newest, copy)));
        let first_uid = versions.first().map(|v| v.uid.as_str());
        let newest = newest_master(versions, first_uid).or_else(|| {
            versions
                .iter()
                .filter(|v| Some(v.uid.as_str()) == first_uid)
                .max_by_key(|v| (v.sequence, v.dtstamp))
        });
        let held = stored.map(stored_version);
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let decision = newest.map(|newest| {
            itip_auth::decide(&itip_auth::UpdateCheck {
                method: newest.method.as_ref(),
                incoming: IncomingVersion {
                    sequence: newest.sequence,
                    dtstamp: newest.dtstamp,
                },
                organizer: sender.organizer.as_ref(),
                overrides_only: sender.overrides_only,
                past: sender.last_end.is_some_and(|end| end < now),
                stored: held.as_ref(),
                sender: dto.sender_auth,
                received: context.received,
                now,
                has_target: context.target.is_some(),
            })
        });
        dto.auto_apply = decision.map_or(AutoApplyDto::None, |d| d.auto_apply);
        dto.organizer_changed = decision.is_some_and(|d| d.organizer_changed);
        dto.stored = stored.map(|c| c.stored.clone());
        dto.target = context.target;
        dto.scheduling = context.scheduling;
    }

    /// The incoming master is older than the stored one: a lower SEQUENCE,
    /// or the same with an older DTSTAMP.
    fn older(incoming: &Version, stored: &StoredCopy) -> bool {
        match incoming.sequence.cmp(&stored.stored.sequence) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Equal => incoming
                .dtstamp
                .zip(stored.dtstamp)
                .is_some_and(|(incoming, stored)| incoming < stored),
            std::cmp::Ordering::Greater => false,
        }
    }

    fn changes_calendar(s: &Sourced) -> bool {
        matches!(
            s.method,
            Some(Method::Request | Method::Cancel | Method::Reply)
        )
    }

    /// The newest master version of `uid`.
    fn newest_master<'a>(versions: &'a [Version], uid: Option<&str>) -> Option<&'a Version> {
        versions
            .iter()
            .filter(|v| v.master && Some(v.uid.as_str()) == uid)
            .max_by_key(|v| (v.sequence, v.dtstamp))
    }

    fn stored_version(copy: &StoredCopy) -> StoredVersion {
        StoredVersion {
            sequence: copy.stored.sequence,
            dtstamp: copy.dtstamp,
            cancelled: copy.stored.status == Some(EventStatusDto::Cancelled),
            partstat: copy.stored.partstat.map(|p| {
                match p {
                    PartstatDto::NeedsAction => "NEEDS-ACTION",
                    PartstatDto::Accepted => "ACCEPTED",
                    PartstatDto::Declined => "DECLINED",
                    PartstatDto::Tentative => "TENTATIVE",
                    PartstatDto::Delegated => "DELEGATED",
                    PartstatDto::Other => "OTHER",
                }
                .to_owned()
            }),
            organizer: copy.organizer.clone(),
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
