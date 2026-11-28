use tagmail_calendar::{
    Calendar, Event, EventStatus, ItipError, Method, ParticipationStatus, apply_cancel,
    apply_update, for_import, parse, reply, schedule_status, set_my_partstat,
};
use time::macros::datetime;

const ME: &str = "bob@example.org";

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn calendar(ics: &[u8]) -> Calendar {
    parse(ics).unwrap_or_else(|e| panic!("{e}"))
}

fn my_partstat(event: &Event) -> &ParticipationStatus {
    &event
        .attendees
        .iter()
        .find(|a| a.is_address(ME))
        .expect("I am an attendee")
        .partstat
}

fn assert_reply_round_trip(name: &str, timezone: &str) {
    let request = calendar(&fixture(name));
    let original = &request.events[0];
    let now = datetime!(2026-11-25 12:00 UTC);
    let ics = reply(
        &request,
        &original.uid,
        None,
        "BOB@example.org",
        &ParticipationStatus::Accepted,
        now,
    )
    .unwrap();

    let answer = calendar(ics.as_bytes());
    assert_eq!(answer.method, Some(Method::Reply), "{name}");
    let [event] = answer.events.as_slice() else {
        panic!("{name}: one event expected")
    };
    assert_eq!(event.uid, original.uid);
    assert_eq!(event.sequence, original.sequence);
    assert_eq!(event.recurrence_id, None);
    assert_eq!(event.organizer, original.organizer);
    assert_eq!(event.dtstamp, Some(now));
    assert_eq!(event.start, original.start, "{name}: VTIMEZONE kept");
    let [me] = event.attendees.as_slice() else {
        panic!("{name}: only my ATTENDEE expected")
    };
    assert_eq!(me.address, ME);
    assert_eq!(me.partstat, ParticipationStatus::Accepted);
    assert!(!me.rsvp);
    assert!(ics.contains(&format!("TZID:{timezone}")), "{name}");
    assert!(!ics.contains("VALARM"), "{name}");
    assert!(!ics.contains("RSVP"), "{name}");
}

#[test]
fn google_request_reply_round_trip() {
    assert_reply_round_trip("google_request.ics", "Europe/Zurich");
}

#[test]
fn outlook_request_reply_round_trip() {
    assert_reply_round_trip("outlook_teams.ics", "W. Europe Standard Time");
}

#[test]
fn apple_request_reply_round_trip() {
    assert_reply_round_trip("apple_recurring.ics", "Europe/Zurich");
}

#[test]
fn reply_to_an_override_keeps_its_recurrence_id() {
    let request = calendar(&fixture("google_recurring_override.ics"));
    let instance = request.events[1].recurrence_id.clone();
    let ics = reply(
        &request,
        "weekly-standup-1@google.com",
        instance.as_ref(),
        ME,
        &ParticipationStatus::Declined,
        datetime!(2026-11-25 12:00 UTC),
    )
    .unwrap();

    let answer = calendar(ics.as_bytes());
    let [event] = answer.events.as_slice() else {
        panic!("one event expected")
    };
    assert_eq!(event.recurrence_id, instance);
    assert_eq!(event.summary.as_deref(), Some("Weekly standup (moved)"));
    assert_eq!(my_partstat(event), &ParticipationStatus::Declined);
}

#[test]
fn reply_needs_me_and_the_event() {
    let request = calendar(&fixture("google_request.ics"));
    let uid = request.events[0].uid.clone();
    let now = datetime!(2026-11-25 12:00 UTC);
    let accepted = ParticipationStatus::Accepted;
    assert_eq!(
        reply(&request, &uid, None, "eve@example.org", &accepted, now),
        Err(ItipError::NotAttendee {
            address: "eve@example.org".to_string()
        })
    );
    assert_eq!(
        reply(&request, "other", None, ME, &accepted, now),
        Err(ItipError::EventNotFound {
            uid: "other".to_string()
        })
    );
}

