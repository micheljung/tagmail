//! Full bootstrap (§3): the snapshot is imported into `replica.tmp/`, which
//! replaces the replica only once the stream ended complete. Not resumable.

use std::sync::Arc;

use reqwest::Method;
use tagmail_api_core::replica_dto::SNAPSHOT_FORMAT;
use tagmail_core::{CacheEpoch, ChangeCursor};

use crate::apply;
use crate::client::ServerClient;
use crate::db::{self, Layout, ReplicaMeta};
use crate::error::{Error, Result};
use crate::import::{RecordReader, Target, import};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub meta: ReplicaMeta,
    pub window_start: i64,
    pub records: u64,
}

/// Imports a snapshot into [`Layout::replica_tmp`]; install it with
/// [`Layout::install_tmp`] after closing the current replica. On error the
/// temporary copy is removed and the current replica is untouched.
pub async fn import_snapshot(
    client: &ServerClient,
    layout: &Layout,
    window_days: u32,
) -> Result<Snapshot> {
    let tmp = layout.replica_tmp();
    let (cache, config, blobs) = db::create(&tmp)?;
    let blobs = Arc::new(blobs);
    let result = async {
        let builder = client.request(
            Method::GET,
            &format!("api/replica/snapshot?windowDays={window_days}"),
        )?;
        let resp = client.send(builder).await?;
        let mut reader = RecordReader::new(resp.bytes_stream());
        let target = Target {
            cache: &cache,
            config: &config,
            blobs: &blobs,
        };
        let imported = import(&mut reader, &target, true).await?;
        let header = imported
            .header
            .ok_or_else(|| Error::Incomplete("snapshot without header".into()))?;
        if header.format != SNAPSHOT_FORMAT {
            return Err(Error::SnapshotFormat(header.format));
        }
        let server_epoch: CacheEpoch = parse(&header.epoch)?;
        let cursor: ChangeCursor = parse(&header.cursor)?;
        let meta = ReplicaMeta {
            snapshot_format: header.format,
            server_epoch,
            cursor,
        };
        cache
            .write(move |tx| {
                apply::derive_all_threads(tx)?;
                meta.write(tx)
            })
            .await?;
        Ok(Snapshot {
            meta,
            window_start: header.window_start,
            records: imported.records,
        })
    }
    .await;
    cache.close().await?;
    config.close().await?;
    drop(blobs);
    if result.is_err() {
        layout.discard_tmp()?;
    }
    result
}

