//! iTIP sender authentication (appointments plan §3.4) and the decision
//! whether an updated invitation is applied without asking (§4 M-C step
//! 3). Shared by `GET /api/messages/{id}/invitation` and the engine's
//! calendar runner, so the card says what the runner does.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use tagmail_calendar::{Event, EventTime, Method, Organizer, ParticipationStatus};

use crate::calendar_dto::{AutoApplyDto, SenderAuthDto};

const FROM: &[u8] = b"from";
const AUTHENTICATION_RESULTS: &[u8] = b"authentication-results";
const DMARC: &str = "dmarc";
const DKIM: &str = "dkim";
const PASS: &str = "pass";
const HEADER_FROM: &str = "header.from";
const HEADER_D: &str = "header.d";
const HEADER_I: &str = "header.i";
/// Names under these TLDs are missing from the PSL; their organizational
/// domain is their last [`RESERVED_DOMAIN_LABELS`] labels.
const RESERVED_TLDS: &[&str] = &["test", "localhost", "local", "internal", "invalid"];
const RESERVED_DOMAIN_LABELS: usize = 2;
/// Received messages a suggested authserv-id is read from.
pub const SUGGESTION_WINDOW: usize = 20;
/// Of [`SUGGESTION_WINDOW`], how many must carry the id topmost.
pub const SUGGESTION_MIN_MESSAGES: usize = 5;
/// Distinct sender domains among them, so one sender cannot seed it.
pub const SUGGESTION_MIN_DOMAINS: usize = 3;
/// The header fields sender authentication reads, as IMAP names them.
pub const SENDER_FIELDS: [&str; 2] = ["FROM", "AUTHENTICATION-RESULTS"];
/// Hex digits of a deterministic apply op id.
const APPLY_ID_LEN: usize = 32;
const MASTER: &str = "master";

/// The unfolded values of every `name` field in `headers`, in order.
fn fields(headers: &[u8], name: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in headers.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
            if let Some(value) = &mut current {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            continue;
        }
        out.extend(current.take());
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        if line[..colon].trim_ascii().eq_ignore_ascii_case(name) {
            current = Some(
                String::from_utf8_lossy(&line[colon + 1..])
                    .trim()
                    .to_owned(),
            );
        }
    }
    out.extend(current);
    out
}

/// The single `From` mailbox; `None` for no `From`, several `From`
/// fields, or a `From` with several mailboxes.
pub fn from_address(headers: &[u8]) -> Option<String> {
    if fields(headers, FROM).len() != 1 {
        return None;
    }
    let message = mail_parser::MessageParser::new().parse(headers)?;
    let [address] = crate::mime::addresses(message.from()).try_into().ok()?;
    Some(address.email)
}

fn domain_of(address: &str) -> Option<String> {
    let (_, domain) = address.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// The registrable domain of `domain` (RFC 7489 §3.2).
fn organizational(domain: &str) -> String {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.last().is_some_and(|tld| RESERVED_TLDS.contains(tld)) {
        let keep = labels.len().min(RESERVED_DOMAIN_LABELS);
        return labels[labels.len() - keep..].join(".");
    }
    psl::domain_str(&domain).map_or(domain.clone(), str::to_owned)
}

/// Removes RFC 5322 comments, which may hold anything; a quoted-pair
/// (`\x`) never opens or closes a comment or quoted string.
fn strip_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' if quoted || depth > 0 => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                quoted = !quoted;
                out.push(c);
            }
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// One `Authentication-Results` field (RFC 8601): its authserv-id and its
/// results as `(method, result, properties)`.
struct AuthResults {
    authserv_id: String,
    results: Vec<(String, String, HashMap<String, String>)>,
}

fn parse_results(value: &str) -> Option<AuthResults> {
    let value = strip_comments(value);
    let mut parts = value.split(';');
    let authserv_id = parts
        .next()?
        .split_whitespace()
        .next()?
        .trim_matches('"')
        .to_ascii_lowercase();
    let mut results = Vec::new();
    for resinfo in parts {
        let mut tokens = resinfo.split_whitespace();
        let Some((method, result)) = tokens.next().and_then(|t| t.split_once('=')) else {
            continue;
        };
        let method = method.split('/').next().unwrap_or(method);
        let props = tokens
            .filter_map(|t| t.split_once('='))
            .map(|(k, v)| {
                (
                    k.to_ascii_lowercase(),
                    v.trim_matches('"').to_ascii_lowercase(),
                )
            })
            .collect();
        results.push((
            method.to_ascii_lowercase(),
            result.to_ascii_lowercase(),
            props,
        ));
    }
    Some(AuthResults {
        authserv_id,
        results,
    })
}

