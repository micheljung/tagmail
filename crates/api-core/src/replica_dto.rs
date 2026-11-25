//! Records of the replica routes (device tokens only): the NDJSON lines of
//! `GET /api/replica/snapshot` and `POST /api/replica/rows`, one JSON
//! object per line tagged by `t`. They carry the server's own rows and ids
//! so the desktop writes them into its copy of `cache.db` and `config.db`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::compose_dto::IdentityDto;
use crate::dto::{AccountDto, ApiVersion};

/// Bumped on any incompatible record change; the replica refuses others.
pub const SNAPSHOT_FORMAT: u32 = 1;
pub const NDJSON: &str = "application/x-ndjson";
pub const DEFAULT_WINDOW_DAYS: u32 = 90;
/// Ids per kind in one rows request.
pub const MAX_ROWS_IDS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ReplicaRecord {
    /// First line of a snapshot.
    Header(SnapshotHeader),
    Account(AccountDto),
    Identity(IdentityDto),
    LabelRegistry(LabelRegistryRecord),
    ImageSender(ImageSenderRecord),
    Mailbox(MailboxRecord),
    Label(LabelRecord),
    Thread(ThreadRecord),
    Message(MessageRowRecord),
    Text(TextRecord),
    /// Last line; a stream without it, or whose counts differ from the
    /// records received, is incomplete.
    End(EndRecord),
}

