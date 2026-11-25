use std::collections::{HashMap, HashSet};
use std::ops::Range;

use calcard::common::timezone::Tz;
use calcard::icalendar::dates::TimeOrDelta;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarProperty,
};
use chrono::DateTime;
use time::OffsetDateTime;

use crate::model::{Calendar, Event, Occurrence};
use crate::tz::Zones;

const MAX_INSTANCES_PER_EVENT: usize = 1000;
const MAX_SCANNED_INSTANCES_PER_PASS: usize = 10_000;
const MAX_RECURRING_SERIES: usize = 100;

impl Calendar {
    /// Instances of all events that overlap `window`, sorted by start, with
    /// RRULE, RDATE, EXDATE and RECURRENCE-ID overrides applied and at most
    /// 1000 instances per event.
    ///
    /// Expansion starts at DTSTART. Each of the first 100 recurring series
    /// (a UID with its overrides) gets its own pass with a budget of 10 000
    /// scanned instances, so a dense rule cannot starve another series; all
    /// other events, and any further recurring series, share one pass with
    /// one such budget. A window far past the start of a long-running rule
    /// may therefore come back short.
    pub fn occurrences(&self, window: Range<OffsetDateTime>) -> Vec<Occurrence> {
        let zones = Zones::new(&self.raw);
        let index_of: HashMap<u32, usize> = self
            .components
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        let isolated = self.isolated_series();
        let shared = self.masked(|event| !isolated.contains(event.uid.as_str()));
        let passes = std::iter::once(shared).chain(
            isolated
                .iter()
                .map(|uid| self.masked(|event| event.uid == *uid)),
        );

        let mut per_event = vec![0usize; self.events.len()];
        let mut occurrences = Vec::new();
        for pass in passes {
            let expanded = pass.expand_dates(Tz::Floating, MAX_SCANNED_INSTANCES_PER_PASS);
            for expanded in expanded.events {
                let Some(&index) = index_of.get(&expanded.comp_id) else {
                    continue;
                };
                let event = &self.events[index];
                let Some(start) = instant(expanded.start, event, &zones) else {
                    continue;
                };
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
                    occurrences.push(Occurrence {
                        event: index,
                        start,
                        end,
                    });
                }
            }
        }
        occurrences.sort_by_key(|occurrence| occurrence.start);
        occurrences
    }

    /// UIDs of the first recurring series (RRULE or RDATE), in source order.
    fn isolated_series(&self) -> HashSet<&str> {
        let mut uids = HashSet::new();
        for (event, id) in self.events.iter().zip(&self.components) {
            if uids.len() == MAX_RECURRING_SERIES {
                break;
            }
            let recurs = self.raw.components.get(*id as usize).is_some_and(|c| {
                c.has_property(&ICalendarProperty::Rrule)
                    || c.has_property(&ICalendarProperty::Rdate)
            });
            if recurs {
                uids.insert(event.uid.as_str());
            }
        }
        uids
    }

    /// A copy holding only the VCALENDAR, its time zones and the events that
    /// `keep` selects; every other component is replaced by an empty one so
    /// component ids stay valid. calcard's scan budget is per call, which
    /// makes it per copy.
    fn masked(&self, keep: impl Fn(&Event) -> bool) -> ICalendar {
        let kept: HashSet<u32> = self
            .events
            .iter()
            .zip(&self.components)
            .filter(|(event, _)| keep(event))
            .map(|(_, id)| *id)
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
                if structural || selected {
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
