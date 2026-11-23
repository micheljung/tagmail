//! Identities (From addresses) and their signatures: validation of the
//! CRUD requests and the choice of the identity a reply is sent from.

use std::borrow::Cow;

use mail_parser::Message;
use tagmail_core::AccountId;

use crate::api::{Api, Response};
use crate::compose::{
    ComposeError, ComposeResult, error_response, json, parse_row_id, respond, valid_address,
};
use crate::compose_dto::{IdentityDto, IdentityPatchDto, NewIdentityDto};
use crate::store::{MessageRecord, Store, StoreError};

/// Largest sanitized signature.
pub const MAX_SIGNATURE: usize = 16 * 1024;
const MAX_NAME_LEN: usize = 200;
/// Signature images must be inline: remote ones are blocked by the
/// compose and render CSPs anyway.
const INLINE_IMAGE: &str = "data:image/";

/// A validated `POST /api/identities`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIdentity {
    pub account_id: AccountId,
    pub address: String,
    pub name: Option<String>,
    /// Sanitized HTML.
    pub signature: Option<String>,
    pub is_default: bool,
}

/// A validated `PATCH /api/identities/{id}`. `Some(None)` clears.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityPatch {
    pub address: Option<String>,
    pub name: Option<Option<String>>,
    /// Sanitized HTML.
    pub signature: Option<Option<String>>,
    /// Make this the account's default, demoting the previous one.
    pub make_default: bool,
}

fn invalid(field: &'static str, message: impl Into<String>) -> ComposeError {
    ComposeError::Invalid {
        field,
        message: message.into(),
    }
}

fn clean_address(address: &str) -> ComposeResult<String> {
    let address = address.trim();
    if valid_address(address) {
        Ok(address.to_owned())
    } else {
        Err(invalid("address", "not an email address"))
    }
}

/// Trimmed; empty is `None`.
fn clean_name(name: &str) -> ComposeResult<Option<String>> {
    let name = name.trim();
    if name.chars().count() > MAX_NAME_LEN || name.chars().any(char::is_control) {
        return Err(invalid(
            "name",
            format!("at most {MAX_NAME_LEN} printable characters"),
        ));
    }
    Ok((!name.is_empty()).then(|| name.to_owned()))
}

/// The signature as the compose policy keeps it, images only as `data:`
/// URIs; empty is `None`.
pub fn sanitize_signature(html: &str) -> ComposeResult<Option<String>> {
    let clean = ammonia::Builder::default()
        .add_generic_attributes(["style"])
        .add_url_schemes(["data"])
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("img", "src") => value
                .trim()
                .get(..INLINE_IMAGE.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(INLINE_IMAGE))
                .then(|| Cow::Owned(value.trim().to_owned())),
            _ => Some(Cow::Borrowed(value)),
        })
        .clean(html)
        .to_string();
    if clean.len() > MAX_SIGNATURE {
        return Err(invalid(
            "signature",
            format!("at most {} KiB", MAX_SIGNATURE / 1024),
        ));
    }
    Ok((!clean.trim().is_empty()).then_some(clean))
}

pub fn validate_new_identity(dto: NewIdentityDto) -> ComposeResult<NewIdentity> {
    Ok(NewIdentity {
        account_id: AccountId(dto.account_id),
        address: clean_address(&dto.address)?,
        name: dto.name.as_deref().map(clean_name).transpose()?.flatten(),
        signature: dto
            .signature
            .as_deref()
            .map(sanitize_signature)
            .transpose()?
            .flatten(),
        is_default: dto.is_default.unwrap_or(false),
    })
}

pub fn validate_identity_patch(dto: IdentityPatchDto) -> ComposeResult<IdentityPatch> {
    if dto.is_default == Some(false) {
        return Err(invalid(
            "isDefault",
            "make another identity the default instead",
        ));
    }
    Ok(IdentityPatch {
        address: dto.address.as_deref().map(clean_address).transpose()?,
        name: dto.name.as_deref().map(clean_name).transpose()?,
        signature: dto
            .signature
            .as_deref()
            .map(sanitize_signature)
            .transpose()?,
        make_default: dto.is_default == Some(true),
    })
}

/// Recipient headers of the original that To and Cc do not show, topmost
/// first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyHeaders {
    pub delivered_to: Vec<String>,
    pub x_original_to: Vec<String>,
}

fn header_addresses(message: &Message<'_>, name: &str) -> Vec<String> {
    message
        .headers()
        .all(name)
        .filter_map(|h| {
            let value = String::from_utf8_lossy(h.raw_value()).into_owned();
            let value = value.trim();
            let address = match (value.find('<'), value.rfind('>')) {
                (Some(start), Some(end)) if start < end => &value[start + 1..end],
                _ => value,
            };
            valid_address(address).then(|| address.to_owned())
        })
        .collect()
}

pub fn reply_headers(message: &Message<'_>) -> ReplyHeaders {
    ReplyHeaders {
        delivered_to: header_addresses(message, "Delivered-To"),
        x_original_to: header_addresses(message, "X-Original-To"),
    }
}