impl ReplicaRecord {
    /// Key of the record in [`EndRecord::counts`]; `None` for header/end.
    pub fn kind(&self) -> Option<&'static str> {
        Some(match self {
            ReplicaRecord::Header(_) | ReplicaRecord::End(_) => return None,
            ReplicaRecord::Account(_) => "account",
            ReplicaRecord::Identity(_) => "identity",
            ReplicaRecord::LabelRegistry(_) => "labelRegistry",
            ReplicaRecord::ImageSender(_) => "imageSender",
            ReplicaRecord::Mailbox(_) => "mailbox",
            ReplicaRecord::Label(_) => "label",
            ReplicaRecord::Thread(_) => "thread",
            ReplicaRecord::Message(_) => "message",
            ReplicaRecord::Text(_) => "text",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotHeader {
    pub format: u32,
    pub api: ApiVersion,
    /// The server's `cache_epoch`.
    pub epoch: String,
    /// `<epoch>:<seq>` the snapshot is exact at; pull `/api/changes` from
    /// here.
    pub cursor: String,
    pub window_days: u32,
    /// Unix seconds; messages from here on (and starred ones) carry text.
    pub window_start: i64,
    /// `user_version` of the server's `cache.db` and `config.db`.
    pub cache_schema: i64,
    pub config_schema: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelRegistryRecord {
    pub account_id: i64,
    pub label_key: String,
    pub display_name: String,
    pub color: Option<String>,
    pub sort: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSenderRecord {
    pub address: String,
}

/// Columns are the server's `mailbox` row; `name` stays modified UTF-7 and
/// `specialUse` the IMAP attribute (e.g. `\Archive`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxRecord {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub delimiter: Option<String>,
    pub special_use: Option<String>,
    pub uidvalidity: i64,
    pub keyword_capable: bool,
    /// `initial`, `catchup` or `live`.
    pub sync_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelRecord {
    pub id: i64,
    pub account_id: i64,
    pub keyword: Option<String>,
    pub label_key: String,
    /// `keyword` or `folder`.
    pub kind: String,
    pub mailbox_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRecord {
    pub id: i64,
    pub account_id: i64,
    pub subject_norm: String,
    pub last_date: i64,
    pub msg_count: i64,
    pub unread_count: i64,
    pub starred: bool,
    pub merged_into: Option<i64>,
}

/// A `message` row with its locations and labels. The address lists are
/// the stored JSON text, passed through unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRowRecord {
    pub id: i64,
    pub account_id: i64,
    pub email_id: String,
    pub msgid_hdr: Option<String>,
    pub in_reply_to: Option<String>,
    pub thread_id: Option<i64>,
    pub date: i64,
    pub subject_norm: String,
    pub from_json: Option<String>,
    pub to_json: Option<String>,
    pub cc_json: Option<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub size: i64,
    pub has_attachment: bool,
    pub tombstoned_at: Option<i64>,
    pub locations: Vec<LocationRecord>,
    /// `label.id`s of the message.
    pub labels: Vec<i64>,
}

/// A `location` row; `srvFlags`/`srvKeywords` are set while a pending op
/// overlays the server state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationRecord {
    pub mailbox_id: i64,
    pub uid: i64,
    pub flags: i64,
    pub keywords: String,
    pub modseq: Option<i64>,
    pub srv_flags: Option<i64>,
    pub srv_keywords: Option<String>,
    pub hidden: bool,
}

/// The stored text blob of a message within the window or starred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRecord {
    pub message_id: i64,
    /// Base64 of the zstd-compressed blob exactly as the server stores it.
    pub zstd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndRecord {
    /// Records per [`ReplicaRecord::kind`].
    pub counts: BTreeMap<String, u64>,
}

/// `POST /api/replica/rows`: the current records of these entities, in the
/// snapshot's order, then `end`. Ids that no longer exist are left out; a
/// merged thread id yields the thread it was merged into. `mailbox` adds
/// every thread and message of that mailbox (a `bulk` change); `uids`
/// narrows that to its locations in this inclusive range (the payload of a
/// `bulk` change that only added them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaRowsDto {
    #[serde(default)]
    pub accounts: Vec<i64>,
    #[serde(default)]
    pub identities: Vec<i64>,
    #[serde(default)]
    pub mailboxes: Vec<i64>,
    #[serde(default)]
    pub labels: Vec<i64>,
    #[serde(default)]
    pub threads: Vec<i64>,
    #[serde(default)]
    pub messages: Vec<i64>,
    #[serde(default)]
    pub mailbox: Option<i64>,
    #[serde(default)]
    pub uids: Option<[u32; 2]>,
    /// Text is sent for messages in this window or starred; default 90.
    #[serde(default)]
    pub window_days: Option<u32>,
}

/// `POST /api/replica/image`: a remote image of a message, fetched by the
/// server's image proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaImageDto {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{AccountStateDto, TlsMode};

    fn records() -> Vec<ReplicaRecord> {
        vec![
            ReplicaRecord::Header(SnapshotHeader {
                format: SNAPSHOT_FORMAT,
                api: ApiVersion { major: 1, minor: 1 },
                epoch: "00ab".into(),
                cursor: "00ab:42".into(),
                window_days: 90,
                window_start: 1_700_000_000,
                cache_schema: 3,
                config_schema: 7,
            }),
            ReplicaRecord::Account(AccountDto {
                id: 1,
                name: "Work".into(),
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                imap_tls: TlsMode::Tls,
                smtp_host: "smtp.example.com".into(),
                smtp_port: 465,
                smtp_tls: TlsMode::Tls,
                username: "me".into(),
                max_connections: 2,
                email: Some("me@example.com".into()),
                status: AccountStateDto::Active,
            }),
            ReplicaRecord::Identity(IdentityDto {
                id: 3,
                account_id: 1,
                address: "me@example.com".into(),
                name: None,
                signature: Some("<p>Me</p>".into()),
                is_default: true,
            }),
            ReplicaRecord::LabelRegistry(LabelRegistryRecord {
                account_id: 1,
                label_key: "work".into(),
                display_name: "Work".into(),
                color: Some("#aabbcc".into()),
                sort: None,
            }),
            ReplicaRecord::ImageSender(ImageSenderRecord {
                address: "news@example.com".into(),
            }),
            ReplicaRecord::Mailbox(MailboxRecord {
                id: 5,
                account_id: 1,
                name: "Archiv&APY-".into(),
                delimiter: Some("/".into()),
                special_use: Some("\\Archive".into()),
                uidvalidity: 7,
                keyword_capable: true,
                sync_state: "live".into(),
            }),
            ReplicaRecord::Label(LabelRecord {
                id: 9,
                account_id: 1,
                keyword: Some("work".into()),
                label_key: "work".into(),
                kind: "keyword".into(),
                mailbox_id: None,
            }),
            ReplicaRecord::Thread(ThreadRecord {
                id: 11,
                account_id: 1,
                subject_norm: "hi".into(),
                last_date: 1_700_000_100,
                msg_count: 1,
                unread_count: 1,
                starred: false,
                merged_into: None,
            }),
            ReplicaRecord::Message(MessageRowRecord {
                id: 21,
                account_id: 1,
                email_id: "e1".into(),
                msgid_hdr: Some("a@b".into()),
                in_reply_to: None,
                thread_id: Some(11),
                date: 1_700_000_100,
                subject_norm: "hi".into(),
                from_json: Some(r#"[{"email":"a@b"}]"#.into()),
                to_json: None,
                cc_json: None,
                subject: Some("Hi".into()),
                snippet: Some("hello".into()),
                size: 120,
                has_attachment: false,
                tombstoned_at: None,
                locations: vec![LocationRecord {
                    mailbox_id: 5,
                    uid: 4,
                    flags: 0,
                    keywords: "work".into(),
                    modseq: Some(17),
                    srv_flags: None,
                    srv_keywords: None,
                    hidden: false,
                }],
                labels: vec![9],
            }),
            ReplicaRecord::Text(TextRecord {
                message_id: 21,
                zstd: "KLUv/QBYAQAAaGk=".into(),
            }),
            ReplicaRecord::End(EndRecord {
                counts: BTreeMap::from([("message".into(), 1), ("text".into(), 1)]),
            }),
        ]
    }

    #[test]
    fn records_round_trip_as_tagged_lines() {
        for record in records() {
            let line = serde_json::to_string(&record).unwrap();
            assert!(!line.contains('\n'));
            let back: ReplicaRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(back, record, "{line}");
        }
    }

    #[test]
    fn tags_match_the_contract() {
        let tags: Vec<String> = records()
            .iter()
            .map(|r| {
                serde_json::to_value(r).unwrap()["t"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            tags,
            [
                "header",
                "account",
                "identity",
                "labelRegistry",
                "imageSender",
                "mailbox",
                "label",
                "thread",
                "message",
                "text",
                "end"
            ]
        );
        for record in records() {
            if let Some(kind) = record.kind() {
                assert_eq!(serde_json::to_value(&record).unwrap()["t"], kind);
            }
        }
    }

    #[test]
    fn unknown_record_types_are_rejected() {
        assert!(serde_json::from_str::<ReplicaRecord>(r#"{"t":"blob","id":1}"#).is_err());
        assert!(serde_json::from_str::<ReplicaRecord>(r#"{"id":1}"#).is_err());
    }

    fn round_trip<T>(value: T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value, "{json}");
    }

    #[test]
    fn server_responses_the_replica_reads_round_trip() {
        use crate::api::{API_VERSION, MIN_CLIENT};
        use crate::auth_dto::DeviceTokenDto;
        use crate::compose_dto::{
            AttachmentDto, DraftDto, OutboxDto, OutboxItemDto, OutboxStateDto, SendStatusDto,
        };
        use crate::dto::{ApiErrorDto, ChangeDto, ChangeOpDto, ChangesDto, EntityDto, VersionDto};
        use crate::ops_dto::{OpDto, OpKindDto, OpStatusDto};

        round_trip(VersionDto {
            api: API_VERSION,
            min_client: MIN_CLIENT,
            server: "0.1.0".into(),
            features: vec!["calendar".into()],
        });
        let old_server: VersionDto = serde_json::from_str(
            r#"{"api":{"major":1,"minor":3},"minClient":{"major":1,"minor":0},"server":"0.1.1"}"#,
        )
        .unwrap();
        assert!(old_server.features.is_empty());
        let change = |entity, op| ChangeDto {
            seq: 4,
            entity,
            id: 7,
            op,
            payload: Some(serde_json::json!({"lastDate": 1})),
        };
        round_trip(ChangesDto {
            changes: vec![
                change(EntityDto::Thread, ChangeOpDto::Upsert),
                change(EntityDto::Message, ChangeOpDto::Delete),
                change(EntityDto::Thread, ChangeOpDto::Merged { target: 3 }),
                change(EntityDto::LabelJob, ChangeOpDto::Bulk { mailbox: 2 }),
                change(EntityDto::Op, ChangeOpDto::Upsert),
            ],
            next: "00ab:4".into(),
            more: false,
        });
        round_trip(OpDto {
            op_id: "o-1".into(),
            seq: 1,
            account_id: 1,
            kind: OpKindDto::Move,
            status: OpStatusDto::Failed,
            error: Some("mailbox not found".into()),
            attempts: 1,
            thread_ids: vec![2],
            label: None,
            mailbox_id: None,
            created_at: 5,
        });
        let attachment = AttachmentDto {
            id: "3".into(),
            name: "a.txt".into(),
            size: 1,
            content_type: "text/plain".into(),
        };
        round_trip(DraftDto {
            id: 1,
            account_id: 1,
            identity_id: Some(2),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: "s".into(),
            text: Some("t".into()),
            html: None,
            in_reply_to_message_id: None,
            attachment_ids: vec!["3".into()],
            attachments: vec![attachment],
            version: 2,
            saved: false,
            save_error: None,
            queued: false,
        });
        round_trip(OutboxDto {
            items: vec![OutboxItemDto {
                id: 1,
                op_id: "s-1".into(),
                account_id: 1,
                draft_id: Some(1),
                subject: "s".into(),
                to: vec![],
                status: OutboxStateDto::Sending,
                attempts: 0,
                error: None,
                created_at: 1,
                sent_at: None,
            }],
        });
        round_trip(SendStatusDto {
            op_id: "s-1".into(),
            status: OutboxStateDto::Queued,
        });
        round_trip(ApiErrorDto {
            error: "gone".into(),
            message: "m".into(),
            field: None,
        });
        round_trip(ApiErrorDto {
            error: "invalid".into(),
            message: "m".into(),
            field: Some("mailbox".into()),
        });
        round_trip(DeviceTokenDto {
            device_id: 1,
            token: "t".into(),
        });
        round_trip(ReplicaImageDto {
            url: "https://example.com/a.png".into(),
        });
    }

    #[test]
    fn rows_requests_default_and_refuse_unknown_fields() {
        let rows: ReplicaRowsDto = serde_json::from_str(r#"{"threads":[1,2]}"#).unwrap();
        assert_eq!(
            rows,
            ReplicaRowsDto {
                threads: vec![1, 2],
                ..ReplicaRowsDto::default()
            }
        );
        assert!(serde_json::from_str::<ReplicaRowsDto>(r#"{"thread":[1]}"#).is_err());
    }
}
