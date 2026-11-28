use std::collections::{HashMap, HashSet};
use std::ops::Range;

use calcard::common::timezone::Tz;
use calcard::icalendar::dates::TimeOrDelta;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarProperty,
};
use chrono::{DateTime, NaiveDateTime};
use time::OffsetDateTime;

use crate::model::{Calendar, Event, EventTime, Occurrence};
use crate::tz::Zones;

const MAX_INSTANCES_PER_EVENT: usize = 1000;
const MAX_SCANNED_INSTANCES_PER_PASS: usize = 10_000;
const MAX_RECURRING_SERIES: usize = 100;
/// Bounds the shared pass, whose expansions are all held at once.
const MAX_SHARED_SCANNED_INSTANCES: usize = 20 * MAX_SCANNED_INSTANCES_PER_PASS;

/// The result of [`Calendar::expand`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub occurrences: Vec<Occurrence>,
    /// The RECURRENCE-ID of each occurrence, in the form of the master's
    /// DTSTART (`YYYYMMDD`, `YYYYMMDDTHHMMSSZ`, `TZID=<id>:YYYYMMDDTHHMMSS`
    /// or floating); `None` for an event that does not recur.
    pub recurrence_ids: Vec<Option<String>>,
    /// A scan budget ran out before the window was covered, so instances
    /// may be missing.
    pub truncated: bool,
    /// A recurring series may have instances at or after the window's end:
    /// one was scanned there, or a budget ran out first.
    pub continues: bool,
}

const DATE_FORMAT: &str = "%Y%m%d";
const LOCAL_FORMAT: &str = "%Y%m%dT%H%M%S";

/// `local` written as a value of the same form as `like`.
fn value_text(local: NaiveDateTime, like: &EventTime) -> String {
    if like.all_day {
        return local.format(DATE_FORMAT).to_string();
    }
    let wall = local.format(LOCAL_FORMAT);
    match &like.tzid {
        Some(tzid) => format!("TZID={tzid}:{wall}"),
        None if like.floating => wall.to_string(),
        None => format!("{wall}Z"),
    }
}

/// A masked copy to expand, and what its budget has to cover.
struct Pass {
    calendar: ICalendar,
    budget: usize,
    recurring: HashSet<u32>,
    /// Scanned instances that produce no output: EXDATEs and overrides.
    slack: usize,
    /// Several rules share the budget, so one can starve the next.
    shared: bool,
}

impl Calendar {
    /// [`Calendar::expand`] without the truncation flag.
    pub fn occurrences(&self, window: Range<OffsetDateTime>) -> Vec<Occurrence> {
        self.expand(window).occurrences
    }

    /// Instances of all events that overlap `window`, sorted by start, with
    /// RRULE, RDATE, EXDATE and RECURRENCE-ID overrides applied and at most
    /// 1000 instances per event.
    ///
    /// Expansion starts at DTSTART. Each of the first 100 recurring series
    /// (a UID with its overrides) gets its own pass with a budget of 10 000
    /// scanned instances, so a dense rule cannot starve another series; all
    /// other events, and any further recurring series, share one pass whose
    /// budget grows by 10 000 per further series, up to 200 000. A window
    /// far past the start of a long-running rule may therefore come back
    /// short, which [`Expansion::truncated`] reports.
    pub fn expand(&self, window: Range<OffsetDateTime>) -> Expansion {
        let zones = Zones::new(&self.raw);
        let index_of: HashMap<u32, usize> = self
            .components
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        let (isolated, extra_series) = self.isolated_series();
        let shared_budget = MAX_SCANNED_INSTANCES_PER_PASS
            .saturating_mul(extra_series + 1)
            .min(MAX_SHARED_SCANNED_INSTANCES);
        let shared = self.pass(
            |event| !isolated.contains(event.uid.as_str()),
            shared_budget,
        );
        let passes = std::iter::once(shared).chain(
            isolated
                .iter()
                .map(|uid| self.pass(|event| event.uid == *uid, MAX_SCANNED_INSTANCES_PER_PASS)),
        );

        let mut per_event = vec![0usize; self.events.len()];
        let mut occurrences = Vec::new();
        let mut truncated = false;
        let mut continues = false;
        for pass in passes {
            let expanded = pass.calendar.expand_dates(Tz::Floating, pass.budget);
            let mut scanned = pass.slack;
            let mut last_scanned = None;
            for expanded in expanded.events {
                let Some(&index) = index_of.get(&expanded.comp_id) else {
                    continue;
                };
                let event = &self.events[index];
                let Some(start) = instant(expanded.start, event, &zones) else {
                    continue;
                };
                let recurs = pass.recurring.contains(&expanded.comp_id);
                if recurs {
                    scanned += 1;
                    last_scanned = Some(start);
                }
                if recurs || event.recurrence_id.is_some() {
                    continues |= start >= window.end;
                }
                let end = match expanded.end {
                    TimeOrDelta::Time(end) => instant(end, event, &zones),
                    TimeOrDelta::Delta(delta) => {
                        time::Duration::try_from(delta.to_std().unwrap_or_default())
                            .ok()
                            .and_then(|delta| start.checked_add(delta))
                    }
                }
                .unwrap_or(start);
                let overlaps = start < window.end && (end > window.start || start >= window.start);
                if overlaps && per_event[index] < MAX_INSTANCES_PER_EVENT {
                    per_event[index] += 1;
                    let recurrence_id = match &event.recurrence_id {
                        Some(rid) => self.override_id(expanded.comp_id, rid),
                        None if recurs => {
                            Some(value_text(expanded.start.naive_local(), &event.start))
                        }
                        None => None,
                    };
                    occurrences.push((
                        Occurrence {
                            event: index,
                            start,
                            end,
                        },
                        recurrence_id,
                    ));
                }
            }
            let exhausted = scanned >= pass.budget;
            let short = last_scanned.is_some_and(|last| last < window.end);
            truncated |= exhausted && (short || pass.shared);
            continues |= exhausted;
        }
        occurrences.sort_by_key(|(occurrence, _)| occurrence.start);
        let (occurrences, recurrence_ids) = occurrences.into_iter().unzip();
        Expansion {
            occurrences,
            recurrence_ids,
            truncated,
            continues,
        }
    }