/// `me+tag@a` → `me@a`.
fn without_tag(address: &str) -> Cow<'_, str> {
    match address.rsplit_once('@') {
        Some((local, domain)) => match local.split_once('+') {
            Some((base, _)) if !base.is_empty() => Cow::Owned(format!("{base}@{domain}")),
            _ => Cow::Borrowed(address),
        },
        None => Cow::Borrowed(address),
    }
}

/// The identity a reply to `original` is sent from, among the identities
/// of the original's account only: the first one addressed by To, Cc,
/// Delivered-To, X-Original-To or the original's From (a reply to own
/// sent mail), matched exactly and then ignoring a `+tag`; else the
/// account's default.
pub fn choose_reply_identity<'a>(
    identities: &'a [IdentityDto],
    account_id: i64,
    original: &MessageRecord,
    headers: &ReplyHeaders,
) -> Option<&'a IdentityDto> {
    let own: Vec<&IdentityDto> = identities
        .iter()
        .filter(|i| i.account_id == account_id)
        .collect();
    let sources: Vec<&str> = original
        .to
        .iter()
        .chain(&original.cc)
        .map(|a| a.email.as_str())
        .chain(headers.delivered_to.iter().map(String::as_str))
        .chain(headers.x_original_to.iter().map(String::as_str))
        .chain(original.from.iter().map(|a| a.email.as_str()))
        .collect();
    let find = |normalize: fn(&str) -> Cow<'_, str>| {
        sources.iter().find_map(|s| {
            let s = normalize(s);
            own.iter()
                .find(|i| i.address.eq_ignore_ascii_case(&s))
                .copied()
        })
    };
    find(|s| Cow::Borrowed(s))
        .or_else(|| find(without_tag))
        .or_else(|| own.iter().find(|i| i.is_default).copied())
        .or_else(|| own.first().copied())
}

type Handled = Result<Response, StoreError>;

impl<S: Store> Api<S> {
    pub(crate) async fn create_identity(&self, body: &[u8]) -> Handled {
        let dto: NewIdentityDto = match json(body) {
            Ok(dto) => dto,
            Err(resp) => return Ok(resp),
        };
        let identity = match validate_new_identity(dto) {
            Ok(identity) => identity,
            Err(e) => return Ok(error_response(e)),
        };
        Ok(match self.composer() {
            Ok(c) => respond(201, c.create_identity(identity).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn update_identity(&self, id: &str, body: &[u8]) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("identity")));
        };
        let dto: IdentityPatchDto = match json(body) {
            Ok(dto) => dto,
            Err(resp) => return Ok(resp),
        };
        let patch = match validate_identity_patch(dto) {
            Ok(patch) => patch,
            Err(e) => return Ok(error_response(e)),
        };
        Ok(match self.composer() {
            Ok(c) => respond(200, c.update_identity(id, patch).await),
            Err(resp) => resp,
        })
    }

    pub(crate) async fn delete_identity(&self, id: &str) -> Handled {
        let Some(id) = parse_row_id(id) else {
            return Ok(error_response(ComposeError::NotFound("identity")));
        };
        Ok(match self.composer() {
            Ok(c) => match c.delete_identity(id).await {
                Ok(()) => Response::new(204, crate::api::JSON, Vec::new()),
                Err(e) => error_response(e),
            },
            Err(resp) => resp,
        })
    }
}

#[cfg(test)]
mod tests {
    use tagmail_core::{MessageRowId, ThreadId};

    use super::*;
    use crate::dto::AddressDto;

    fn addr(email: &str) -> AddressDto {
        AddressDto {
            name: None,
            email: email.to_owned(),
        }
    }

    fn identity(id: i64, account_id: i64, address: &str, is_default: bool) -> IdentityDto {
        IdentityDto {
            id,
            account_id,
            address: address.to_owned(),
            name: None,
            signature: None,
            is_default,
        }
    }

    fn record(from: &str, to: &[&str], cc: &[&str]) -> MessageRecord {
        MessageRecord {
            id: MessageRowId(7),
            account_id: AccountId(1),
            thread_id: ThreadId(3),
            date: 0,
            subject: None,
            snippet: String::new(),
            from: vec![addr(from)],
            to: to.iter().map(|a| addr(a)).collect(),
            cc: cc.iter().map(|a| addr(a)).collect(),
            unread: false,
            starred: false,
            has_attachment: false,
        }
    }

