//! Reads an NDJSON record stream (snapshot or rows) and applies it in
//! batches. A stream without its `end` line, or whose counts differ from
//! the records received, is incomplete.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use rusqlite::Connection;
use tagmail_api_core::replica_dto::{ReplicaRecord, SnapshotHeader};
use tagmail_core::Flags;
use tagmail_store::{BlobStore, CacheDb, ConfigDb};

use crate::apply;
use crate::error::{Error, Result};

/// Longest accepted line; text blobs are capped at 64 KiB before base64.
const MAX_LINE: usize = 16 * 1024 * 1024;
const BATCH: usize = 500;

type ByteStream<'a> = Pin<Box<dyn Stream<Item = std::result::Result<Vec<u8>, String>> + Send + 'a>>;

pub struct RecordReader<'a> {
    stream: ByteStream<'a>,
    buf: Vec<u8>,
    done: bool,
    counts: BTreeMap<String, u64>,
    ended: bool,
}

impl<'a> RecordReader<'a> {
    pub fn new<S, B, E>(stream: S) -> RecordReader<'a>
    where
        S: Stream<Item = std::result::Result<B, E>> + Send + 'a,
        B: AsRef<[u8]>,
        E: Display,
    {
        RecordReader {
            stream: Box::pin(stream.map(|chunk| {
                chunk
                    .map(|b| b.as_ref().to_vec())
                    .map_err(|e| e.to_string())
            })),
            buf: Vec::new(),
            done: false,
            counts: BTreeMap::new(),
            ended: false,
        }
    }

    /// The next record before `end`; `None` once `end` matched the counts.
    pub async fn next(&mut self) -> Result<Option<ReplicaRecord>> {
        if self.ended {
            return Ok(None);
        }
        let Some(line) = self.line().await? else {
            return Err(Error::Incomplete(
                "stream ended before its end record".into(),
            ));
        };
        let record: ReplicaRecord = serde_json::from_slice(&line)
            .map_err(|e| Error::Incomplete(format!("bad record: {e}")))?;
        if let ReplicaRecord::End(end) = &record {
            if end.counts != self.counts {
                return Err(Error::Incomplete(format!(
                    "counts {:?} != received {:?}",
                    end.counts, self.counts
                )));
            }
            self.ended = true;
            return Ok(None);
        }
        if let Some(kind) = record.kind() {
            *self.counts.entry(kind.to_owned()).or_default() += 1;
        }
        Ok(Some(record))
    }

    async fn line(&mut self) -> Result<Option<Vec<u8>>> {
        while let Some(line) = self.raw_line().await? {
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Ok(Some(line));
            }
        }
        Ok(None)
    }

    /// The next line without its `\n`, blank ones included.
    pub(crate) async fn raw_line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                return Ok(Some(self.buf.drain(..=pos).take(pos).collect()));
            }
            if self.buf.len() > MAX_LINE {
                return Err(Error::Incomplete("record line too long".into()));
            }
            if self.done {
                return Ok(None);
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(Error::Incomplete(e)),
                None => self.done = true,
            }
        }
    }
}

/// Where imported records go.
pub struct Target<'a> {
    pub cache: &'a CacheDb,
    pub config: &'a ConfigDb,
    pub blobs: &'a Arc<BlobStore>,
}

/// A message seen for the first time, unread and in INBOX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub message_id: i64,
    pub account_id: i64,
    pub thread_id: Option<i64>,
    pub subject: Option<String>,
    pub from_json: Option<String>,
}

#[derive(Debug, Default)]
pub struct Imported {
    pub header: Option<SnapshotHeader>,
    /// Threads whose `thread_mailbox` rows need deriving.
    pub threads: BTreeSet<i64>,
    pub arrivals: Vec<Arrival>,
    pub records: u64,
}

/// Applies every record of `reader`. A header is required first when
/// `snapshot`, refused otherwise.
pub async fn import(
    reader: &mut RecordReader<'_>,
    target: &Target<'_>,
    snapshot: bool,
) -> Result<Imported> {
    let mut out = Imported::default();
    if snapshot {
        match reader.next().await? {
            Some(ReplicaRecord::Header(header)) => out.header = Some(header),
            _ => return Err(Error::Incomplete("snapshot without header".into())),
        }
    }
    let mut config = Vec::new();
    let mut cache = Vec::new();
    while let Some(record) = reader.next().await? {
        out.records += 1;
        match record {
            ReplicaRecord::Header(_) => {
                return Err(Error::Incomplete("unexpected header".into()));
            }
            ReplicaRecord::End(_) => unreachable!("the reader consumes end"),
            ReplicaRecord::Account(_)
            | ReplicaRecord::Identity(_)
            | ReplicaRecord::LabelRegistry(_)
            | ReplicaRecord::ImageSender(_) => config.push(record),
            _ => cache.push(record),
        }
        if config.len() >= BATCH {
            flush_config(target.config, std::mem::take(&mut config)).await?;
        }
        if cache.len() >= BATCH {
            flush_cache(target, std::mem::take(&mut cache), &mut out).await?;
        }
    }
    flush_config(target.config, config).await?;
    flush_cache(target, cache, &mut out).await?;
    Ok(out)
}

