use time::{OffsetDateTime, Weekday};

use crate::error::EventError;

/// A parsed iCalendar object (iTIP message or plain `.ics` file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    pub method: Option<Method>,
    /// Every VEVENT in source order, recurrence overrides included.
    pub events: Vec<Event>,
    /// VEVENTs left out of `events` because they are unusable.
    pub skipped: Vec<EventError>,
    pub(crate) raw: calcard::icalendar::ICalendar,
    pub(crate) components: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Publish,
    Request,
    Reply,
    Add,
    Cancel,
    Refresh,
    Counter,
    DeclineCounter,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub recurrence_id: Option<EventTime>,
    /// 0 when absent or unparsable, as RFC 5545 defines the default.
    pub sequence: u32,
    pub dtstamp: Option<OffsetDateTime>,
    pub summary: Option<String>,
    pub start: EventTime,
    /// DTEND, or DTSTART + DURATION.
    pub end: Option<EventTime>,
    pub location: Option<String>,
    pub description: Option<String>,
    /// The raw X-ALT-DESC value (usually HTML); callers render it as text.
    pub alt_description: Option<String>,
    pub url: Option<String>,
    pub organizer: Option<Organizer>,
    pub attendees: Vec<Attendee>,
    pub status: Option<EventStatus>,
    pub recurrence: Option<Recurrence>,
}

/// A DATE or DATE-TIME value.
///
/// `instant` is the UTC instant for zoned and UTC values. For `all_day` and
/// `floating` values, which have no zone, it is the wall-clock time read as
/// UTC. A TZID that resolves to no zone yields a floating value that keeps
/// the TZID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTime {
    pub instant: OffsetDateTime,
    /// The original TZID parameter, e.g. `W. Europe Standard Time`.
    pub tzid: Option<String>,
    pub all_day: bool,
    pub floating: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organizer {
    /// The calendar address, without the scheme when it is `mailto:`.
    pub address: String,
    /// Whether `address` is an email address: a `mailto:` URI or a value
    /// with no URI scheme. False for e.g. `urn:uuid:` addresses.
    pub address_is_email: bool,
    pub name: Option<String>,
    /// The `EMAIL` parameter. Sender-controlled like the address; used for
    /// matching only where the address is not an email address.
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    /// The calendar address, without the scheme when it is `mailto:`.
    pub address: String,
    /// Whether `address` is an email address: a `mailto:` URI or a value
    /// with no URI scheme. False for e.g. `urn:uuid:` addresses.
    pub address_is_email: bool,
    pub name: Option<String>,
    pub email: Option<String>,
    pub partstat: ParticipationStatus,
    pub role: ParticipationRole,
    pub rsvp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipationStatus {
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
    Delegated,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipationRole {
    Chair,
    ReqParticipant,
    OptParticipant,
    NonParticipant,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    Tentative,
    Confirmed,
    Cancelled,
}

/// The RRULE parts a human-readable summary needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub frequency: Frequency,
    pub interval: u16,
    pub count: Option<u32>,
    pub until: Option<EventTime>,
    pub by_day: Vec<WeekdayNum>,
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<u8>,
    pub exdates: Vec<EventTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Secondly,
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A BYDAY entry such as `TU` (`ordinal: None`) or `-1SU`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeekdayNum {
    pub ordinal: Option<i16>,
    pub weekday: Weekday,
}

/// One expanded instance of an event, see [`Calendar::occurrences`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    /// Index into [`Calendar::events`].
    pub event: usize,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
}

impl Organizer {
    /// Whether the RFC 5322 From `address` is this organizer, see [`calendar_user_is`].
    pub fn matches(&self, address: &str) -> bool {
        calendar_user_is(
            &self.address,
            self.address_is_email,
            self.email.as_deref(),
            address,
        )
    }
}

impl Attendee {
    /// Whether the RFC 5322 From `address` is this attendee, see [`calendar_user_is`].
    pub fn matches(&self, address: &str) -> bool {
        calendar_user_is(
            &self.address,
            self.address_is_email,
            self.email.as_deref(),
            address,
        )
    }
}

/// Apple's iMIP relay; its `mailto:` addresses are opaque aliases of the
/// `EMAIL` parameter rather than the sender's mailbox.
const IMIP_ALIAS_DOMAIN: &str = "imip.me.com";

/// Case-insensitive match of a From address against a calendar user. An
/// email address is authoritative: the `EMAIL` parameter counts only for
/// other schemes (e.g. `urn:uuid:`) and iMIP aliases, since anyone can add an
/// `EMAIL` naming their own mailbox next to someone else's address.
fn calendar_user_is(
    address: &str,
    address_is_email: bool,
    email: Option<&str>,
    from: &str,
) -> bool {
    let from = from.trim();
    if address_is_email && !is_imip_alias(address) {
        return address.eq_ignore_ascii_case(from);
    }
    email.is_some_and(|email| email.trim().eq_ignore_ascii_case(from))
}

fn is_imip_alias(address: &str) -> bool {
    address
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(IMIP_ALIAS_DOMAIN))
}