/// The authserv-id of the topmost `Authentication-Results` field.
pub fn topmost_authserv_id(headers: &[u8]) -> Option<String> {
    fields(headers, AUTHENTICATION_RESULTS)
        .first()
        .and_then(|v| parse_results(v))
        .map(|r| r.authserv_id)
        .filter(|id| !id.is_empty())
}

/// Whether the topmost `Authentication-Results` field shows, under the
/// `trusted` authserv-id, DMARC pass for the From domain or DKIM pass
/// aligned with it. A topmost field of any other id fails: the trusted MTA
/// adds its field on top of everything a sender wrote, so anything above it
/// was forged on the way in.
pub fn domain_authenticated(headers: &[u8], from: &str, trusted: &str) -> bool {
    let Some(from_domain) = domain_of(from) else {
        return false;
    };
    let trusted = trusted.trim().to_ascii_lowercase();
    let Some(results) = fields(headers, AUTHENTICATION_RESULTS)
        .first()
        .and_then(|v| parse_results(v))
        .filter(|r| r.authserv_id == trusted)
    else {
        return false;
    };
    let org = organizational(&from_domain);
    results.results.iter().any(|(method, result, props)| {
        if result != PASS {
            return false;
        }
        match method.as_str() {
            DMARC => props.get(HEADER_FROM).is_some_and(|d| *d == from_domain),
            DKIM => props
                .get(HEADER_D)
                .cloned()
                .or_else(|| props.get(HEADER_I).and_then(|i| domain_of(i)))
                .is_some_and(|d| organizational(&d) == org),
            _ => false,
        }
    })
}

/// The authserv-id to offer the owner: topmost on at least
/// [`SUGGESTION_MIN_MESSAGES`] of `samples` (`(authserv-id, sender
/// domain)` of recent messages) from at least [`SUGGESTION_MIN_DOMAINS`]
/// sender domains.
pub fn suggestion(samples: &[(String, String)]) -> Option<String> {
    let mut by_id: HashMap<&str, (usize, HashSet<&str>)> = HashMap::new();
    for (id, domain) in samples.iter().take(SUGGESTION_WINDOW) {
        let entry = by_id.entry(id.as_str()).or_default();
        entry.0 += 1;
        entry.1.insert(domain.as_str());
    }
    by_id
        .into_iter()
        .filter(|(_, (n, domains))| {
            *n >= SUGGESTION_MIN_MESSAGES && domains.len() >= SUGGESTION_MIN_DOMAINS
        })
        .max_by_key(|(id, (n, _))| (*n, std::cmp::Reverse(*id)))
        .map(|(id, _)| id.to_owned())
}

/// The sender domain of a header block's single `From`.
pub fn sender_domain(headers: &[u8]) -> Option<String> {
    from_address(headers).as_deref().and_then(domain_of)
}

/// Whether `from` is the calendar user an iTIP message must come from:
/// the ORGANIZER of a REQUEST or CANCEL, a replying ATTENDEE. With
/// `strict`, only email addresses count, never the `EMAIL` parameter or an
/// iMIP alias (required for anything applied without asking); otherwise
/// those count too, so an Apple invitation is not shown as spoofed.
pub fn from_matches(method: Option<&Method>, event: &Event, from: &str, strict: bool) -> bool {
    match method {
        Some(Method::Reply) => event.attendees.iter().any(|a| {
            if strict {
                a.is_address(from)
            } else {
                a.matches(from)
            }
        }),
        _ => event.organizer.as_ref().is_some_and(|o| {
            if strict {
                o.is_address(from)
            } else {
                o.matches(from)
            }
        }),
    }
}

