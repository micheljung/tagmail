//! The sync window (§4): older unstarred messages keep their metadata but
//! lose text, body index and raw blob; in-window raw messages are fetched
//! ahead of time under a disk cap.

use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::Method;
use rusqlite::params;
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_core::Flags;
use tagmail_store::{BlobKey, BlobStore, DEFAULT_RAW_CAP};

use crate::apply;
use crate::client::ServerClient;
use crate::error::Result;

pub const DAY_SECS: i64 = 24 * 60 * 60;
pub const DEFAULT_RAW_CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const PREFETCH_CONCURRENCY: usize = 2;
const SWEEP_BATCH: i64 = 500;
const PREFETCH_BATCH: i64 = 50;

pub fn window_start(now: i64, window_days: u32) -> i64 {
    now - i64::from(window_days) * DAY_SECS
}

const STARRED: &str = "EXISTS (SELECT 1 FROM location l WHERE l.message_id = message.id \
     AND (l.flags & ?2) != 0)";

/// Drops text, body index and raw blob of messages before `start` that are
/// not starred. Returns how many messages were trimmed.
pub async fn sweep(store: &SqliteStore, start: i64) -> Result<u64> {
    let mut total = 0;
    loop {
        let blobs = Arc::clone(store.blobs());
        let trimmed = store
            .cache()
            .write(move |tx| {
                let ids: Vec<(i64, Option<String>, Option<String>)> = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT id, text_blob, raw_blob FROM message \
                         WHERE date < ?1 AND (text_blob IS NOT NULL OR raw_blob IS NOT NULL) \
                         AND NOT {STARRED} LIMIT ?3"
                    ))?;
                    stmt.query_map(params![start, Flags::FLAGGED.bits(), SWEEP_BATCH], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (id, text, raw) in &ids {
                    if let Some(text) = text {
                        blobs.delete_text(&BlobKey::parse(text)?)?;
                    }
                    if let Some(raw) = raw {
                        blobs.delete_raw(&BlobKey::parse(raw)?)?;
                    }
                    tx.execute(
                        "UPDATE message SET text_blob = NULL, raw_blob = NULL WHERE id = ?1",
                        [id],
                    )?;
                    apply::reindex(tx, *id, "")?;
                }
                Ok(ids.len() as u64)
            })
            .await?;
        total += trimmed;
        if trimmed < SWEEP_BATCH as u64 {
            return Ok(total);
        }
    }
}

/// Fetches raw messages from `start` on, newest first, two at a time,
/// until none is missing or the raw blobs reach `cap` bytes. Returns how
/// many were fetched.
pub async fn prefetch(
    client: &ServerClient,
    store: &SqliteStore,
    start: i64,
    cap: u64,
) -> Result<u64> {
    let mut fetched = 0;
    let mut failed: Vec<i64> = Vec::new();
    loop {
        let skip = serde_json::to_string(&failed).unwrap_or_default();
        let batch: Vec<(i64, BlobKey)> = store
            .cache()
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, account_id, email_id FROM message \
                     WHERE date >= ?1 AND raw_blob IS NULL AND tombstoned_at IS NULL \
                     AND id NOT IN (SELECT value FROM json_each(?2)) \
                     ORDER BY date DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![start, skip, PREFETCH_BATCH], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (id, account, email_id) = row?;
                    out.push((
                        id,
                        BlobKey::for_message(
                            tagmail_core::AccountId(account),
                            &tagmail_core::EmailId::parse(&email_id)?,
                        ),
                    ));
                }
                Ok(out)
            })
            .await?;
        if batch.is_empty() {
            return Ok(fetched);
        }
        let results: Vec<(i64, Result<BlobKey>)> = futures_util::stream::iter(batch)
            .map(|(id, key)| async move { (id, fetch_raw(client, store.blobs(), id, key).await) })
            .buffer_unordered(PREFETCH_CONCURRENCY)
            .collect()
            .await;
        let mut stored = Vec::new();
        for (id, result) in results {
            match result {
                Ok(key) => stored.push((id, key)),
                Err(crate::Error::Client(e @ crate::ClientError::Offline(_))) => {
                    return Err(e.into());
                }
                Err(e) => {
                    tracing::warn!(message = id, error = %e, "raw prefetch failed");
                    failed.push(id);
                }
            }
        }
        fetched += stored.len() as u64;
        store
            .cache()
            .write(move |tx| {
                for (id, key) in &stored {
                    tx.execute(
                        "UPDATE message SET raw_blob = ?2 WHERE id = ?1",
                        params![id, key.as_str()],
                    )?;
                }
                Ok(())
            })
            .await?;
        let blobs = Arc::clone(store.blobs());
        let freed = tokio::task::spawn_blocking(move || blobs.evict_raw(cap))
            .await
            .map_err(|e| crate::Error::Corrupt(format!("evict task: {e}")))??;
        if freed > 0 {
            return Ok(fetched);
        }
    }
}

/// Downloads one raw message into the blob store.
pub async fn fetch_raw(
    client: &ServerClient,
    blobs: &Arc<BlobStore>,
    id: i64,
    key: BlobKey,
) -> Result<BlobKey> {
    let builder = client.raw_request(Method::GET, &format!("api/replica/messages/{id}/raw"))?;
    let resp = client.send(builder).await?;
    let mut body = resp.bytes_stream();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<Vec<u8>>>(4);
    let blobs = Arc::clone(blobs);
    let writer = tokio::task::spawn_blocking(move || -> Result<Option<BlobKey>> {
        let mut w = blobs.raw_writer(key, DEFAULT_RAW_CAP)?;
        while let Some(chunk) = rx.blocking_recv() {
            match chunk {
                Some(chunk) => w.write_chunk(&chunk)?,
                None => return Ok(Some(w.finish()?)),
            }
        }
        Ok(None)
    });
    let mut failure = None;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                if tx.send(Some(bytes.to_vec())).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                client.link().mark_down();
                failure = Some(crate::ClientError::Offline(e.to_string()));
                break;
            }
        }
    }
    if failure.is_none() {
        let _ = tx.send(None).await;
    }
    drop(tx);
    let written = writer
        .await
        .map_err(|e| crate::Error::Corrupt(format!("raw writer: {e}")))??;
    if let Some(e) = failure {
        return Err(e.into());
    }
    written.ok_or_else(|| crate::Error::Incomplete(format!("raw message {id} cut short")))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tagmail_core::{AccountId, EmailId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use super::*;

    const RAW: &str = "From: ann@x.example\r\nSubject: Hi\r\n\r\nHello\r\n";

    #[tokio::test]
    async fn a_raw_message_may_take_longer_than_the_stream_idle_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{RAW}",
                RAW.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let client = ServerClient::with_idle(&url, None, Duration::from_millis(200)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blobs = Arc::new(BlobStore::open(dir.path()).unwrap());
        let key = BlobKey::for_message(AccountId(1), &EmailId::parse("g:1").unwrap());

        fetch_raw(&client, &blobs, 1, key.clone()).await.unwrap();

        assert!(blobs.open_raw(&key).unwrap().is_some());
    }
}
