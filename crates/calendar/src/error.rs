/// Why an iCalendar object could not be turned into a [`crate::Calendar`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("input is not an iCalendar object")]
    NotICalendar,
    #[error("malformed iCalendar object: {0}")]
    Malformed(String),
    #[error("iCalendar object contains no usable VEVENT")]
    NoEvents,
}

/// Why a single VEVENT was left out of [`crate::Calendar::events`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    #[error("VEVENT #{index} has no UID")]
    MissingUid { index: usize },
    #[error("VEVENT {uid} has no valid DTSTART")]
    InvalidStart { uid: String },
}

/// Why an iTIP or `.ics` transform failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ItipError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("no VEVENT {uid} with that RECURRENCE-ID")]
    EventNotFound { uid: String },
    #[error("{address} is not an attendee")]
    NotAttendee { address: String },
}