/// Plan §3.4 from the address checks: `matches` (lenient) and `strict`,
/// see [`from_matches`]; `None` when no event's method changes a calendar.
pub fn sender_auth_of(
    matches: Option<(bool, bool)>,
    headers: &[u8],
    trusted: Option<&str>,
) -> SenderAuthDto {
    let Some((matches, strict)) = matches else {
        return SenderAuthDto::UntrustedConfig;
    };
    if !matches {
        return SenderAuthDto::AddressMismatch;
    }
    let Some(trusted) = trusted else {
        return SenderAuthDto::UntrustedConfig;
    };
    let authenticated =
        from_address(headers).is_some_and(|from| domain_authenticated(headers, &from, trusted));
    if strict && authenticated {
        SenderAuthDto::Verified
    } else {
        SenderAuthDto::Unauthenticated
    }
}

/// Plan §3.4 for the events of one message. `events` are only those of
/// methods that change a calendar.
pub fn sender_auth<'a>(
    events: impl IntoIterator<Item = (Option<&'a Method>, &'a Event)>,
    headers: &[u8],
    trusted: Option<&str>,
) -> SenderAuthDto {
    let events: Vec<_> = events.into_iter().collect();
    let from = from_address(headers);
    let check = |strict: bool| {
        from.as_deref().is_some_and(|from| {
            events
                .iter()
                .all(|(m, e)| from_matches(*m, e, from, strict))
        })
    };
    let matches = (!events.is_empty()).then(|| (check(false), check(true)));
    sender_auth_of(matches, headers, trusted)
}

/// What the calendar holds of an invitation's UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub sequence: u32,
    /// Unix seconds.
    pub dtstamp: Option<i64>,
    pub cancelled: bool,
    /// My PARTSTAT, upper case; `None` when I am not an attendee.
    pub partstat: Option<String>,
    /// The ORGANIZER calendar address, without `mailto:`.
    pub organizer: Option<String>,
}

/// One incoming VEVENT's version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingVersion {
    pub sequence: u32,
    /// Unix seconds.
    pub dtstamp: Option<i64>,
}

/// Whether the stored copy already has this update (RFC 5546 §2.1.5):
/// same or higher SEQUENCE and a DTSTAMP not older, or, for a CANCEL, it is
/// cancelled.
pub fn already_applied(cancel: bool, incoming: IncomingVersion, stored: &StoredVersion) -> bool {
    if cancel && stored.cancelled {
        return true;
    }
    stored.sequence > incoming.sequence
        || (stored.sequence == incoming.sequence
            && match (stored.dtstamp, incoming.dtstamp) {
                (Some(s), Some(i)) => s >= i,
                (_, None) => true,
                (None, Some(_)) => false,
            })
}

fn responded(stored: &StoredVersion) -> bool {
    stored
        .partstat
        .as_deref()
        .is_some_and(|p| !p.eq_ignore_ascii_case("NEEDS-ACTION"))
}

/// A calendar address that is a mail address (no URI scheme).
fn is_mail_address(address: &str) -> bool {
    address.contains('@') && !address.contains(':')
}

/// The ORGANIZER of a UID's VEVENTs: the main one's, `None` when another
/// VEVENT names someone else.
pub fn uid_organizer<'a>(events: &[&'a Event]) -> Option<&'a Organizer> {
    let main = events
        .iter()
        .max_by_key(|e| (e.recurrence_id.is_none(), e.sequence, e.dtstamp))?;
    let organizer = main.organizer.as_ref()?;
    events
        .iter()
        .all(|e| {
            e.organizer
                .as_ref()
                .is_none_or(|o| o.address.eq_ignore_ascii_case(&organizer.address))
        })
        .then_some(organizer)
}

/// Whether `incoming` (the update's ORGANIZER) is the organizer the
/// calendar holds: both mail addresses, equal ignoring case.
pub fn same_organizer(stored: Option<&str>, incoming: Option<&Organizer>) -> bool {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => {
            is_mail_address(stored)
                && incoming.address_is_email
                && stored.eq_ignore_ascii_case(&incoming.address)
        }
        _ => false,
    }
}

/// Invitations received longer ago than this (by INTERNALDATE) are not
/// applied without asking.
pub const AUTO_APPLY_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

