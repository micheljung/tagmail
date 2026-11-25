//! One CalDAV/IMAP user per calendar test: tests run in parallel and each
//! empties its user first, so a user is never shared.

/// Stalwart tests own an attendee/organizer pair: the attendee's `calN` and
/// `calN+1`.
pub const STALWART_ORGANIZER_OFFSET: u32 = 1;

pub const MAX_CAL_USER: u32 = 48;

/// (test fn, user). Extend with the next free `calN`; Stalwart provisions
/// `cal1`..`cal6` only.
pub static CAL_USERS: &[(&str, &str)] = &[
    ("stalwart_scheduling_delivers_exactly_one_reply", "cal1"),
    ("server_mode_apply_already_applied_is_noop", "cal3"),
    ("never_ingested_invite_falls_back_to_outbox_reply", "cal5"),
    ("rsvp_client_mode_sends_one_reply", "cal7"),
    ("default_trust_is_manual", "cal8"),
    ("edit_roundtrips_to_radicale", "cal9"),
    (
        "invitation_reports_stored_partstat_and_outdated_sequence",
        "cal10",
    ),
    (
        "advertised_but_undelivered_schedule_status_queues_reply",
        "cal11",
    ),
    ("rsvp_server_mode_queues_no_outbox", "cal12"),
    ("rsvp_412_rebases_partstat", "cal13"),
    ("put_412_server_wins_and_keeps_rejected_ics", "cal14"),
    (
        "client_mode_put_failure_shows_reply_sent_not_saved",
        "cal15",
    ),
    ("import_updates_existing_uid_after_lookup", "cal16"),
    ("incoming_reply_updates_attendee_partstat", "cal17"),
    ("spoofed_reply_sender_is_not_applied", "cal18"),
    (
        "forged_reply_without_authentication_results_is_manual",
        "cal19",
    ),
    (
        "invite_client_mode_sends_request_from_account_identity",
        "cal20",
    ),
    ("invite_server_mode_sends_no_outbox", "cal21"),
    (
        "time_change_bumps_sequence_and_sends_updated_request",
        "cal22",
    ),
    ("delete_sends_cancel", "cal23"),
    ("publish_import_puts_each_vevent_and_sends_no_mail", "cal24"),
    ("sync_collection_incremental", "cal25"),
    ("ctag_etag_fallback_when_sync_collection_disabled", "cal26"),
    ("cache_wipe_rebuilds_identical_href_etag_set", "cal27"),
    ("dav_radicale_suite", "cal28"),
];

pub fn user_of(test: &str) -> &'static str {
    CAL_USERS
        .iter()
        .find(|(t, _)| *t == test)
        .unwrap_or_else(|| panic!("{test} has no row in CAL_USERS"))
        .1
}

pub fn number_of(user: &str) -> u32 {
    user.strip_prefix("cal")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{user} is not a calN user"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn listed(file: &str) -> HashSet<String> {
        let path = format!("{}/../../test/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .lines()
            .filter_map(|l| l.split(':').next())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn cal_users_are_unique_and_provisioned() {
        let dovecot = listed("dovecot/users");
        let radicale = listed("radicale/users");
        let stalwart = std::fs::read_to_string(format!(
            "{}/../../test/stalwart/config.toml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut seen = HashSet::new();
        let mut tests = HashSet::new();
        for (test, user) in CAL_USERS {
            assert!(tests.insert(*test), "{test} listed twice");
            assert!(seen.insert(*user), "{user} used twice");
            let n = number_of(user);
            assert!((1..=MAX_CAL_USER).contains(&n), "{user} out of range");
            assert!(dovecot.contains(*user), "{user} missing in dovecot/users");
            assert!(radicale.contains(*user), "{user} missing in radicale/users");
        }
        for (test, user) in CAL_USERS.iter().filter(|(_, u)| number_of(u) <= 6) {
            let organizer = format!("cal{}", number_of(user) + STALWART_ORGANIZER_OFFSET);
            assert!(
                !seen.contains(organizer.as_str()),
                "{test}: organizer {organizer} is also a test user"
            );
            for name in [*user, organizer.as_str()] {
                assert!(
                    stalwart.contains(&format!("name = \"{name}\"")),
                    "{name} missing in stalwart config"
                );
            }
        }
    }
}
