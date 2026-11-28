use std::collections::HashMap;

use calcard::icalendar::timezone::TzResolver;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarProperty, ICalendarValue,
    ICalendarWeekday,
};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Weekday};

const SECONDS_PER_HOUR: i32 = 3600;
const SECONDS_PER_MINUTE: i32 = 60;
const DAYS_PER_WEEK: i64 = 7;

/// Maps TZIDs to UTC offsets: IANA and Windows names and VTIMEZONE hints via
/// calcard, then VTIMEZONEs with no known name (Outlook's "Customized Time
/// Zone") from their own STANDARD/DAYLIGHT observances.
pub(crate) struct Zones {
    known: TzResolver<String>,
    custom: HashMap<String, CustomZone>,
}

impl Zones {
    pub(crate) fn new(ical: &ICalendar) -> Self {
        let known = ical.build_owned_tz_resolver();
        let custom = ical
            .timezones()
            .filter_map(|zone| {
                let tzid = text(zone, &ICalendarProperty::Tzid)?;
                if known.resolve(tzid).is_some() {
                    return None;
                }
                Some((tzid.to_string(), CustomZone::new(ical, zone)?))
            })
            .collect();
        Self { known, custom }
    }

    /// The UTC instant of `local` in zone `tzid`, or `None` if the zone is unknown.
    pub(crate) fn to_utc(&self, tzid: &str, local: NaiveDateTime) -> Option<NaiveDateTime> {
        if let Some(tz) = self.known.resolve(tzid).filter(|tz| !tz.is_floating()) {
            let zoned = tz.from_local_datetime(&local).earliest().or_else(|| {
                tz.from_local_datetime(&(local + Duration::hours(1)))
                    .earliest()
            })?;
            return Some(zoned.naive_utc());
        }
        let offset = self.custom.get(tzid)?.offset_at(local);
        Some(local - Duration::seconds(i64::from(offset)))
    }

    /// The wall clock in zone `tzid` at `utc`, or `None` if the zone is
    /// unknown.
    pub(crate) fn to_local(&self, tzid: &str, utc: NaiveDateTime) -> Option<NaiveDateTime> {
        if let Some(tz) = self.known.resolve(tzid).filter(|tz| !tz.is_floating()) {
            return Some(tz.from_utc_datetime(&utc).naive_local());
        }
        let zone = self.custom.get(tzid)?;
        let guess = utc + Duration::seconds(i64::from(zone.offset_at(utc)));
        Some(utc + Duration::seconds(i64::from(zone.offset_at(guess))))
    }
}

struct CustomZone {
    observances: Vec<Observance>,
}

struct Observance {
    start: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
    rule: Option<YearlyRule>,
}

/// The only RRULE shape Windows-derived zones use: the nth weekday of a month.
struct YearlyRule {
    month: u32,
    ordinal: i16,
    weekday: Weekday,
    until: Option<NaiveDateTime>,
}

impl CustomZone {
    fn new(ical: &ICalendar, zone: &ICalendarComponent) -> Option<Self> {
        let observances: Vec<_> = zone
            .component_ids
            .iter()
            .filter_map(|id| ical.components.get(*id as usize))
            .filter(|c| {
                matches!(
                    c.component_type,
                    ICalendarComponentType::Standard | ICalendarComponentType::Daylight
                )
            })
            .filter_map(Observance::new)
            .collect();
        (!observances.is_empty()).then_some(Self { observances })
    }

    fn offset_at(&self, local: NaiveDateTime) -> i32 {
        let latest = self
            .observances
            .iter()
            .flat_map(|o| o.onsets(local.year()).map(move |onset| (onset, o)))
            .filter(|(onset, _)| *onset <= local)
            .max_by_key(|(onset, _)| *onset);
        match latest {
            Some((_, observance)) => observance.offset_to,
            None => self
                .observances
                .iter()
                .min_by_key(|o| o.start)
                .map_or(0, |o| o.offset_from),
        }
    }
}

