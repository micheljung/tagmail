//! The notification payload (contract §6): minimal, text only, and small
//! enough that the padded ciphertext fits a few 256-byte buckets.

use tagmail_api_core::Arrival;
use tagmail_api_core::push_dto::{PUSH_PAYLOAD_VERSION, PushKindDto, PushPayloadDto};

pub const MAX_FROM_CHARS: usize = 64;
pub const MAX_SUBJECT_CHARS: usize = 120;
/// JSON bytes before padding; larger is a bug.
pub const MAX_JSON: usize = 1024;
pub const INBOX_URL: &str = "/inbox";
pub const TEST_URL: &str = "/settings/notifications";
const ELLIPSIS: char = '…';

/// At most `max` chars, ending in "…" when cut; control characters become
/// spaces so JSON escaping cannot inflate the size.
fn clip(text: &str, max: usize) -> String {
    let clean: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= max {
        return clean;
    }
    let mut cut: String = clean.chars().take(max - 1).collect();
    cut.push(ELLIPSIS);
    cut
}

/// A push standing for `count` arrivals of one account, described by the
/// newest.
pub fn new_mail(newest: &Arrival, count: u32, preview: bool) -> PushPayloadDto {
    let thread = newest.thread_id.filter(|_| preview && count == 1);
    let from = newest.from.as_ref().filter(|_| preview).map(|a| {
        let name = a.name.as_deref().map(str::trim).unwrap_or_default();
        clip(
            if name.is_empty() { &a.email } else { name },
            MAX_FROM_CHARS,
        )
    });
    PushPayloadDto {
        v: PUSH_PAYLOAD_VERSION,
        kind: PushKindDto::NewMail,
        account_id: Some(newest.account_id),
        count: Some(count),
        thread_id: newest.thread_id.filter(|_| preview),
        from,
        subject: newest
            .subject
            .as_deref()
            .filter(|_| preview)
            .map(|s| clip(s, MAX_SUBJECT_CHARS)),
        url: thread.map_or_else(|| INBOX_URL.to_owned(), |id| format!("/thread/{id}")),
    }
}

/// A push without arrivals behind it, e.g. after the dispatcher lagged.
pub fn count_only(account_id: Option<i64>, count: u32) -> PushPayloadDto {
    PushPayloadDto {
        v: PUSH_PAYLOAD_VERSION,
        kind: PushKindDto::NewMail,
        account_id,
        count: Some(count),
        thread_id: None,
        from: None,
        subject: None,
        url: INBOX_URL.to_owned(),
    }
}

pub fn test() -> PushPayloadDto {
    PushPayloadDto {
        v: PUSH_PAYLOAD_VERSION,
        kind: PushKindDto::Test,
        account_id: None,
        count: None,
        thread_id: None,
        from: None,
        subject: None,
        url: TEST_URL.to_owned(),
    }
}

/// The plaintext to encrypt.
pub fn encode(payload: &PushPayloadDto) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(payload)?;
    anyhow::ensure!(
        json.len() <= MAX_JSON,
        "push payload of {} bytes",
        json.len()
    );
    Ok(json)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tagmail_api_core::dto::AddressDto;

    use super::*;

    fn arrival(name: Option<&str>, subject: &str) -> Arrival {
        Arrival {
            account_id: 1,
            message_id: 9,
            thread_id: Some(4711),
            from: Some(AddressDto {
                name: name.map(str::to_owned),
                email: "alice@example.com".into(),
            }),
            subject: Some(subject.into()),
            date: 0,
        }
    }

    fn value(p: &PushPayloadDto) -> serde_json::Value {
        serde_json::from_slice(&encode(p).unwrap()).unwrap()
    }

    #[test]
    fn single_arrival_links_its_thread() {
        let p = new_mail(&arrival(Some("Alice Example"), "Lunch?"), 1, true);
        assert_eq!(
            value(&p),
            json!({"v": 1, "kind": "newMail", "accountId": 1, "count": 1, "threadId": 4711,
                   "from": "Alice Example", "subject": "Lunch?", "url": "/thread/4711"})
        );
    }

    #[test]
    fn several_arrivals_open_the_inbox() {
        let p = new_mail(&arrival(None, "Lunch?"), 5, true);
        assert_eq!(p.url, INBOX_URL);
        assert_eq!(p.count, Some(5));
        assert_eq!(p.from.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn without_preview_sender_subject_and_thread_are_omitted() {
        let p = new_mail(&arrival(Some("Alice"), "Secret"), 1, false);
        assert_eq!(
            value(&p),
            json!({"v": 1, "kind": "newMail", "accountId": 1, "count": 1, "url": "/inbox"})
        );
    }

    #[test]
    fn long_texts_are_cut_at_char_boundaries() {
        let name = "é".repeat(100);
        let subject = format!("{}\u{0}tail", "😀".repeat(200));
        let p = new_mail(&arrival(Some(&name), &subject), 1, true);
        let from = p.from.unwrap();
        assert_eq!(from.chars().count(), MAX_FROM_CHARS);
        assert!(from.ends_with(ELLIPSIS));
        let subject = p.subject.unwrap();
        assert_eq!(subject.chars().count(), MAX_SUBJECT_CHARS);
        assert!(subject.ends_with(ELLIPSIS));
        assert_eq!(clip(" short ", MAX_FROM_CHARS), "short");
        assert_eq!(clip("a\u{7}b", MAX_FROM_CHARS), "a b");
    }

    #[test]
    fn worst_case_payload_fits_the_budget() {
        let worst = "\u{1}".repeat(500) + &"😀".repeat(500);
        let mut a = arrival(Some(&worst), &worst);
        a.account_id = i64::MAX;
        a.thread_id = Some(i64::MAX);
        let json = encode(&new_mail(&a, u32::MAX, true)).unwrap();
        assert!(json.len() <= MAX_JSON);
    }

    #[test]
    fn test_and_count_only_payloads() {
        assert_eq!(
            value(&test()),
            json!({"v": 1, "kind": "test", "url": "/settings/notifications"})
        );
        assert_eq!(
            value(&count_only(None, 70)),
            json!({"v": 1, "kind": "newMail", "count": 70, "url": "/inbox"})
        );
    }
}