    #[test]
    fn reply_identity_follows_the_recipient_headers_in_order() {
        let ids = [
            identity(1, 1, "me@a.example", true),
            identity(2, 1, "alias@a.example", false),
            identity(3, 1, "list-me@a.example", false),
            identity(4, 2, "other@b.example", true),
        ];
        let headers = |delivered: &[&str], original: &[&str]| ReplyHeaders {
            delivered_to: delivered.iter().map(|a| a.to_string()).collect(),
            x_original_to: original.iter().map(|a| a.to_string()).collect(),
        };
        let none = headers(&[], &[]);
        let pick = |r: &MessageRecord, h: &ReplyHeaders| {
            choose_reply_identity(&ids, r.account_id.0, r, h).map(|i| i.id)
        };
        let cases: &[(&str, MessageRecord, ReplyHeaders, Option<i64>)] = &[
            (
                "To wins over Cc",
                record("x@y", &["x@y", "ALIAS@a.example"], &["me@a.example"]),
                none.clone(),
                Some(2),
            ),
            (
                "Cc only",
                record("x@y", &["list@l.example"], &["alias@a.example"]),
                none.clone(),
                Some(2),
            ),
            (
                "alias only in Delivered-To",
                record("x@y", &["list@l.example"], &[]),
                headers(&["alias@a.example", "me@a.example"], &[]),
                Some(2),
            ),
            (
                "X-Original-To after Delivered-To",
                record("x@y", &["list@l.example"], &[]),
                headers(&["nobody@a.example"], &["list-me@a.example"]),
                Some(3),
            ),
            (
                "+tag ignored on the second pass",
                record("x@y", &["alias+news@a.example"], &[]),
                none.clone(),
                Some(2),
            ),
            (
                "exact match beats a tagged earlier one",
                record("x@y", &["alias+news@a.example", "list-me@a.example"], &[]),
                none.clone(),
                Some(3),
            ),
            (
                "own sent mail",
                record("alias@a.example", &["bob@x"], &[]),
                none.clone(),
                Some(2),
            ),
            (
                "never another account's identity",
                record("x@y", &["other@b.example"], &[]),
                headers(&["other@b.example"], &[]),
                Some(1),
            ),
        ];
        for (what, r, h, want) in cases {
            assert_eq!(pick(r, h), *want, "{what}");
        }
        let mut foreign = record("x@y", &["me@a.example"], &[]);
        foreign.account_id = AccountId(9);
        assert_eq!(pick(&foreign, &none), None);
    }

    #[test]
    fn delivered_to_headers_are_read_topmost_first() {
        let raw = b"Delivered-To: alias@a.example\r\nDelivered-To: <me@a.example>\r\n\
                    X-Original-To: list-me@a.example\r\nSubject: s\r\n\r\nbody";
        let message = crate::mime::parse(raw).unwrap();
        assert_eq!(
            reply_headers(&message),
            ReplyHeaders {
                delivered_to: vec!["alias@a.example".into(), "me@a.example".into()],
                x_original_to: vec!["list-me@a.example".into()],
            }
        );
    }

    #[test]
    fn signatures_are_sanitized() {
        let clean = sanitize_signature(
            "<p style=\"color:red\">Ann</p><script>alert(1)</script>\
             <img src=\"https://t.example/pixel.gif\"><img src=\"data:image/png;base64,AAAA\">\
             <a href=\"javascript:alert(1)\">x</a>",
        )
        .unwrap()
        .unwrap();
        assert!(clean.contains("<p style=\"color:red\">Ann</p>"), "{clean}");
        assert!(
            clean.contains("src=\"data:image/png;base64,AAAA\""),
            "{clean}"
        );
        for gone in ["script", "alert", "t.example", "javascript"] {
            assert!(!clean.contains(gone), "{gone} in {clean}");
        }
        assert_eq!(sanitize_signature("").unwrap(), None);
        assert_eq!(sanitize_signature("<script>x</script>").unwrap(), None);
        assert!(matches!(
            sanitize_signature(&"a".repeat(MAX_SIGNATURE + 1)),
            Err(ComposeError::Invalid {
                field: "signature",
                ..
            })
        ));
    }

    #[test]
    fn identity_requests_are_validated() {
        let new = validate_new_identity(NewIdentityDto {
            account_id: 2,
            address: " b2@b.example ".into(),
            name: Some("  ".into()),
            signature: Some("".into()),
            is_default: None,
        })
        .unwrap();
        assert_eq!(
            new,
            NewIdentity {
                account_id: AccountId(2),
                address: "b2@b.example".into(),
                name: None,
                signature: None,
                is_default: false,
            }
        );
        let bad = NewIdentityDto {
            account_id: 2,
            address: "b2".into(),
            name: None,
            signature: None,
            is_default: None,
        };
        assert!(matches!(
            validate_new_identity(bad),
            Err(ComposeError::Invalid {
                field: "address",
                ..
            })
        ));
        let patch = validate_identity_patch(IdentityPatchDto {
            name: Some(String::new()),
            signature: Some(String::new()),
            is_default: Some(true),
            ..IdentityPatchDto::default()
        })
        .unwrap();
        assert_eq!(
            patch,
            IdentityPatch {
                address: None,
                name: Some(None),
                signature: Some(None),
                make_default: true,
            }
        );
        assert!(
            validate_identity_patch(IdentityPatchDto {
                is_default: Some(false),
                ..IdentityPatchDto::default()
            })
            .is_err()
        );
    }
}
