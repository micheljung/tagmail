//! Expanding an object with many VEVENTs stays linear in its size: one shared
//! pass for single events, one masked copy at a time for recurring series.

use std::fmt::Write;
use std::time::{Duration, Instant};

use peak_alloc::PeakAlloc;
use tagmail_calendar::parse;
use time::macros::datetime;

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

const EVENTS: usize = 10_000;
const RECURRING_EVERY: usize = 50;
const WEEKLY_COUNT: usize = 4;
const MAX_PEAK_BYTES: usize = 24 << 20;
const MAX_DURATION: Duration = Duration::from_secs(10);

fn large_calendar() -> String {
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:PUBLISH\r\n");
    for i in 0..EVENTS {
        let day = i % 28 + 1;
        write!(
            ics,
            "BEGIN:VEVENT\r\nUID:event-{i}@example.com\r\nDTSTAMP:20261110T120000Z\r\n\
             DTSTART:202701{day:02}T090000Z\r\nDTEND:202701{day:02}T100000Z\r\n\
             SUMMARY:Event {i}\r\n"
        )
        .expect("write to String");
        if i % RECURRING_EVERY == 0 {
            write!(ics, "RRULE:FREQ=WEEKLY;COUNT={WEEKLY_COUNT}\r\n").expect("write to String");
        }
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ics
}

#[test]
fn ten_thousand_events_expand_within_bounds() {
    let ics = large_calendar();
    let calendar = parse(ics.as_bytes()).expect("parse");
    assert_eq!(calendar.events.len(), EVENTS);

    let before = ALLOC.current_usage();
    ALLOC.reset_peak_usage();
    let started = Instant::now();
    let occurrences =
        calendar.occurrences(datetime!(2027-01-01 0:00 UTC)..datetime!(2027-03-01 0:00 UTC));
    let elapsed = started.elapsed();
    let peak = ALLOC.peak_usage() - before;

    let recurring = EVENTS / RECURRING_EVERY;
    assert_eq!(
        occurrences.len(),
        EVENTS - recurring + recurring * WEEKLY_COUNT
    );
    assert!(peak < MAX_PEAK_BYTES, "peak {} KiB", peak / 1024);
    assert!(elapsed < MAX_DURATION, "took {elapsed:?}");
}
