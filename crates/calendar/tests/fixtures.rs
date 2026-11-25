use tagmail_calendar::{
    Calendar, EventError, EventStatus, Frequency, Method, ParseError, ParticipationRole,
    ParticipationStatus, WeekdayNum, parse,
};
use time::macros::datetime;
use time::{OffsetDateTime, Weekday};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn calendar(name: &str) -> Calendar {
    parse(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn starts(calendar: &Calendar, from: OffsetDateTime, to: OffsetDateTime) -> Vec<OffsetDateTime> {
    calendar
        .occurrences(from..to)
        .into_iter()
        .map(|o| o.start)
        .collect()
}

#[test]
fn google_request() {
    let cal = calendar("google_request.ics");
    assert_eq!(cal.method, Some(Method::Request));
    let [event] = cal.events.as_slice() else {
        panic!("one event expected")
    };
    assert_eq!(event.uid, "7kq3hb2v0c8n1m5p4r6s9t0u2w@google.com");
    assert_eq!(event.sequence, 0);
    assert_eq!(event.dtstamp, Some(datetime!(2026-11-24 15:30:12 UTC)));
    assert_eq!(event.summary.as_deref(), Some("Quarterly planning"));
    assert_eq!(
        event.location.as_deref(),
        Some("Meeting room 3, Example HQ")
    );
    assert!(
        event
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("Quarterly planning, bring your numbers.\n\nJoin"))
    );
    assert_eq!(event.start.instant, datetime!(2026-12-08 09:00 UTC));
    assert_eq!(event.start.tzid.as_deref(), Some("Europe/Zurich"));
    assert!(!event.start.all_day && !event.start.floating);
    assert_eq!(
        event.end.as_ref().map(|end| end.instant),
        Some(datetime!(2026-12-08 10:00 UTC))
    );
    assert_eq!(event.status, Some(EventStatus::Confirmed));
    assert_eq!(event.recurrence, None);

    let organizer = event.organizer.as_ref().expect("organizer");
    assert_eq!(organizer.address, "alice@example.com");
    assert_eq!(organizer.name.as_deref(), Some("Alice Example"));

    assert_eq!(event.attendees.len(), 3);
    let bob = &event.attendees[0];
    assert_eq!(bob.address, "bob@example.org");
    assert_eq!(bob.partstat, ParticipationStatus::NeedsAction);
    assert_eq!(bob.role, ParticipationRole::ReqParticipant);
    assert!(bob.rsvp);
    let carol = &event.attendees[2];
    assert_eq!(carol.name.as_deref(), Some("Carol Example"));
    assert_eq!(carol.partstat, ParticipationStatus::Tentative);
    assert_eq!(carol.role, ParticipationRole::OptParticipant);
}

#[test]
fn google_updated_request_has_higher_sequence_and_new_time() {
    let original = calendar("google_request.ics");
    let updated = calendar("google_request_updated.ics");
    let (original, updated) = (&original.events[0], &updated.events[0]);
    assert_eq!(updated.uid, original.uid);
    assert_eq!(updated.sequence, 1);
    assert!(updated.dtstamp > original.dtstamp);
    assert_eq!(updated.start.instant, datetime!(2026-12-09 13:00 UTC));
    assert_eq!(
        updated.end.as_ref().map(|end| end.instant),
        Some(datetime!(2026-12-09 14:30 UTC))
    );
}

#[test]
fn google_cancel() {
    let cal = calendar("google_cancel.ics");
    assert_eq!(cal.method, Some(Method::Cancel));
    let event = &cal.events[0];
    assert_eq!(event.uid, "7kq3hb2v0c8n1m5p4r6s9t0u2w@google.com");
    assert_eq!(event.sequence, 2);
    assert_eq!(event.status, Some(EventStatus::Cancelled));
}

#[test]
fn outlook_teams_resolves_windows_zone() {
    let cal = calendar("outlook_teams.ics");
    assert_eq!(cal.method, Some(Method::Request));
    let event = &cal.events[0];
    assert!(event.uid.starts_with("040000008200E00074C5B7101A82E008"));
    assert_eq!(event.summary.as_deref(), Some("Migration sync"));
    assert_eq!(event.location.as_deref(), Some("Microsoft Teams Meeting"));
    assert_eq!(event.start.tzid.as_deref(), Some("W. Europe Standard Time"));
    assert!(!event.start.floating);
    assert_eq!(event.start.instant, datetime!(2026-12-15 13:30 UTC));
    assert_eq!(
        event.end.as_ref().map(|end| end.instant),
        Some(datetime!(2026-12-15 14:15 UTC))
    );
    assert!(
        event
            .description
            .as_deref()
            .is_some_and(|d| d.contains("Microsoft Teams meeting"))
    );
    assert!(
        event
            .alt_description
            .as_deref()
            .is_some_and(|d| d.starts_with("<html>") && d.contains("Click here to join"))
    );
    assert_eq!(event.attendees.len(), 3);
    assert_eq!(event.attendees[1].name.as_deref(), Some("Erin Placeholder"));
    assert_eq!(event.attendees[2].role, ParticipationRole::NonParticipant);
    assert_eq!(event.attendees[2].partstat, ParticipationStatus::Accepted);
}

#[test]
fn outlook_custom_zone_uses_vtimezone_observances_across_dst() {
    let cal = calendar("outlook_custom_tz.ics");
    let event = &cal.events[0];
    assert_eq!(event.start.tzid.as_deref(), Some("Customized Time Zone"));
    assert!(!event.start.floating);
    assert_eq!(event.start.instant, datetime!(2026-10-20 14:00 UTC));

    let recurrence = event.recurrence.as_ref().expect("recurrence");
    assert_eq!(recurrence.frequency, Frequency::Weekly);
    assert_eq!(
        recurrence.by_day,
        vec![WeekdayNum {
            ordinal: None,
            weekday: Weekday::Tuesday
        }]
    );
    assert_eq!(
        recurrence.until.as_ref().map(|until| until.instant),
        Some(datetime!(2026-11-17 15:00 UTC))
    );
    assert_eq!(
        recurrence
            .exdates
            .iter()
            .map(|e| e.instant)
            .collect::<Vec<_>>(),
        vec![datetime!(2026-11-03 15:00 UTC)]
    );

    assert_eq!(
        starts(
            &cal,
            datetime!(2026-10-01 0:00 UTC),
            datetime!(2026-12-01 0:00 UTC)
        ),
        vec![
            datetime!(2026-10-20 14:00 UTC),
            datetime!(2026-10-27 14:00 UTC),
            datetime!(2026-11-10 15:00 UTC),
            datetime!(2026-11-17 15:00 UTC),
        ]
    );
}

#[test]
fn apple_all_day() {
    let cal = calendar("apple_all_day.ics");
    let event = &cal.events[0];
    assert!(event.start.all_day);
    assert_eq!(event.start.instant, datetime!(2026-12-24 0:00 UTC));
    let end = event.end.as_ref().expect("end");
    assert!(end.all_day);
    assert_eq!(end.instant, datetime!(2026-12-27 0:00 UTC));
    assert_eq!(
        event.url.as_deref(),
        Some("https://example.com/family-visit")
    );
    assert_eq!(
        event.location.as_deref(),
        Some("Example Street 1\n8000 Zurich\nSwitzerland")
    );
    let organizer = event.organizer.as_ref().expect("organizer");
    assert_eq!(organizer.email.as_deref(), Some("grace@icloud.example"));
    assert!(organizer.matches("Grace@iCloud.example"));
    assert_eq!(event.attendees[0].role, ParticipationRole::Chair);
}

#[test]
fn apple_recurring_expands_over_dst_with_exdate() {
    let cal = calendar("apple_recurring.ics");
    let event = &cal.events[0];
    let recurrence = event.recurrence.as_ref().expect("recurrence");
    assert_eq!(recurrence.frequency, Frequency::Weekly);
    assert_eq!(recurrence.interval, 1);
    assert_eq!(recurrence.count, Some(6));
    assert_eq!(recurrence.exdates.len(), 1);

    let occurrences =
        cal.occurrences(datetime!(2026-10-01 0:00 UTC)..datetime!(2027-01-01 0:00 UTC));
    assert_eq!(
        occurrences.iter().map(|o| o.start).collect::<Vec<_>>(),
        vec![
            datetime!(2026-10-06 16:00 UTC),
            datetime!(2026-10-13 16:00 UTC),
            datetime!(2026-10-27 17:00 UTC),
            datetime!(2026-11-03 17:00 UTC),
            datetime!(2026-11-10 17:00 UTC),
        ]
    );
    assert!(
        occurrences
            .iter()
            .all(|o| o.end - o.start == time::Duration::minutes(90))
    );
    assert_eq!(
        starts(
            &cal,
            datetime!(2026-10-26 0:00 UTC),
            datetime!(2026-11-04 0:00 UTC)
        ),
        vec![
            datetime!(2026-10-27 17:00 UTC),
            datetime!(2026-11-03 17:00 UTC)
        ]
    );
}

#[test]
fn publish_with_multiple_events() {
    let cal = calendar("publish_multi.ics");
    assert_eq!(cal.method, Some(Method::Publish));
    let uids: Vec<_> = cal.events.iter().map(|e| e.uid.as_str()).collect();
    assert_eq!(
        uids,
        [
            "talk-001@conf.example",
            "talk-002@conf.example",
            "social-001@conf.example"
        ]
    );
    assert_eq!(
        cal.events[0].end.as_ref().map(|end| end.instant),
        Some(datetime!(2027-03-10 08:45 UTC))
    );
    assert!(cal.events[2].start.all_day);
    assert_eq!(cal.events[2].organizer, None);
    assert_eq!(
        cal.occurrences(datetime!(2027-03-10 0:00 UTC)..datetime!(2027-03-12 0:00 UTC))
            .len(),
        3
    );
}

#[test]
fn spoofed_organizer_does_not_match_sender() {
    let cal = calendar("spoofed_organizer.ics");
    let organizer = cal.events[0].organizer.as_ref().expect("organizer");
    assert_eq!(organizer.address, "Payroll@Bank.Example");
    assert!(organizer.matches("payroll@bank.example"));
    assert!(!organizer.matches("payroll@bank-example.invalid"));
    assert!(!organizer.matches("attacker@evil.example"));
}

#[test]
fn malformed_input_is_a_typed_error() {
    assert_eq!(
        parse(&fixture("malformed_not_ical.ics")),
        Err(ParseError::NotICalendar)
    );
    assert!(matches!(
        parse(&fixture("malformed_truncated.ics")),
        Err(ParseError::Malformed(_))
    ));
    assert_eq!(
        parse(&fixture("malformed_no_dtstart.ics")),
        Err(ParseError::NoEvents)
    );
    assert_eq!(
        parse(&fixture("malformed_bad_values.ics")),
        Err(ParseError::NoEvents)
    );
    assert_eq!(parse(b""), Err(ParseError::NotICalendar));
    assert_eq!(
        parse(b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n"),
        Err(ParseError::NoEvents)
    );
    assert_eq!(
        parse(
            b"BEGIN:VCALENDAR
BEGIN:VEVENT
DTSTART:20261202T130000Z
END:VEVENT
END:VCALENDAR
"
        ),
        Err(ParseError::NoEvents)
    );
}

#[test]
fn invalid_utf8_is_tolerated() {
    let mut input = fixture("google_request.ics");
    let at = input
        .windows(b"Quarterly planning".len())
        .position(|w| w == b"Quarterly planning")
        .expect("summary");
    input[at] = 0xFF;
    let cal = parse(&input).expect("lossy decoding");
    assert_eq!(cal.events[0].uid, "7kq3hb2v0c8n1m5p4r6s9t0u2w@google.com");
}

#[test]
fn round_trip_keeps_unknown_properties() {
    for name in [
        "google_request.ics",
        "outlook_teams.ics",
        "outlook_custom_tz.ics",
        "apple_all_day.ics",
        "apple_recurring.ics",
        "publish_multi.ics",
    ] {
        let source = String::from_utf8(fixture(name)).expect("utf-8 fixture");
        let cal = calendar(name);
        let output = cal.to_ics();
        for marker in [
            "X-GOOGLE-CONFERENCE",
            "X-NUM-GUESTS",
            "X-MICROSOFT-SKYPETEAMSPROPERTIES",
            "X-MICROSOFT-CDO-BUSYSTATUS",
            "X-ALT-DESC",
            "X-APPLE-STRUCTURED-LOCATION",
            "X-WR-ALARMUID",
            "X-WR-CALNAME",
            "BEGIN:VALARM",
            "BEGIN:VTIMEZONE",
        ] {
            assert_eq!(
                source.contains(marker),
                output.contains(marker),
                "{name}: {marker}"
            );
        }
        let reparsed = parse(output.as_bytes()).expect("reparse");
        assert_eq!(reparsed, cal, "{name}");
    }
}

#[test]
fn spoofed_email_param_does_not_override_mailto_organizer() {
    let cal = calendar("spoofed_organizer_email_param.ics");
    let event = &cal.events[0];
    let organizer = event.organizer.as_ref().expect("organizer");
    assert_eq!(organizer.address, "ceo@corp.example");
    assert!(organizer.address_is_email);
    assert_eq!(organizer.email.as_deref(), Some("x@evil.example"));
    assert!(!organizer.matches("x@evil.example"));
    assert!(organizer.matches("CEO@corp.example"));

    let bob = &event.attendees[0];
    assert!(bob.address_is_email);
    assert!(bob.matches("bob@example.org"));
    let room = &event.attendees[1];
    assert_eq!(
        room.address,
        "urn:uuid:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"
    );
    assert!(!room.address_is_email);
    assert!(room.matches("room-board@corp.example"));
    assert!(!room.matches("urn:uuid:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"));
}

#[test]
fn imip_alias_organizer_matches_its_email_param() {
    let cal = calendar("apple_all_day.ics");
    let organizer = cal.events[0].organizer.as_ref().expect("organizer");
    assert!(organizer.address_is_email);
    assert!(organizer.address.ends_with("@imip.me.com"));
    assert!(organizer.matches("grace@icloud.example"));
    assert!(!organizer.matches("attacker@evil.example"));
}

#[test]
fn unusable_events_are_skipped_not_fatal() {
    let cal = calendar("publish_partially_bad.ics");
    let uids: Vec<_> = cal.events.iter().map(|e| e.uid.as_str()).collect();
    assert_eq!(uids, ["match-002@club.example"]);
    assert_eq!(
        cal.skipped,
        vec![
            EventError::MissingUid { index: 0 },
            EventError::InvalidStart {
                uid: "match-003@club.example".to_string()
            },
        ]
    );
    assert_eq!(
        starts(
            &cal,
            datetime!(2027-01-01 0:00 UTC),
            datetime!(2027-03-01 0:00 UTC)
        ),
        vec![datetime!(2027-01-22 18:00 UTC)]
    );
}

#[test]
fn dense_rule_does_not_starve_other_series() {
    let cal = calendar("dense_rule_then_weekly.ics");
    let weekly: Vec<_> = cal
        .occurrences(datetime!(2026-11-30 0:00 UTC)..datetime!(2027-01-01 0:00 UTC))
        .into_iter()
        .filter(|o| o.event == 1)
        .map(|o| o.start)
        .collect();
    assert_eq!(
        weekly,
        vec![
            datetime!(2026-12-01 9:00 UTC),
            datetime!(2026-12-08 9:00 UTC),
            datetime!(2026-12-15 9:00 UTC),
        ]
    );
}

#[test]
fn schemeless_and_uppercase_mailto_addresses_are_authoritative() {
    let cal = calendar("spoofed_schemeless_email_param.ics");
    let event = &cal.events[0];

    let organizer = event.organizer.as_ref().expect("organizer");
    assert_eq!(organizer.address, "ceo@corp.example");
    assert!(organizer.address_is_email);
    assert!(!organizer.matches("x@evil.example"));
    assert!(organizer.matches("ceo@corp.example"));

    let finance = &event.attendees[0];
    assert_eq!(finance.address, "finance@corp.example");
    assert!(finance.address_is_email);
    assert!(!finance.matches("x@evil.example"));
    assert!(finance.matches("Finance@Corp.Example"));

    let bob = &event.attendees[1];
    assert_eq!(bob.address, "Bob@Example.org");
    assert!(bob.address_is_email);
    assert!(!bob.matches("x@evil.example"));
    assert!(bob.matches("bob@example.org"));
}
