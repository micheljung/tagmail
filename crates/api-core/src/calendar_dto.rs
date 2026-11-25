//! Calendar DTOs (plan §3.5). They exist in every build, so the generated
//! TypeScript types do not depend on the `calendar` feature.

use serde::Serialize;
use ts_rs::TS;

/// `GET /api/messages/{id}/invitation`: the invitation card and what
/// applying it would do. Reading it has no side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InvitationDto {
    pub method: InvitationMethodDto,
    pub events: Vec<InvitationEventDto>,
    /// A calendar part was over the size caps and is not shown.
    pub too_large: bool,
    /// More calendar parts than are read; only the first ones are shown.
    pub truncated: bool,
    pub sender_auth: SenderAuthDto,
    /// The event as the calendar has it; `null` when not stored.
    pub stored: Option<StoredEventDto>,
    /// The invitation is older than the stored event.
    pub outdated: bool,
    /// The calendar an apply would write to; `null` when the account has
    /// none.
    pub target: Option<CalendarTargetDto>,
    /// `null` until the account's CalDAV server is known.
    pub scheduling: Option<SchedulingDto>,
    pub auto_apply: AutoApplyDto,
}

/// The iTIP `METHOD`; `none` for a calendar without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum InvitationMethodDto {
    Request,
    Cancel,
    Reply,
    Publish,
    Other,
    None,
}

/// Plan §3.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SenderAuthDto {
    Verified,
    /// `From` is not the organizer (or, for a reply, the attendee).
    AddressMismatch,
    Unauthenticated,
    /// No trusted authserv-id is confirmed for the account.
    UntrustedConfig,
    /// Served offline by a replica.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AutoApplyDto {
    Applied,
    Pending,
    Manual,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingDto {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PartstatDto {
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
    Delegated,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CalendarTargetDto {
    pub account_id: i64,
    pub calendar_href: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StoredEventDto {
    pub account_id: i64,
    pub calendar_href: String,
    /// The owner's PARTSTAT; `null` when not an attendee.
    pub partstat: Option<PartstatDto>,
    pub sequence: u32,
    pub status: Option<EventStatusDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EventStatusDto {
    Tentative,
    Confirmed,
    Cancelled,
}

/// One VEVENT. Times are UTC RFC 3339 (`2026-11-25T09:00:00Z`); all-day
/// dates are `YYYY-MM-DD` with an exclusive end; floating times have no
/// offset (`2026-11-25T09:00:00`) and are shown as they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InvitationEventDto {
    pub uid: String,
    /// RECURRENCE-ID of an override of one instance.
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    /// The original TZID.
    pub tzid: Option<String>,
    pub location: Option<String>,
    /// Plain text; an HTML-only `X-ALT-DESC` is converted to text.
    pub description: Option<String>,
    pub url: Option<String>,
    pub organizer: Option<InvitationOrganizerDto>,
    pub attendees: Vec<InvitationAttendeeDto>,
    pub recurrence: Option<RecurrenceDto>,
    pub cancelled: bool,
}

/// The ORGANIZER; `email` is its calendar address, without `mailto:` for
/// a mail address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InvitationOrganizerDto {
    pub name: Option<String>,
    pub email: String,
    /// The `EMAIL` parameter when it differs from `email`.
    pub alt_email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InvitationAttendeeDto {
    pub name: Option<String>,
    /// The calendar address, without `mailto:` for a mail address.
    pub email: String,
    pub partstat: PartstatDto,
}

/// An RRULE with its EXDATEs; times as in [`InvitationEventDto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RecurrenceDto {
    pub frequency: FrequencyDto,
    pub interval: u16,
    pub count: Option<u32>,
    pub until: Option<String>,
    pub by_day: Vec<WeekdayNumDto>,
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<u8>,
    pub exdates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FrequencyDto {
    Secondly,
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A BYDAY entry: `-1FR` is `{ordinal: -1, weekday: "fr"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WeekdayNumDto {
    pub ordinal: Option<i16>,
    pub weekday: WeekdayDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum WeekdayDto {
    Mo,
    Tu,
    We,
    Th,
    Fr,
    Sa,
    Su,
}