/// What decides whether an update is applied without asking; the card and
/// the calendar runner both read it, so they agree.
#[derive(Debug, Clone)]
pub struct UpdateCheck<'a> {
    pub method: Option<&'a Method>,
    pub incoming: IncomingVersion,
    pub organizer: Option<&'a Organizer>,
    /// Every incoming VEVENT of the UID is an override (RECURRENCE-ID).
    pub overrides_only: bool,
    /// Every instance has ended.
    pub past: bool,
    pub stored: Option<&'a StoredVersion>,
    pub sender: SenderAuthDto,
    /// INTERNALDATE, Unix seconds; `None` when unknown.
    pub received: Option<i64>,
    pub now: i64,
    /// The account has a calendar to write to.
    pub has_target: bool,
}

/// The answer of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateDecision {
    pub auto_apply: AutoApplyDto,
    /// The update names another organizer than the stored event.
    pub organizer_changed: bool,
}

/// `applied` when the calendar already has the update, `pending` when the
/// runner applies it (answered stored event, SEQUENCE ≥ and newer DTSTAMP,
/// same organizer, verified sender, recent, not past, a calendar to write
/// to, not only overrides), `manual` when only "Update calendar" does,
/// `none` when there is nothing to apply.
pub fn decide(check: &UpdateCheck<'_>) -> UpdateDecision {
    let decided = |auto_apply, organizer_changed| UpdateDecision {
        auto_apply,
        organizer_changed,
    };
    let cancel = match check.method {
        Some(Method::Request) => false,
        Some(Method::Cancel) => true,
        _ => return decided(AutoApplyDto::None, false),
    };
    let Some(stored) = check.stored else {
        return decided(AutoApplyDto::None, false);
    };
    let organizer_changed = !same_organizer(stored.organizer.as_deref(), check.organizer);
    if !check.overrides_only && already_applied(cancel, check.incoming, stored) {
        let applied = stored.sequence == check.incoming.sequence || cancel;
        return decided(
            if applied {
                AutoApplyDto::Applied
            } else {
                AutoApplyDto::None
            },
            organizer_changed,
        );
    }
    let recent = check
        .received
        .is_some_and(|r| r >= check.now - AUTO_APPLY_MAX_AGE_SECS);
    let automatic = !organizer_changed
        && responded(stored)
        && check.sender == SenderAuthDto::Verified
        && recent
        && !check.past
        && !check.overrides_only
        && check.has_target;
    decided(
        if automatic {
            AutoApplyDto::Pending
        } else {
            AutoApplyDto::Manual
        },
        organizer_changed,
    )
}

/// Whether every instance of `events` (one UID) has ended at `now`.
pub fn past(events: &[&Event], now: i64) -> bool {
    !events.iter().any(|e| e.recurrence.is_some())
        && events
            .iter()
            .all(|e| e.end.as_ref().unwrap_or(&e.start).instant.unix_timestamp() < now)
}

/// A RECURRENCE-ID as an apply id names it: the UTC instant
/// (`20261208T090000Z`), or the date of an all-day or the wall clock of a
/// floating one.
pub fn recurrence_key(t: &EventTime) -> String {
    let utc = if t.all_day || t.floating {
        t.instant
    } else {
        t.instant.to_offset(time::UtcOffset::UTC)
    };
    let date = format!(
        "{:04}{:02}{:02}",
        utc.year(),
        u8::from(utc.month()),
        utc.day()
    );
    if t.all_day {
        return date;
    }
    let zone = if t.floating { "" } else { "Z" };
    format!(
        "{date}T{:02}{:02}{:02}{zone}",
        utc.hour(),
        utc.minute(),
        utc.second()
    )
}

/// The partstat text the calendar stores.
pub fn partstat_text(partstat: &ParticipationStatus) -> String {
    match partstat {
        ParticipationStatus::NeedsAction => "NEEDS-ACTION".to_owned(),
        ParticipationStatus::Accepted => "ACCEPTED".to_owned(),
        ParticipationStatus::Declined => "DECLINED".to_owned(),
        ParticipationStatus::Tentative => "TENTATIVE".to_owned(),
        ParticipationStatus::Delegated => "DELEGATED".to_owned(),
        ParticipationStatus::Other(other) => other.to_ascii_uppercase(),
    }
}

