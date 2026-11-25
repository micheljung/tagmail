//! Pure iCalendar handling for invitations: parsing iTIP and `.ics` data into
//! a normalised model, time zone resolution (IANA, Windows, custom
//! VTIMEZONE) and recurrence expansion. No IO.

mod error;
mod expand;
mod model;
mod parse;
mod tz;

pub use error::{EventError, ParseError};
pub use model::{
    Attendee, Calendar, Event, EventStatus, EventTime, Frequency, Method, Occurrence, Organizer,
    ParticipationRole, ParticipationStatus, Recurrence, WeekdayNum,
};
pub use parse::parse;

impl Calendar {
    /// Re-serialises the parsed object, keeping unknown properties,
    /// parameters and components such as VALARM.
    pub fn to_ics(&self) -> String {
        self.raw.to_string()
    }
}