    /// An override's RECURRENCE-ID as written, in the form of `rid`.
    fn override_id(&self, component: u32, rid: &EventTime) -> Option<String> {
        let parsed = self
            .raw
            .components
            .get(component as usize)?
            .property(&ICalendarProperty::RecurrenceId)?
            .values
            .first()?
            .as_partial_date_time()?
            .to_date_time()?;
        Some(match parsed.offset {
            Some(offset) => {
                let utc = parsed.date_time
                    - chrono::Duration::seconds(i64::from(offset.local_minus_utc()));
                format!("{}Z", utc.format(LOCAL_FORMAT))
            }
            None => value_text(parsed.date_time, rid),
        })
    }

    fn pass(&self, keep: impl Fn(&Event) -> bool, budget: usize) -> Pass {
        let mut recurring = HashSet::new();
        let mut slack = 0;
        for (event, id) in self.events.iter().zip(&self.components) {
            if !keep(event) {
                continue;
            }
            if self.recurs(*id) {
                recurring.insert(*id);
                slack += event.recurrence.as_ref().map_or(0, |r| r.exdates.len());
            } else if event.recurrence_id.is_some() {
                slack += 1;
            }
        }
        Pass {
            calendar: self.masked(&keep),
            budget,
            shared: recurring.len() > 1,
            recurring,
            slack,
        }
    }

    fn recurs(&self, id: u32) -> bool {
        self.raw.components.get(id as usize).is_some_and(|c| {
            c.has_property(&ICalendarProperty::Rrule) || c.has_property(&ICalendarProperty::Rdate)
        })
    }

    /// UIDs of the first recurring series (RRULE or RDATE), in source
    /// order, and how many further recurring series there are.
    fn isolated_series(&self) -> (HashSet<&str>, usize) {
        let mut uids = HashSet::new();
        let mut extra = HashSet::new();
        for (event, id) in self.events.iter().zip(&self.components) {
            if !self.recurs(*id) || uids.contains(event.uid.as_str()) {
                continue;
            }
            if uids.len() < MAX_RECURRING_SERIES {
                uids.insert(event.uid.as_str());
            } else {
                extra.insert(event.uid.as_str());
            }
        }
        (uids, extra.len())
    }

    /// A copy holding only the VCALENDAR, its time zones and the events that
    /// `keep` selects; every other component is replaced by an empty one so
    /// component ids stay valid. calcard's scan budget is per call, which
    /// makes it per copy.
    ///
    /// calcard pairs an override with its series only when their SEQUENCEs
    /// are equal, but organizers raise an override's SEQUENCE on their own
    /// (RFC 5546 §3.2.2), so each override is given its master's here.
    fn masked(&self, keep: &impl Fn(&Event) -> bool) -> ICalendar {
        let kept: HashSet<u32> = self
            .events
            .iter()
            .zip(&self.components)
            .filter(|(event, _)| keep(event))
            .map(|(_, id)| *id)
            .collect();
        let mut master_sequence: HashMap<&str, Option<&ICalendarEntry>> = HashMap::new();
        for (event, id) in self.events.iter().zip(&self.components) {
            if event.recurrence_id.is_none() && kept.contains(id) && self.recurs(*id) {
                master_sequence
                    .entry(event.uid.as_str())
                    .or_insert_with(|| {
                        self.raw
                            .components
                            .get(*id as usize)
                            .and_then(|master| master.property(&ICalendarProperty::Sequence))
                    });
            }
        }
        let overrides: HashMap<u32, Option<&ICalendarEntry>> = self
            .events
            .iter()
            .zip(&self.components)
            .filter(|(event, id)| event.recurrence_id.is_some() && kept.contains(id))
            .filter_map(|(event, id)| Some((*id, *master_sequence.get(event.uid.as_str())?)))
            .collect();
        let components = self
            .raw
            .components
            .iter()
            .enumerate()
            .map(|(id, component)| {
                let structural = matches!(
                    component.component_type,
                    ICalendarComponentType::VCalendar
                        | ICalendarComponentType::VTimezone
                        | ICalendarComponentType::Standard
                        | ICalendarComponentType::Daylight
                );
                let selected = u32::try_from(id).is_ok_and(|id| kept.contains(&id));
                let paired = u32::try_from(id).ok().and_then(|id| overrides.get(&id));
                if let Some(sequence) = paired {
                    let mut component = component.clone();
                    component
                        .entries
                        .retain(|entry| entry.name != ICalendarProperty::Sequence);
                    component.entries.extend(sequence.cloned());
                    component
                } else if structural || selected {
                    component.clone()
                } else {
                    ICalendarComponent {
                        component_type: ICalendarComponentType::Other(String::new()),
                        entries: Vec::new(),
                        component_ids: Vec::new(),
                    }
                }
            })
            .collect();
        ICalendar { components }
    }
}

