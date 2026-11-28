//! Pure iCalendar handling for invitations: parsing iTIP and `.ics` data into
//! a normalised model, time zone resolution (IANA, Windows, custom
//! VTIMEZONE) and recurrence expansion. No IO.

mod alarm;
mod edit;
mod error;
mod expand;
mod itip;
mod model;
mod parse;
mod tz;
mod window;

pub use alarm::{Alarm, is_time_zone};
pub use edit::{
    EditAttendee, EditOrganizer, Edited, EventFields, LocalTime, apply_reply, attendee_partstat,
    cancel, edit, fields, has_exceptions, has_until, is_known_zone, request,
};
pub use error::{EventError, ItipError, ParseError};
pub use expand::Expansion;
pub use itip::{apply_cancel, apply_update, for_import, reply, schedule_status, set_my_partstat};
pub use model::{
    Attendee, Calendar, Event, EventStatus, EventTime, Frequency, Method, Occurrence, Organizer,
    ParticipationRole, ParticipationStatus, Recurrence, WeekdayNum,
};
pub use parse::parse;
pub use window::{Instance, Summary, WINDOW_DAYS, Window};

impl Calendar {
    /// Re-serialises the parsed object, keeping unknown properties,
    /// parameters and components such as VALARM.
    pub fn to_ics(&self) -> String {
        self.raw.to_string()
    }
}