impl Observance {
    fn new(component: &ICalendarComponent) -> Option<Self> {
        let mut start = None;
        let mut offset_from = None;
        let mut offset_to = None;
        let mut rule = None;
        for entry in &component.entries {
            match (&entry.name, entry.values.first()) {
                (ICalendarProperty::Dtstart, Some(ICalendarValue::PartialDateTime(dt))) => {
                    start = dt.to_date_time().map(|dt| dt.date_time);
                }
                (ICalendarProperty::Tzoffsetfrom, Some(ICalendarValue::PartialDateTime(dt))) => {
                    offset_from = utc_offset(dt);
                }
                (ICalendarProperty::Tzoffsetto, Some(ICalendarValue::PartialDateTime(dt))) => {
                    offset_to = utc_offset(dt);
                }
                (ICalendarProperty::Rrule, Some(ICalendarValue::RecurrenceRule(rrule))) => {
                    rule = match (rrule.bymonth.as_slice(), rrule.byday.as_slice()) {
                        ([month], [day]) => Some(YearlyRule {
                            month: u32::from(month.month()),
                            ordinal: day.ordwk.unwrap_or(1),
                            weekday: weekday(day.weekday),
                            until: rrule
                                .until
                                .as_ref()
                                .and_then(|u| u.to_date_time())
                                .map(|u| u.date_time),
                        }),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        let offset_to = offset_to?;
        Some(Self {
            start: start?,
            offset_from: offset_from.unwrap_or(offset_to),
            offset_to,
            rule,
        })
    }

    /// Transition times (local, before the change) in `year` and the year before.
    fn onsets(&self, year: i32) -> impl Iterator<Item = NaiveDateTime> + '_ {
        let ruled = self.rule.iter().flat_map(move |rule| {
            [year - 1, year].into_iter().filter_map(move |y| {
                let onset = nth_weekday(y, rule.month, rule.ordinal, rule.weekday)?
                    .and_time(self.start.time());
                let in_range = onset >= self.start && rule.until.is_none_or(|u| onset <= u);
                in_range.then_some(onset)
            })
        });
        let single = self.rule.is_none().then_some(self.start);
        ruled.chain(single)
    }
}

fn nth_weekday(year: i32, month: u32, ordinal: i16, weekday: Weekday) -> Option<NaiveDate> {
    if ordinal >= 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let shift = (i64::from(weekday.num_days_from_monday())
            - i64::from(first.weekday().num_days_from_monday()))
        .rem_euclid(DAYS_PER_WEEK);
        let weeks = i64::from(ordinal.max(1) - 1);
        let date = first + Duration::days(shift + weeks * DAYS_PER_WEEK);
        (date.month() == month).then_some(date)
    } else {
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let last = NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()?;
        let shift = (i64::from(last.weekday().num_days_from_monday())
            - i64::from(weekday.num_days_from_monday()))
        .rem_euclid(DAYS_PER_WEEK);
        let weeks = i64::from(ordinal.unsigned_abs() - 1);
        let date = last - Duration::days(shift + weeks * DAYS_PER_WEEK);
        (date.month() == month).then_some(date)
    }
}

fn utc_offset(dt: &calcard::common::PartialDateTime) -> Option<i32> {
    let seconds = i32::from(dt.tz_hour?) * SECONDS_PER_HOUR
        + i32::from(dt.tz_minute.unwrap_or(0)) * SECONDS_PER_MINUTE;
    Some(if dt.tz_minus { -seconds } else { seconds })
}

pub(crate) fn weekday(day: ICalendarWeekday) -> Weekday {
    match day {
        ICalendarWeekday::Monday => Weekday::Mon,
        ICalendarWeekday::Tuesday => Weekday::Tue,
        ICalendarWeekday::Wednesday => Weekday::Wed,
        ICalendarWeekday::Thursday => Weekday::Thu,
        ICalendarWeekday::Friday => Weekday::Fri,
        ICalendarWeekday::Saturday => Weekday::Sat,
        ICalendarWeekday::Sunday => Weekday::Sun,
    }
}

pub(crate) fn text<'a>(
    component: &'a ICalendarComponent,
    name: &ICalendarProperty,
) -> Option<&'a str> {
    component
        .property(name)
        .and_then(|entry| entry.values.first())
        .and_then(|value| value.as_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nth_weekday_handles_first_and_last() {
        let second_sunday_march = nth_weekday(2026, 3, 2, Weekday::Sun);
        let last_sunday_october = nth_weekday(2026, 10, -1, Weekday::Sun);
        let fifth_monday_february = nth_weekday(2026, 2, 5, Weekday::Mon);
        assert_eq!(second_sunday_march, NaiveDate::from_ymd_opt(2026, 3, 8));
        assert_eq!(last_sunday_october, NaiveDate::from_ymd_opt(2026, 10, 25));
        assert_eq!(fifth_monday_february, None);
    }
}