/// calcard leaves times in zones it cannot name floating; those get the
/// event's own VTIMEZONE observances here.
fn instant(value: DateTime<Tz>, event: &Event, zones: &Zones) -> Option<OffsetDateTime> {
    let utc = if value.timezone().is_floating() {
        let local = value.naive_local();
        event
            .start
            .tzid
            .as_deref()
            .and_then(|tzid| zones.to_utc(tzid, local))
            .unwrap_or(local)
    } else {
        value.naive_utc()
    };
    OffsetDateTime::from_unix_timestamp(utc.and_utc().timestamp()).ok()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use time::macros::datetime;

    use crate::parse;

    const SERIES: usize = 150;
    const COUNT: usize = 500;

    fn calendar(events: impl IntoIterator<Item = (String, &'static str, &'static str)>) -> String {
        let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n");
        for (uid, start, rule) in events {
            write!(
                ics,
                "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261110T120000Z\r\nDTSTART:{start}\r\n\
                 DURATION:PT1H\r\nRRULE:{rule}\r\nEND:VEVENT\r\n"
            )
            .expect("write to String");
        }
        ics.push_str("END:VCALENDAR\r\n");
        ics
    }

    #[test]
    fn series_past_the_isolated_ones_share_a_scaled_budget() {
        let ics = calendar((0..SERIES).map(|i| {
            (
                format!("series-{i}"),
                "20270101T090000Z",
                "FREQ=HOURLY;COUNT=500",
            )
        }));
        let expansion = parse(ics.as_bytes())
            .expect("parse")
            .expand(datetime!(2027-01-01 0:00 UTC)..datetime!(2028-01-01 0:00 UTC));
        assert_eq!(expansion.occurrences.len(), SERIES * COUNT);
        assert!(!expansion.truncated);
    }

    #[test]
    fn a_rule_scanned_short_of_the_window_is_reported() {
        let ics = calendar([("old".to_owned(), "19900101T090000Z", "FREQ=DAILY")]);
        let calendar = parse(ics.as_bytes()).expect("parse");
        let far = calendar.expand(datetime!(2030-01-01 0:00 UTC)..datetime!(2030-02-01 0:00 UTC));
        assert!(far.truncated);
        assert!(far.occurrences.is_empty());

        let near = calendar.expand(datetime!(1990-01-01 0:00 UTC)..datetime!(1990-02-01 0:00 UTC));
        assert!(!near.truncated);
        assert_eq!(near.occurrences.len(), 31);
    }

    #[test]
    fn recurrence_ids_follow_the_master_start_and_series_ends_are_reported() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:z\r\n\
                   DTSTAMP:20261110T120000Z\r\nDTSTART;TZID=Europe/Zurich:20261201T090000\r\n\
                   DURATION:PT1H\r\nRRULE:FREQ=DAILY;COUNT=2\r\nEND:VEVENT\r\n\
                   BEGIN:VEVENT\r\nUID:z\r\nDTSTAMP:20261110T120000Z\r\n\
                   RECURRENCE-ID;TZID=Europe/Zurich:20261202T090000\r\n\
                   DTSTART;TZID=Europe/Zurich:20261202T100000\r\nDURATION:PT1H\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let calendar = parse(ics.as_bytes()).expect("parse");
        let window = datetime!(2026-11-01 0:00 UTC)..datetime!(2027-01-01 0:00 UTC);
        let expansion = calendar.expand(window);
        assert_eq!(
            expansion.recurrence_ids,
            [
                Some("TZID=Europe/Zurich:20261201T090000".to_owned()),
                Some("TZID=Europe/Zurich:20261202T090000".to_owned()),
            ]
        );
        assert!(!expansion.continues, "both instances are in the window");
        let early = calendar.expand(datetime!(2026-11-01 0:00 UTC)..datetime!(2026-12-02 0:00 UTC));
        assert!(early.continues);

        let open = calendar_of_rule("FREQ=WEEKLY");
        assert!(
            open.expand(datetime!(2026-11-01 0:00 UTC)..datetime!(2027-01-01 0:00 UTC))
                .continues
        );
    }

    fn calendar_of_rule(rule: &'static str) -> crate::Calendar {
        let ics = calendar([("w".to_owned(), "20261201T090000Z", rule)]);
        parse(ics.as_bytes()).expect("parse")
    }
}