async fn flush_config(config: &ConfigDb, records: Vec<ReplicaRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    config
        .write(move |tx| {
            for record in &records {
                match record {
                    ReplicaRecord::Account(a) => apply::put_account(tx, a)?,
                    ReplicaRecord::Identity(i) => apply::put_identity(tx, i)?,
                    ReplicaRecord::LabelRegistry(r) => apply::put_label_registry(tx, r)?,
                    ReplicaRecord::ImageSender(r) => apply::put_image_sender(tx, r)?,
                    _ => {}
                }
            }
            Ok(())
        })
        .await?;
    Ok(())
}

async fn flush_cache(
    target: &Target<'_>,
    records: Vec<ReplicaRecord>,
    out: &mut Imported,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let blobs = Arc::clone(target.blobs);
    let (threads, arrivals) = target
        .cache
        .write(move |tx| {
            let mut threads = BTreeSet::new();
            let mut arrivals = Vec::new();
            for record in &records {
                match record {
                    ReplicaRecord::Mailbox(m) => apply::put_mailbox(tx, m)?,
                    ReplicaRecord::Label(l) => apply::put_label(tx, l)?,
                    ReplicaRecord::Thread(t) => apply::put_thread(tx, t)?,
                    ReplicaRecord::Message(m) => {
                        let put = apply::put_message(tx, m)?;
                        threads.extend(m.thread_id);
                        threads.extend(put.left_thread);
                        if put.new && unread_in_inbox(tx, m)? {
                            arrivals.push(Arrival {
                                message_id: m.id,
                                account_id: m.account_id,
                                thread_id: m.thread_id,
                                subject: m.subject.clone(),
                                from_json: m.from_json.clone(),
                            });
                        }
                    }
                    ReplicaRecord::Text(t) => apply::put_text(tx, &blobs, t)?,
                    _ => {}
                }
            }
            Ok((threads, arrivals))
        })
        .await?;
    out.threads.extend(threads);
    out.arrivals.extend(arrivals);
    Ok(())
}

fn unread_in_inbox(
    conn: &Connection,
    m: &tagmail_api_core::replica_dto::MessageRowRecord,
) -> tagmail_store::Result<bool> {
    let seen = i64::from(Flags::SEEN.bits());
    let mut inbox =
        conn.prepare_cached("SELECT 1 FROM mailbox WHERE id = ?1 AND upper(name) = 'INBOX'")?;
    for l in &m.locations {
        if !l.hidden && l.flags & seen == 0 && inbox.exists([l.mailbox_id])? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn reader(chunks: Vec<&'static str>) -> RecordReader<'static> {
        RecordReader::new(stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, String>(c.as_bytes().to_vec())),
        ))
    }

    const IMAGE: &str = r#"{"t":"imageSender","address":"a@b"}"#;

    #[tokio::test]
    async fn lines_split_across_chunks() {
        let mut r = reader(vec![
            &IMAGE[..10],
            &IMAGE[10..],
            "\n{\"t\":\"end\",\"co",
            "unts\":{\"imageSender\":1}}\n",
        ]);
        assert!(matches!(
            r.next().await.unwrap(),
            Some(ReplicaRecord::ImageSender(_))
        ));
        assert!(r.next().await.unwrap().is_none());
        assert!(r.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_end_is_incomplete() {
        let mut r = reader(vec![IMAGE, "\n"]);
        r.next().await.unwrap();
        assert!(matches!(r.next().await, Err(Error::Incomplete(_))));
    }

    #[tokio::test]
    async fn count_mismatch_is_incomplete() {
        let mut r = reader(vec![
            IMAGE,
            "\n",
            r#"{"t":"end","counts":{"imageSender":2}}"#,
            "\n",
        ]);
        r.next().await.unwrap();
        assert!(matches!(r.next().await, Err(Error::Incomplete(_))));
    }

    #[tokio::test]
    async fn unknown_record_is_rejected() {
        let mut r = reader(vec![r#"{"t":"bogus"}"#, "\n"]);
        assert!(matches!(r.next().await, Err(Error::Incomplete(_))));
    }
}