fn parse<T: std::str::FromStr>(value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::Incomplete(format!("bad header value {value:?}")))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use tagmail_api_core::dto::{AccountDto, AccountStateDto, ApiVersion, TlsMode};
    use tagmail_api_core::replica_dto::{
        EndRecord, LocationRecord, MailboxRecord, MessageRowRecord, ReplicaRecord, SnapshotHeader,
        TextRecord, ThreadRecord,
    };
    use tagmail_api_core::{Store, View};
    use tagmail_core::ThreadId;

    use super::*;
    use crate::client::tests::canned;
    use crate::db::{Expected, Opened};

    const EPOCH: &str = "00000000000000ab";

    fn message(id: i64, thread: i64, date: i64, subject: &str) -> MessageRowRecord {
        MessageRowRecord {
            id,
            account_id: 1,
            email_id: format!("o:M{id}"),
            msgid_hdr: None,
            in_reply_to: None,
            thread_id: Some(thread),
            date,
            subject_norm: subject.to_lowercase(),
            from_json: Some(r#"[{"name":"Ann","email":"ann@example.org"}]"#.into()),
            to_json: None,
            cc_json: None,
            subject: Some(subject.into()),
            snippet: None,
            size: 10,
            has_attachment: false,
            tombstoned_at: None,
            locations: vec![LocationRecord {
                mailbox_id: 10,
                uid: id,
                flags: 0,
                keywords: String::new(),
                modseq: None,
                srv_flags: None,
                srv_keywords: None,
                hidden: false,
            }],
            labels: vec![],
        }
    }

    fn snapshot(seq: i64, with_end: bool) -> &'static str {
        let text = zstd::encode_all("the quick brown fox".as_bytes(), 3).unwrap();
        let mut records = vec![
            ReplicaRecord::Header(SnapshotHeader {
                format: SNAPSHOT_FORMAT,
                api: ApiVersion { major: 1, minor: 1 },
                epoch: EPOCH.into(),
                cursor: format!("{EPOCH}:{seq}"),
                window_days: 90,
                window_start: 1_000,
                cache_schema: tagmail_store::CACHE_SCHEMA,
                config_schema: tagmail_store::CONFIG_SCHEMA,
            }),
            ReplicaRecord::Account(AccountDto {
                id: 1,
                name: "Work".into(),
                imap_host: "imap.example.org".into(),
                imap_port: 993,
                imap_tls: TlsMode::Tls,
                smtp_host: "smtp.example.org".into(),
                smtp_port: 465,
                smtp_tls: TlsMode::Tls,
                username: "ann".into(),
                max_connections: 2,
                email: Some("ann@example.org".into()),
                status: AccountStateDto::Active,
            }),
            ReplicaRecord::Mailbox(MailboxRecord {
                id: 10,
                account_id: 1,
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                special_use: None,
                uidvalidity: 1,
                keyword_capable: true,
                sync_state: "live".into(),
            }),
            ReplicaRecord::Thread(ThreadRecord {
                id: 100,
                account_id: 1,
                subject_norm: "hello".into(),
                last_date: 2_000,
                msg_count: 1,
                unread_count: 1,
                starred: false,
                merged_into: None,
            }),
            ReplicaRecord::Thread(ThreadRecord {
                id: 101,
                account_id: 1,
                subject_norm: "older".into(),
                last_date: 500,
                msg_count: 1,
                unread_count: 1,
                starred: false,
                merged_into: None,
            }),
            ReplicaRecord::Message(message(1000, 100, 2_000, "Hello")),
            ReplicaRecord::Message(message(1001, 101, 500, "Older")),
            ReplicaRecord::Text(TextRecord {
                message_id: 1000,
                zstd: base64::engine::general_purpose::STANDARD.encode(text),
            }),
        ];
        let mut end = EndRecord::default();
        for r in &records {
            if let Some(kind) = r.kind() {
                *end.counts.entry(kind.into()).or_default() += 1;
            }
        }
        if with_end {
            records.push(ReplicaRecord::End(end));
        }
        let body: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        Box::leak(body.into_boxed_str())
    }

    async fn open_store(
        layout: &Layout,
    ) -> (tagmail_api_core::sqlite_store::SqliteStore, ReplicaMeta) {
        match db::open(layout, Expected::current(), None).unwrap() {
            Opened::Ready(dbs) => dbs.into_store().await.unwrap(),
            Opened::NeedsBootstrap(why) => panic!("no replica: {why:?}"),
        }
    }

    #[tokio::test]
    async fn imports_a_snapshot_readable_through_the_store() {
        let (url, mut heads) = canned(vec![(200, snapshot(7, true))]).await;
        let client = ServerClient::new(&url, Some("t".into())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());

        let snap = import_snapshot(&client, &layout, 30).await.unwrap();
        layout.install_tmp().unwrap();

        assert_eq!(snap.meta.cursor.seq, 7);
        assert_eq!(snap.records, 7);
        assert!(heads.recv().await.unwrap().contains("windowDays=30"));
        let (store, meta) = open_store(&layout).await;
        assert_eq!(meta, snap.meta);
        let inbox = store.view_index(View::Inbox).await.unwrap();
        assert_eq!(inbox.ids, vec![ThreadId(100), ThreadId(101)]);
        let found = store
            .view_index(View::Search(tagmail_core::query::parse("fox").unwrap()))
            .await
            .unwrap();
        assert_eq!(found.ids, vec![ThreadId(100)]);
        let by_sender = store
            .view_index(View::Search(
                tagmail_core::query::parse("from:ann").unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(by_sender.ids.len(), 2);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_truncated_stream_keeps_the_old_replica() {
        let (url, _) = canned(vec![(200, snapshot(7, true)), (200, snapshot(9, false))]).await;
        let client = ServerClient::new(&url, None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        import_snapshot(&client, &layout, 90).await.unwrap();
        layout.install_tmp().unwrap();

        let err = import_snapshot(&client, &layout, 90).await.unwrap_err();

        assert!(matches!(err, Error::Incomplete(_)), "{err:?}");
        assert!(!layout.replica_tmp().exists());
        let (store, meta) = open_store(&layout).await;
        assert_eq!(meta.cursor.seq, 7);
        store.close().await.unwrap();
    }
}