#[test]
fn me_never_matches_via_the_email_parameter() {
    let spoofed = fixture("spoofed_schemeless_email_param.ics");
    assert_eq!(
        set_my_partstat(
            &spoofed,
            "x@evil.example",
            &ParticipationStatus::Accepted,
            false
        ),
        Err(ItipError::NotAttendee {
            address: "x@evil.example".to_string()
        })
    );
    assert!(set_my_partstat(&spoofed, ME, &ParticipationStatus::Accepted, false).is_ok());
}

#[test]
fn set_my_partstat_keeps_unknown_properties() {
    let ics = set_my_partstat(
        &fixture("google_request.ics"),
        ME,
        &ParticipationStatus::Tentative,
        false,
    )
    .unwrap();

    let event = &calendar(ics.as_bytes()).events[0];
    assert_eq!(my_partstat(event), &ParticipationStatus::Tentative);
    assert_eq!(event.attendees[2].partstat, ParticipationStatus::Tentative);
    assert_eq!(event.attendees[1].partstat, ParticipationStatus::Accepted);
    for kept in [
        "X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij",
        "X-MICROSOFT-CDO-OWNERAPPTID:-1234567890",
        "X-NUM-GUESTS=0",
        "BEGIN:VALARM",
    ] {
        assert!(ics.contains(kept), "{kept}");
    }
    assert!(!ics.contains("SCHEDULE-AGENT"));
}

#[test]
fn set_my_partstat_in_client_mode_marks_organizer_and_me() {
    let ics = set_my_partstat(
        &fixture("google_request.ics"),
        ME,
        &ParticipationStatus::Accepted,
        true,
    )
    .unwrap();

    let unfolded = ics.replace("\r\n ", "");
    let organizer = unfolded
        .lines()
        .find(|line| line.starts_with("ORGANIZER"))
        .unwrap();
    assert!(organizer.contains("SCHEDULE-AGENT=CLIENT"));
    let marked: Vec<&str> = unfolded
        .lines()
        .filter(|line| line.starts_with("ATTENDEE") && line.contains("SCHEDULE-AGENT=CLIENT"))
        .collect();
    assert_eq!(marked.len(), 1);
    assert!(marked[0].ends_with("mailto:bob@example.org"));
}

#[test]
fn set_my_partstat_covers_overrides() {
    let ics = set_my_partstat(
        &fixture("google_recurring_override.ics"),
        ME,
        &ParticipationStatus::Accepted,
        false,
    )
    .unwrap();

    let stored = calendar(ics.as_bytes());
    assert_eq!(stored.events.len(), 2);
    for event in &stored.events {
        assert_eq!(my_partstat(event), &ParticipationStatus::Accepted);
    }
}

fn accepted_with_local_additions(name: &str) -> String {
    set_my_partstat(&fixture(name), ME, &ParticipationStatus::Accepted, true)
        .unwrap()
        .replacen(
            "SUMMARY:",
            "X-TAGMAIL-LOCAL;X-NOTE=mine:kept\r\nCATEGORIES:Work\r\nSUMMARY:",
            1,
        )
}

#[test]
fn apply_update_takes_the_request_and_keeps_my_answer_and_additions() {
    let stored = accepted_with_local_additions("google_request.ics");
    let ics = apply_update(
        stored.as_bytes(),
        &fixture("google_request_updated.ics"),
        ME,
    )
    .unwrap();

    let updated = calendar(ics.as_bytes());
    assert_eq!(updated.method, None);
    let [event] = updated.events.as_slice() else {
        panic!("one event expected")
    };
    assert_eq!(event.sequence, 1);
    assert_eq!(event.start.instant, datetime!(2026-12-09 13:00 UTC));
    assert_eq!(my_partstat(event), &ParticipationStatus::Accepted);
    for kept in [
        "X-TAGMAIL-LOCAL;X-NOTE=mine:kept",
        "CATEGORIES:Work",
        "X-GOOGLE-CONFERENCE",
        "BEGIN:VALARM",
        "TZID:Europe/Zurich",
    ] {
        assert!(ics.contains(kept), "{kept}");
    }
    assert_eq!(ics.matches("BEGIN:VTIMEZONE").count(), 1);
    assert_eq!(ics.matches("SCHEDULE-AGENT=CLIENT").count(), 2);
}

