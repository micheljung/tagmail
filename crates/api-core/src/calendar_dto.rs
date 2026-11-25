//! Calendar DTOs (plan §3.5). They exist in every build, so the generated
//! TypeScript types do not depend on the `calendar` feature.

use serde::{Deserialize, Deserializer, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingDto {
    Server,
    Client,
}

/// The owner's scheduling choice (plan §3.3); `auto` follows what
/// discovery detected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingOverrideDto {
    #[default]
    Auto,
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CaldavStatusDto {
    /// Not discovered yet.
    Unconfigured,
    /// The server is under another registrable domain than the IMAP
    /// host; `POST .../confirm-host` before anything is sent to it.
    NeedsConfirmation,
    Ok,
    Error,
}

/// `GET|PUT /api/accounts/{id}/caldav`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CaldavSettingsDto {
    /// `null`: discovered from the account's addresses.
    pub url: Option<String>,
    /// A login other than the IMAP one; `null` reuses the IMAP login.
    pub username: Option<String>,
    /// A password is stored for `username`; never returned itself.
    pub has_password: bool,
    /// Allows `http` URLs, like the IMAP plain-security setting.
    pub allow_insecure: bool,
    pub scheduling: SchedulingOverrideDto,
    /// What discovery detected; `null` before discovery.
    pub scheduling_detected: Option<SchedulingDto>,
    /// Invitations of an account without calendars go to this account's.
    pub fallback_account_id: Option<i64>,
    pub default_calendar_href: Option<String>,
    /// The owner-confirmed `Authentication-Results` id; `null` = never
    /// applied automatically.
    pub authserv_id: Option<String>,
    /// A proposal for `authservId`, read only.
    pub authserv_id_suggestion: Option<String>,
    pub confirmed_host: Option<String>,
    pub status: CaldavStatusDto,
    pub last_error: Option<String>,
}

/// `PUT /api/accounts/{id}/caldav` body; it replaces every setting but
/// two: `password` absent keeps the stored one for an unchanged `username`,
/// and `authservId` absent keeps the stored one (`null` clears it).
#[derive(Clone, Default, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
#[ts(optional_fields = nullable)]
pub struct CaldavSettingsInputDto {
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[ts(optional = false)]
    pub allow_insecure: bool,
    #[ts(optional = false)]
    pub scheduling: SchedulingOverrideDto,
    pub fallback_account_id: Option<i64>,
    pub default_calendar_href: Option<String>,
    #[serde(deserialize_with = "present")]
    #[ts(type = "string | null", optional)]
    pub authserv_id: Option<Option<String>>,
}

impl std::fmt::Debug for CaldavSettingsInputDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaldavSettingsInputDto")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("allow_insecure", &self.allow_insecure)
            .field("scheduling", &self.scheduling)
            .finish_non_exhaustive()
    }
}

/// `POST /api/accounts/{id}/caldav/discover` and `.../confirm-host`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CaldavDiscoveryDto {
    pub status: CaldavStatusDto,
    /// The host to confirm (`needs_confirmation`) or the confirmed one.
    pub host: Option<String>,
    pub url: Option<String>,
    pub scheduling_detected: Option<SchedulingDto>,
    pub calendars: Vec<CalendarDto>,
    pub error: Option<String>,
}

/// `POST /api/accounts/{id}/caldav/confirm-host` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmHostDto {
    pub host: String,
}

/// A calendar collection, keyed by (`accountId`, `href`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CalendarDto {
    pub account_id: i64,
    pub href: String,
    pub display_name: Option<String>,
    /// The owner's color, else the server's.
    pub color: Option<String>,
    pub visible: bool,
    pub sort: Option<i64>,
    pub writable: bool,
    /// Invitations of the account go here.
    pub is_default: bool,
}

/// `GET /api/calendars`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CalendarsDto {
    pub calendars: Vec<CalendarDto>,
}

/// `PATCH /api/calendars` body. `color: null` and `sort: null` reset to
/// the default; absent fields stay.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CalendarPatchDto {
    pub account_id: i64,
    pub href: String,
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub color: Option<Option<String>>,
    #[serde(default)]
    #[ts(optional)]
    pub visible: Option<bool>,
    #[serde(default)]
    #[ts(optional)]
    pub default: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    #[ts(optional)]
    pub sort: Option<Option<i64>>,
}

/// Tells a field that is `null` (`Some(None)`) from one that is absent.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
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
