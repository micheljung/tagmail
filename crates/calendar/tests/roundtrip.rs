use tagmail_calendar::{EventFields, LocalTime, edit, fields, parse};
use time::macros::{date, datetime};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn lines(ics: &str) -> Vec<String> {
    ics.replace("\r\n ", "")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn current(ics: &[u8]) -> EventFields {
    fields(&parse(ics).unwrap()).unwrap()
}

#[test]
fn edit_preserves_valarms_and_x_props() {
    let stored = fixture("edit_rich.ics");
    let before = current(&stored);
    assert_eq!(before.alarms, [15]);
    assert_eq!(
        before.start,
        LocalTime::DateTime {
            local: datetime!(2026-12-10 09:00),
            zone: Some("Europe/Zurich".into())
        }
    );
    let renamed = EventFields {
        summary: Some("Planning".into()),
        ..before
    };
    let edited = edit(
        Some(&stored),
        "rich-1@example.com",
        &renamed,
        None,
        None,
        true,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    assert!(edited.significant);
    assert_eq!(edited.sequence, 4);

    let (old, new) = (
        lines(&String::from_utf8(stored).unwrap()),
        lines(&edited.ics),
    );
    let differing = |a: &[String], b: &[String]| -> Vec<String> {
        a.iter().filter(|l| !b.contains(l)).cloned().collect()
    };
    let name = |l: &String| l.split([':', ';']).next().unwrap().to_owned();
    let mut changed: Vec<String> = differing(&old, &new)
        .iter()
        .chain(&differing(&new, &old))
        .map(name)
        .collect();
    changed.sort();
    changed.dedup();
    assert_eq!(
        changed,
        ["DTSTAMP", "SEQUENCE", "SUMMARY"],
        "{}",
        edited.ics
    );
    assert_eq!(old.len(), new.len());
    assert!(new.contains(&"SUMMARY;LANGUAGE=de:Planning".to_owned()));
}

#[test]
fn reminders_only_change_neither_sequence_nor_dtstamp() {
    let stored = fixture("edit_rich.ics");
    let mut wanted = current(&stored);
    wanted.alarms = vec![60];
    let edited = edit(
        Some(&stored),
        "rich-1@example.com",
        &wanted,
        None,
        None,
        true,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    assert!(!edited.significant);
    assert_eq!(edited.sequence, 3);
    let after = current(edited.ics.as_bytes());
    assert_eq!(after.alarms, [60]);
    assert!(edited.ics.contains("ACTION:AUDIO"), "absolute alarms stay");
    assert!(edited.ics.contains("DTSTAMP:20261101T080000Z"));
}

#[test]
fn new_all_day_event_with_attendees_names_the_organizer() {
    let fields = EventFields {
        summary: Some("Offsite".into()),
        location: None,
        description: None,
        start: LocalTime::Date(date!(2026 - 12 - 14)),
        end: LocalTime::Date(date!(2026 - 12 - 16)),
        attendees: vec![tagmail_calendar::EditAttendee {
            address: "ann@example.com".into(),
            name: Some("Ann".into()),
            partstat: tagmail_calendar::ParticipationStatus::NeedsAction,
        }],
        alarms: vec![0],
    };
    let organizer = tagmail_calendar::EditOrganizer {
        address: "me@example.org".into(),
        name: Some("Me".into()),
    };
    let edited = edit(
        None,
        "new-1",
        &fields,
        Some(&organizer),
        Some(true),
        true,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    let ics = edited.ics.replace("\r\n ", "");
    assert!(ics.contains("DTSTART;VALUE=DATE:20261214"), "{ics}");
    assert!(ics.contains("DTEND;VALUE=DATE:20261216"), "{ics}");
    assert!(
        ics.contains("ORGANIZER;CN=Me:mailto:me@example.org"),
        "{ics}"
    );
    assert!(ics.contains("SCHEDULE-AGENT=CLIENT"), "{ics}");
    assert!(ics.contains("SEQUENCE:0"), "{ics}");
    let parsed = current(ics.as_bytes());
    assert_eq!(parsed.attendees.len(), 1);
    assert_eq!(parsed.alarms, [0]);
    assert_eq!(parsed.summary.as_deref(), Some("Offsite"));
}

#[test]
fn a_new_zone_gets_its_vtimezone_and_floating_stays_floating() {
    let at = |zone: Option<&str>, hour| LocalTime::DateTime {
        local: datetime!(2026-12-01 00:00).replace_hour(hour).unwrap(),
        zone: zone.map(str::to_owned),
    };
    let base = EventFields {
        summary: Some("Call".into()),
        location: None,
        description: None,
        start: at(Some("America/New_York"), 9),
        end: at(Some("America/New_York"), 10),
        attendees: Vec::new(),
        alarms: Vec::new(),
    };
    let now = datetime!(2026-11-28 12:00 UTC);
    let zoned = edit(None, "z-1", &base, None, None, true, now).unwrap();
    assert!(zoned.ics.contains("TZID:America/New_York"), "{}", zoned.ics);
    assert!(
        zoned
            .ics
            .contains("DTSTART;TZID=America/New_York:20261201T090000")
    );
    let floating = EventFields {
        start: at(None, 9),
        end: at(None, 10),
        ..base.clone()
    };
    let ics = edit(None, "f-1", &floating, None, None, true, now)
        .unwrap()
        .ics;
    assert!(ics.contains("DTSTART:20261201T090000\r\n"), "{ics}");
    let utc = EventFields {
        start: at(Some("UTC"), 9),
        end: at(Some("UTC"), 10),
        ..base
    };
    let ics = edit(None, "u-1", &utc, None, None, true, now).unwrap().ics;
    assert!(ics.contains("DTSTART:20261201T090000Z\r\n"), "{ics}");
}

#[test]
fn request_and_cancel_leave_out_reminders_and_scheduling_parameters() {
    let stored = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace("X-NUM-GUESTS=0", "SCHEDULE-AGENT=CLIENT")
        .replace(
            "ATTENDEE;CN=Ann",
            "ORGANIZER:mailto:me@example.org\r\nATTENDEE;CN=Bo:mailto:bo@example.com\r\nATTENDEE;CN=Ann",
        );
    let request = tagmail_calendar::request(stored.as_bytes()).unwrap();
    assert!(request.contains("METHOD:REQUEST"));
    assert!(!request.contains("VALARM") && !request.contains("SCHEDULE-AGENT"));
    assert!(request.contains("BEGIN:VTIMEZONE") && request.contains("X-EXAMPLE-CUSTOM"));

    let only = ["bo@example.com".to_owned()];
    let now = datetime!(2026-11-28 12:00 UTC);
    let cancel = tagmail_calendar::cancel(stored.as_bytes(), Some(&only), 4, now).unwrap();
    let cancel = cancel.replace("\r\n ", "");
    assert!(cancel.contains("METHOD:CANCEL") && cancel.contains("STATUS:CANCELLED"));
    assert!(cancel.contains("SEQUENCE:4") && cancel.contains("DTSTAMP:20261128T120000Z"));
    assert!(cancel.contains("mailto:bo@example.com") && !cancel.contains("ann@example.com"));
}

fn reply(rid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//EN\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\n\
         UID:rich-1@example.com\r\nDTSTAMP:20261102T120000Z\r\nSEQUENCE:3\r\n{rid}\
         ATTENDEE;PARTSTAT=DECLINED:mailto:ann@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

#[test]
fn a_reply_sets_that_attendees_partstat_once() {
    let stored = fixture("edit_rich.ics");
    let series = "DTSTART;TZID=Europe/Zurich:20261210T090000\r\n";
    let reply = |_: &str| reply(series);
    let accepted = reply("").replace("DECLINED", "ACCEPTED");
    assert_eq!(
        tagmail_calendar::apply_reply(&stored, accepted.as_bytes(), "ANN@example.com").unwrap(),
        None
    );
    let declined = tagmail_calendar::apply_reply(&stored, reply("").as_bytes(), "ann@example.com")
        .unwrap()
        .unwrap();
    assert!(declined.contains("PARTSTAT=DECLINED;X-NUM-GUESTS=0:mailto:ann@example.com"));
    assert!(declined.contains("SEQUENCE:3"));
    assert!(
        tagmail_calendar::apply_reply(&stored, reply("").as_bytes(), "eve@example.com").is_err()
    );
}

#[test]
fn a_reply_for_one_instance_gets_its_own_override() {
    let series = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace(
            "TRANSP:OPAQUE\r\n",
            "TRANSP:OPAQUE\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\n",
        );
    let rid = "RECURRENCE-ID;TZID=Europe/Zurich:20261217T090000\r\n";
    let instant = datetime!(2026-12-17 08:00 UTC);
    let one =
        tagmail_calendar::apply_reply(series.as_bytes(), reply(rid).as_bytes(), "ann@example.com")
            .unwrap()
            .unwrap();
    let parsed = parse(one.as_bytes()).unwrap();
    assert_eq!(parsed.events.len(), 2);
    let partstat = |at| {
        tagmail_calendar::attendee_partstat(one.as_bytes(), "ann@example.com", at).map(|(p, _)| p)
    };
    assert_eq!(
        partstat(None),
        Some(tagmail_calendar::ParticipationStatus::Accepted)
    );
    assert_eq!(
        partstat(Some(instant)),
        Some(tagmail_calendar::ParticipationStatus::Declined)
    );
    assert_eq!(
        tagmail_calendar::apply_reply(one.as_bytes(), reply(rid).as_bytes(), "ann@example.com")
            .unwrap(),
        None,
        "the override has it now"
    );
    let unfolded = one.replace("\r\n ", "");
    let made = &unfolded[unfolded.rfind("BEGIN:VEVENT").unwrap()..];
    assert!(
        made.contains("DTSTART;TZID=Europe/Zurich:20261217T090000"),
        "{made}"
    );
    assert!(made.contains("DURATION:PT1H30M"), "{made}");
    assert!(
        made.contains("DTSTAMP:20261101T080000Z") && made.contains("SEQUENCE:3"),
        "{made}"
    );
    assert!(!made.contains("RRULE") && !made.contains("DTEND"), "{made}");
    assert!(
        made.contains("TRIGGER:-PT15M") && made.contains("ACTION:AUDIO"),
        "{made}"
    );
    assert_eq!(
        unfolded.matches("X-WR-ALARMUID:alarm-1").count(),
        2,
        "my reminders stay on both"
    );

    let stray = "RECURRENCE-ID;TZID=Europe/Zurich:20261218T090000\r\n";
    let refused = tagmail_calendar::apply_reply(
        series.as_bytes(),
        reply(stray).as_bytes(),
        "ann@example.com",
    );
    assert!(refused.is_err());
    let off = Some(datetime!(2026-12-18 08:00 UTC));
    assert_eq!(
        tagmail_calendar::attendee_partstat(series.as_bytes(), "ann@example.com", off),
        None,
        "not an instance of the series"
    );
}

#[test]
fn moving_the_start_of_a_duration_event_writes_both_ends() {
    let stored = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace("DTEND;TZID=Europe/Zurich:20261210T103000", "DURATION:PT90M");
    let before = current(stored.as_bytes());
    assert_eq!(
        before.end,
        LocalTime::DateTime {
            local: datetime!(2026-12-10 10:30),
            zone: Some("Europe/Zurich".into())
        }
    );
    let moved = EventFields {
        start: LocalTime::DateTime {
            local: datetime!(2026-12-10 11:00),
            zone: Some("Europe/Zurich".into()),
        },
        end: LocalTime::DateTime {
            local: datetime!(2026-12-10 12:30),
            zone: Some("Europe/Zurich".into()),
        },
        ..before
    };
    let edited = edit(
        Some(stored.as_bytes()),
        "rich-1@example.com",
        &moved,
        None,
        None,
        true,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    assert!(!edited.ics.contains("DURATION"), "{}", edited.ics);
    assert!(
        edited
            .ics
            .contains("DTEND;TZID=Europe/Zurich:20261210T123000")
    );
    assert_eq!(current(edited.ics.as_bytes()).end, moved.end);
}

#[test]
fn stored_text_that_only_differs_in_white_space_is_kept() {
    let stored = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace(
            "DESCRIPTION:Agenda\\, Budget",
            "DESCRIPTION:Agenda\\, Budget\\n\\n",
        );
    let mut wanted = current(stored.as_bytes());
    assert!(wanted.description.as_deref().unwrap().ends_with('\n'));
    wanted.description = Some("Agenda, Budget".into());
    wanted.alarms = vec![30];
    let edited = edit(
        Some(stored.as_bytes()),
        "rich-1@example.com",
        &wanted,
        None,
        None,
        true,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    assert!(!edited.significant);
    assert_eq!(edited.sequence, 3);
    assert!(
        edited.ics.contains("DESCRIPTION:Agenda\\, Budget\\n\\n"),
        "{}",
        edited.ics
    );
}

#[test]
fn an_invited_event_edit_keeps_sequence_and_other_attendees() {
    let stored = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace(
            "ATTENDEE;CN=Ann",
            "ORGANIZER:mailto:boss@example.com\r\nATTENDEE:urn:uuid:room-4\r\nATTENDEE;CN=Ann",
        );
    let before = current(stored.as_bytes());
    assert_eq!(before.attendees.len(), 1, "a room is not a mail address");
    let renamed = EventFields {
        summary: Some("Mine".into()),
        ..before
    };
    let edited = edit(
        Some(stored.as_bytes()),
        "rich-1@example.com",
        &renamed,
        None,
        Some(false),
        false,
        datetime!(2026-11-28 12:00 UTC),
    )
    .unwrap();
    assert!(edited.significant);
    assert_eq!(edited.sequence, 3);
    assert!(edited.ics.contains("SEQUENCE:3") && edited.ics.contains("DTSTAMP:20261101T080000Z"));
    assert!(
        edited.ics.contains("ATTENDEE:urn:uuid:room-4"),
        "{}",
        edited.ics
    );
}

#[test]
fn repeating_events_with_exceptions_are_known() {
    let plain = parse(&fixture("edit_rich.ics")).unwrap();
    assert!(!tagmail_calendar::has_exceptions(&plain));
    let series = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace(
            "TRANSP:OPAQUE\r\n",
            "TRANSP:OPAQUE\r\nRRULE:FREQ=WEEKLY\r\nEXDATE;TZID=Europe/Zurich:20261217T090000\r\n",
        );
    assert!(tagmail_calendar::has_exceptions(
        &parse(series.as_bytes()).unwrap()
    ));
    let until = String::from_utf8(fixture("edit_rich.ics"))
        .unwrap()
        .replace(
            "TRANSP:OPAQUE\r\n",
            "TRANSP:OPAQUE\r\nRRULE:FREQ=WEEKLY;UNTIL=20261231T080000Z\r\n",
        );
    let until = parse(until.as_bytes()).unwrap();
    assert!(!tagmail_calendar::has_exceptions(&until) && tagmail_calendar::has_until(&until));
}

#[test]
fn moving_a_series_with_an_end_moves_its_until() {
    for (rule, moved) in [
        ("UNTIL=20261231T080000Z", "UNTIL=20261231T100000Z"),
        ("UNTIL=20261231", "UNTIL=20261231"),
    ] {
        let stored = String::from_utf8(fixture("edit_rich.ics"))
            .unwrap()
            .replace(
                "TRANSP:OPAQUE\r\n",
                &format!("TRANSP:OPAQUE\r\nRRULE:FREQ=WEEKLY;{rule}\r\n"),
            );
        let before = current(stored.as_bytes());
        let later = |h| LocalTime::DateTime {
            local: datetime!(2026-12-10 00:00).replace_hour(h).unwrap(),
            zone: Some("Europe/Zurich".into()),
        };
        let wanted = EventFields {
            start: later(11),
            end: later(12),
            ..before
        };
        let now = datetime!(2026-11-28 12:00 UTC);
        let edited = edit(
            Some(stored.as_bytes()),
            "rich-1@example.com",
            &wanted,
            None,
            None,
            true,
            now,
        )
        .unwrap();
        assert!(edited.ics.contains(moved), "{rule}: {}", edited.ics);
    }
}