#[test]
fn apply_update_of_one_override_keeps_the_rest() {
    let stored = accepted_with_local_additions("google_recurring_override.ics");
    let request = String::from_utf8(fixture("google_recurring_override.ics")).unwrap();
    let master_start = request.find("BEGIN:VEVENT").unwrap();
    let override_start = request.rfind("BEGIN:VEVENT").unwrap();
    let only_override = format!("{}{}", &request[..master_start], &request[override_start..])
        .replace("Weekly standup (moved)", "Weekly standup (moved again)")
        .replace("SEQUENCE:0", "SEQUENCE:1");

    let ics = apply_update(stored.as_bytes(), only_override.as_bytes(), ME).unwrap();

    let updated = calendar(ics.as_bytes());
    let [master, instance] = updated.events.as_slice() else {
        panic!("master and override expected")
    };
    assert_eq!(master.summary.as_deref(), Some("Weekly standup"));
    assert_eq!(master.sequence, 0);
    assert_eq!(
        instance.summary.as_deref(),
        Some("Weekly standup (moved again)")
    );
    assert_eq!(instance.sequence, 1);
    assert_eq!(my_partstat(instance), &ParticipationStatus::Accepted);
}

#[test]
fn apply_update_for_another_uid_fails() {
    let stored = fixture("google_request.ics");
    assert_eq!(
        apply_update(&stored, &fixture("outlook_teams.ics"), ME),
        Err(ItipError::EventNotFound {
            uid: "7kq3hb2v0c8n1m5p4r6s9t0u2w@google.com".to_string()
        })
    );
}

#[test]
fn apply_cancel_of_the_series_keeps_the_event() {
    let stored = accepted_with_local_additions("google_request.ics");
    let ics = apply_cancel(stored.as_bytes(), &fixture("google_cancel.ics")).unwrap();

    let cancelled = calendar(ics.as_bytes());
    let event = &cancelled.events[0];
    assert_eq!(event.status, Some(EventStatus::Cancelled));
    assert_eq!(event.sequence, 2);
    assert_eq!(event.summary.as_deref(), Some("Quarterly planning"));
    assert_eq!(my_partstat(event), &ParticipationStatus::Accepted);
    assert!(ics.contains("X-TAGMAIL-LOCAL;X-NOTE=mine:kept"));
    assert!(ics.contains("BEGIN:VALARM"));
    assert_eq!(ics.matches("STATUS:").count(), 1);
}

#[test]
fn apply_cancel_of_one_instance_adds_a_cancelled_override() {
    let stored = accepted_with_local_additions("google_recurring_override.ics");
    let ics = apply_cancel(stored.as_bytes(), &fixture("google_cancel_instance.ics")).unwrap();

    let cancelled = calendar(ics.as_bytes());
    let [master, moved, instance] = cancelled.events.as_slice() else {
        panic!("master, override and the cancelled instance expected")
    };
    assert_eq!(master.status, Some(EventStatus::Confirmed));
    assert_eq!(moved.status, Some(EventStatus::Confirmed));
    assert_eq!(instance.status, Some(EventStatus::Cancelled));
    assert_eq!(instance.sequence, 1);
    assert_eq!(master.sequence, 0);
    assert_eq!(my_partstat(instance), &ParticipationStatus::Accepted);
    assert_eq!(instance.summary.as_deref(), Some("Weekly standup"));
    assert_eq!(instance.start.instant, datetime!(2026-12-22 09:00 UTC));
    assert_eq!(
        instance.end.as_ref().map(|end| end.instant),
        Some(datetime!(2026-12-22 10:00 UTC))
    );
    assert_eq!(instance.location.as_deref(), Some("Room 1"));
    assert_eq!(instance.recurrence, None);
    let starts: Vec<_> = cancelled
        .occurrences(datetime!(2026-12-01 0:00 UTC)..datetime!(2027-01-01 0:00 UTC))
        .into_iter()
        .map(|o| o.start)
        .collect();
    assert_eq!(starts.len(), 4, "{starts:?}");
}

