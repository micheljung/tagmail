//! The cached span of a calendar and what a cache keeps of a resource: its
//! instances in the window and a summary of its main event. The server's
//! sync and the desktop replica both derive their rows here, so the two
//! expand alike.

use time::{Date, OffsetDateTime, Time};

use crate::model::{Calendar, EventStatus, ParticipationStatus};

/// Days before and after today whose instances are kept.
pub const WINDOW_DAYS: i64 = 400;
const DAY_SECS: i64 = 86_400;

/// The cached span around a day, from midnight UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
}

impl Window {
    pub fn around(today: Date) -> Window {
        let midnight = OffsetDateTime::new_utc(today, Time::MIDNIGHT);
        let days = time::Duration::days(WINDOW_DAYS);
        Window {
            start: midnight - days,
            end: midnight + days,
        }
    }

    /// The window around the UTC day of `now`, Unix seconds.
    pub fn around_unix(now: i64) -> Window {
        let midnight = now - now.rem_euclid(DAY_SECS);
        let today = OffsetDateTime::from_unix_timestamp(midnight)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .date();
        Window::around(today)
    }

    pub fn start_ts(&self) -> i64 {
        self.start.unix_timestamp()
    }

    pub fn end_ts(&self) -> i64 {
        self.end.unix_timestamp()
    }
}

/// One instance in the window; times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub start: i64,
    pub end: i64,
    pub recurrence_id: Option<String>,
    pub all_day: bool,
}

/// What a cache keeps of a resource's main event (the master, else the
/// first override); times are Unix seconds, values iCalendar text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub uid: String,
    /// The master's SEQUENCE; 0 when the resource holds only overrides.
    pub sequence: u32,
    pub dtstamp: Option<i64>,
    /// e.g. `CONFIRMED`.
    pub status: Option<String>,
    /// The PARTSTAT of the first attendee matching one of `me`.
    pub my_partstat: Option<String>,
    pub organizer: Option<String>,
    pub is_recurring: bool,
    pub first_start: Option<i64>,
    /// `None` for a recurring resource.
    pub last_end: Option<i64>,
}

fn status_text(status: EventStatus) -> &'static str {
    match status {
        EventStatus::Tentative => "TENTATIVE",
        EventStatus::Confirmed => "CONFIRMED",
        EventStatus::Cancelled => "CANCELLED",
    }
}

fn partstat_text(partstat: &ParticipationStatus) -> String {
    match partstat {
        ParticipationStatus::NeedsAction => "NEEDS-ACTION".to_owned(),
        ParticipationStatus::Accepted => "ACCEPTED".to_owned(),
        ParticipationStatus::Declined => "DECLINED".to_owned(),
        ParticipationStatus::Tentative => "TENTATIVE".to_owned(),
        ParticipationStatus::Delegated => "DELEGATED".to_owned(),
        ParticipationStatus::Other(other) => other.clone(),
    }
}

impl Calendar {
    fn has_recurrence(&self) -> bool {
        self.events.iter().any(|e| e.recurrence.is_some())
    }

    /// The instances in `window`, whether the expansion was truncated, and
    /// the window edge to record: the window's end while a recurring series
    /// may continue past it, `None` once all its instances are in.
    pub fn instances(&self, window: &Window) -> (Vec<Instance>, bool, Option<i64>) {
        let expansion = self.expand(window.start..window.end);
        let edge = (self.has_recurrence() && (expansion.truncated || expansion.continues))
            .then(|| window.end_ts());
        let rows = expansion
            .occurrences
            .iter()
            .zip(expansion.recurrence_ids)
            .map(|(o, recurrence_id)| Instance {
                start: o.start.unix_timestamp(),
                end: o.end.unix_timestamp(),
                recurrence_id,
                all_day: self.events[o.event].start.all_day,
            })
            .collect();
        (rows, expansion.truncated, edge)
    }

    /// `None` without any event.
    pub fn summary(&self, me: &[String]) -> Option<Summary> {
        let master = self.events.iter().find(|e| e.recurrence_id.is_none());
        let main = master.or_else(|| self.events.first())?;
        let is_recurring = self.has_recurrence();
        let end_of =
            |e: &crate::model::Event| e.end.as_ref().unwrap_or(&e.start).instant.unix_timestamp();
        Some(Summary {
            uid: main.uid.clone(),
            sequence: master.map_or(0, |m| m.sequence),
            dtstamp: master.and_then(|m| m.dtstamp).map(|d| d.unix_timestamp()),
            status: main.status.map(|s| status_text(s).to_owned()),
            my_partstat: main
                .attendees
                .iter()
                .find(|a| me.iter().any(|m| a.matches(m)))
                .map(|a| partstat_text(&a.partstat)),
            organizer: main.organizer.as_ref().map(|o| o.address.clone()),
            is_recurring,
            first_start: self
                .events
                .iter()
                .map(|e| e.start.instant.unix_timestamp())
                .min(),
            last_end: if is_recurring {
                None
            } else {
                self.events.iter().map(end_of).max()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_spans_whole_days_around_today() {
        let noon = 1_764_072_000;
        let w = Window::around_unix(noon);
        assert_eq!(w.start_ts().rem_euclid(DAY_SECS), 0);
        assert_eq!(w.end_ts() - w.start_ts(), 2 * WINDOW_DAYS * DAY_SECS);
        assert!(w.start_ts() < noon && noon < w.end_ts());
    }
}