/// The op id of a sync-time apply (plan §3.6), from iCalendar data only,
/// so a cache wipe or another copy of the same message derives it again.
/// A REPLY includes the replying attendee.
pub fn apply_op_id(
    account: i64,
    uid: &str,
    recurrence_id: Option<&str>,
    sequence: u32,
    dtstamp: Option<i64>,
    method: &str,
    attendee: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    let sequence = sequence.to_string();
    let dtstamp = dtstamp.map(|d| d.to_string()).unwrap_or_default();
    let account = account.to_string();
    let mut fields = vec![
        account.as_str(),
        uid,
        recurrence_id.unwrap_or(MASTER),
        sequence.as_str(),
        dtstamp.as_str(),
        method,
    ];
    if method.eq_ignore_ascii_case("REPLY") {
        fields.push(attendee.unwrap_or_default());
    }
    for field in fields {
        hasher.update(field.as_bytes());
        hasher.update([0]);
    }
    let mut id = String::with_capacity(APPLY_ID_LEN);
    for b in hasher.finalize() {
        let _ = write!(id, "{b:02x}");
    }
    id.truncate(APPLY_ID_LEN);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUSTED: &str = "mx.example.net";

    fn headers(from: &str, results: &[&str]) -> Vec<u8> {
        let mut out = String::new();
        for r in results {
            out.push_str(&format!("Authentication-Results: {r}\r\n"));
        }
        out.push_str(&format!("From: <{from}>\r\n"));
        out.into_bytes()
    }

    #[test]
    fn dmarc_pass_for_the_from_domain_verifies() {
        let h = headers(
            "boss@corp.example.com",
            &["mx.example.net;\r\n dmarc=pass (p=reject) header.from=corp.example.com"],
        );
        assert!(domain_authenticated(&h, "boss@corp.example.com", TRUSTED));
        assert!(!domain_authenticated(&h, "boss@other.example", TRUSTED));
    }

    #[test]
    fn dkim_pass_needs_alignment() {
        let aligned = headers(
            "boss@corp.example.com",
            &["mx.example.net; dkim=pass header.d=example.com header.s=s1"],
        );
        assert!(domain_authenticated(
            &aligned,
            "boss@corp.example.com",
            TRUSTED
        ));
        let unaligned = headers(
            "boss@corp.example.com",
            &["mx.example.net; dkim=pass header.d=mailer.example.org"],
        );
        assert!(!domain_authenticated(
            &unaligned,
            "boss@corp.example.com",
            TRUSTED
        ));
    }

    #[test]
    fn only_the_topmost_field_of_the_trusted_id_counts() {
        let forged_below = headers(
            "boss@example.com",
            &[
                "mx.example.net; dmarc=fail header.from=example.com",
                "mx.example.net; dmarc=pass header.from=example.com",
            ],
        );
        assert!(!domain_authenticated(
            &forged_below,
            "boss@example.com",
            TRUSTED
        ));
        let other_id = headers(
            "boss@example.com",
            &["evil.example; dmarc=pass header.from=example.com"],
        );
        assert!(!domain_authenticated(
            &other_id,
            "boss@example.com",
            TRUSTED
        ));
        assert_eq!(
            topmost_authserv_id(&other_id).as_deref(),
            Some("evil.example")
        );
    }

    #[test]
    fn suggestion_needs_enough_messages_and_domains() {
        let sample = |id: &str, d: &str| (id.to_owned(), d.to_owned());
        let one_domain: Vec<_> = (0..10).map(|_| sample(TRUSTED, "a.example")).collect();
        assert_eq!(suggestion(&one_domain), None);
        let mut spread: Vec<_> = [
            "a.example",
            "b.example",
            "c.example",
            "a.example",
            "b.example",
        ]
        .iter()
        .map(|d| sample(TRUSTED, d))
        .collect();
        spread.push(sample("forged.example", "c.example"));
        assert_eq!(suggestion(&spread).as_deref(), Some(TRUSTED));
        assert_eq!(suggestion(&spread[..4]), None);
    }

    #[test]
    fn a_forged_field_above_or_below_the_trusted_one_fails() {
        let pass = "mx.example.net; dmarc=pass header.from=example.com";
        let above = headers(
            "boss@example.com",
            &["evil.example; dmarc=pass header.from=example.com", pass],
        );
        assert!(!domain_authenticated(&above, "boss@example.com", TRUSTED));
        let topmost = headers("boss@example.com", &[pass, "evil.example; dmarc=fail"]);
        assert!(domain_authenticated(&topmost, "boss@example.com", TRUSTED));
    }

    #[test]
    fn quoted_pairs_do_not_end_comments() {
        assert_eq!(
            strip_comments(r#"a (x \) dmarc=pass) b "q\"(c)" d"#),
            r#"a  b "q\"(c)" d"#
        );
    }

    #[test]
    fn apply_ids_are_stable_and_name_the_instance_and_reply_attendee() {
        let id = |r, m, a| apply_op_id(1, "u1", r, 2, Some(10), m, a);
        assert_eq!(
            id(None, "REQUEST", None),
            id(None, "REQUEST", Some("x@example.com"))
        );
        assert_ne!(id(None, "REQUEST", None), id(None, "CANCEL", None));
        assert_ne!(
            id(None, "REQUEST", None),
            id(Some("20261208T090000Z"), "REQUEST", None)
        );
        assert_ne!(
            id(None, "REPLY", Some("a@example.com")),
            id(None, "REPLY", Some("b@example.com"))
        );
        assert_eq!(id(None, "REQUEST", None).len(), APPLY_ID_LEN);
        let zurich = EventTime {
            instant: time::macros::datetime!(2026-12-08 10:00 +1),
            tzid: Some("Europe/Zurich".into()),
            all_day: false,
            floating: false,
        };
        assert_eq!(recurrence_key(&zurich), "20261208T090000Z");
    }

    fn organizer(address: &str) -> Organizer {
        Organizer {
            address: address.into(),
            address_is_email: true,
            name: None,
            email: None,
        }
    }

    #[test]
    fn decide_needs_an_answered_newer_recent_update_from_the_same_verified_organizer() {
        const NOW: i64 = 10_000_000;
        let stored = StoredVersion {
            sequence: 1,
            dtstamp: Some(100),
            cancelled: false,
            partstat: Some("ACCEPTED".into()),
            organizer: Some("Boss@example.com".into()),
        };
        let boss = organizer("boss@example.com");
        let check = UpdateCheck {
            method: Some(&Method::Request),
            incoming: IncomingVersion {
                sequence: 2,
                dtstamp: Some(200),
            },
            organizer: Some(&boss),
            overrides_only: false,
            past: false,
            stored: Some(&stored),
            sender: SenderAuthDto::Verified,
            received: Some(NOW),
            now: NOW,
            has_target: true,
        };
        let auto = |c: &UpdateCheck<'_>| decide(c).auto_apply;
        assert_eq!(auto(&check), AutoApplyDto::Pending);
        for manual in [
            UpdateCheck {
                sender: SenderAuthDto::UntrustedConfig,
                ..check.clone()
            },
            UpdateCheck {
                received: Some(NOW - AUTO_APPLY_MAX_AGE_SECS - 1),
                ..check.clone()
            },
            UpdateCheck {
                past: true,
                ..check.clone()
            },
            UpdateCheck {
                overrides_only: true,
                ..check.clone()
            },
            UpdateCheck {
                has_target: false,
                ..check.clone()
            },
        ] {
            assert_eq!(auto(&manual), AutoApplyDto::Manual, "{manual:?}");
        }
        let unanswered = StoredVersion {
            partstat: Some("NEEDS-ACTION".into()),
            ..stored.clone()
        };
        assert_eq!(
            auto(&UpdateCheck {
                stored: Some(&unanswered),
                ..check.clone()
            }),
            AutoApplyDto::Manual
        );
        let thief = organizer("thief@evil.example");
        let takeover = decide(&UpdateCheck {
            organizer: Some(&thief),
            ..check.clone()
        });
        assert_eq!(
            (takeover.auto_apply, takeover.organizer_changed),
            (AutoApplyDto::Manual, true)
        );
        let same = IncomingVersion {
            sequence: 1,
            dtstamp: Some(100),
        };
        assert_eq!(
            auto(&UpdateCheck {
                incoming: same,
                ..check.clone()
            }),
            AutoApplyDto::Applied
        );
        assert_eq!(
            auto(&UpdateCheck {
                stored: None,
                ..check
            }),
            AutoApplyDto::None
        );
    }
}