#[test]
fn apply_cancel_of_an_existing_override_changes_only_it() {
    let stored = accepted_with_local_additions("google_recurring_override.ics");
    let cancel = String::from_utf8(fixture("google_cancel_instance.ics"))
        .unwrap()
        .replace("20261222T100000", "20261215T100000");
    let ics = apply_cancel(stored.as_bytes(), cancel.as_bytes()).unwrap();

    let cancelled = calendar(ics.as_bytes());
    let [master, moved] = cancelled.events.as_slice() else {
        panic!("master and override expected")
    };
    assert_eq!(master.status, Some(EventStatus::Confirmed));
    assert_eq!(moved.status, Some(EventStatus::Cancelled));
    assert_eq!(moved.summary.as_deref(), Some("Weekly standup (moved)"));
    assert_eq!(moved.sequence, 1);
    assert_eq!(master.sequence, 0);
    let starts: Vec<_> = cancelled
        .occurrences(datetime!(2026-12-01 0:00 UTC)..datetime!(2027-01-01 0:00 UTC))
        .into_iter()
        .map(|o| o.start)
        .collect();
    assert_eq!(
        starts,
        [
            datetime!(2026-12-08 9:00 UTC),
            datetime!(2026-12-15 10:00 UTC),
            datetime!(2026-12-22 9:00 UTC),
            datetime!(2026-12-29 9:00 UTC),
        ]
    );
}

#[test]
fn for_import_splits_a_publish_by_uid_without_method() {
    let objects = for_import(&fixture("publish_multi.ics")).unwrap();

    let uids: Vec<&str> = objects.iter().map(|(uid, _)| uid.as_str()).collect();
    assert_eq!(
        uids,
        [
            "talk-001@conf.example",
            "talk-002@conf.example",
            "social-001@conf.example"
        ]
    );
    for (uid, ics) in &objects {
        assert!(!ics.contains("METHOD"), "{uid}");
        assert!(ics.contains("X-WR-CALNAME:Example Conf 2027"), "{uid}");
        let object = calendar(ics.as_bytes());
        assert_eq!(object.method, None);
        let [event] = object.events.as_slice() else {
            panic!("{uid}: one event expected")
        };
        assert_eq!(&event.uid, uid);
    }
}

#[test]
fn for_import_keeps_overrides_and_their_timezones_together() {
    let objects = for_import(&fixture("google_recurring_override.ics")).unwrap();

    let [(uid, ics)] = objects.as_slice() else {
        panic!("one object expected")
    };
    assert_eq!(uid, "weekly-standup-1@google.com");
    assert!(!ics.contains("METHOD"));
    assert_eq!(ics.matches("BEGIN:VTIMEZONE").count(), 1);
    assert_eq!(calendar(ics.as_bytes()).events.len(), 2);
    assert!(ics.contains("X-GOOGLE-CONFERENCE"));
}

#[test]
fn schedule_status_reads_the_organizer_parameter() {
    let stored = fixture("stalwart_scheduled_copy.ics");
    assert_eq!(
        schedule_status(&stored, "Alice@Example.com")
            .unwrap()
            .as_deref(),
        Some("1.2")
    );
    assert_eq!(schedule_status(&stored, "eve@example.org").unwrap(), None);
    assert_eq!(
        schedule_status(&fixture("google_request.ics"), "alice@example.com").unwrap(),
        None
    );
}

#[test]
fn round_trips_keep_unknown_parameters_of_a_stored_copy() {
    let stored = fixture("stalwart_scheduled_copy.ics");
    let ics = set_my_partstat(&stored, ME, &ParticipationStatus::Declined, false).unwrap();
    assert!(ics.contains("X-TAGMAIL-UNKNOWN;X-PARAM=kept:value"));
    assert!(ics.contains("SCHEDULE-STATUS=1.2"));
    assert_eq!(
        my_partstat(&calendar(ics.as_bytes()).events[0]),
        &ParticipationStatus::Declined
    );
}
