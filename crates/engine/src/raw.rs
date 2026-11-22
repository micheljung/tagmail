//! Raw message fetch on open (plan §3.4 step 6): the whole message is
//! streamed in `BODY.PEEK[]` chunks into a temp file, then compressed into
//! the raw blob cache, which is trimmed back to its size cap (LRU).

use std::io::{Read, Seek};

use tagmail_api_core::FetchRawError;
use tagmail_core::EmailId;
use tagmail_imap::{ImapSession, SelectOptions, Uid};
use tagmail_store::BlobKey;
use tokio::io::AsyncWriteExt;

use crate::ctx::Ctx;
use crate::error::Result;

/// Bytes copied from the temp file into the blob per write.
const COPY_CHUNK: usize = 64 * 1024;

/// Where the raw message can be fetched from.
struct Source {
    email_id: String,
    /// `(mailbox name, uid)`, INBOX first.
    locations: Vec<(String, u32)>,
}

async fn source(ctx: &Ctx, message: i64) -> Result<Option<Source>> {
    let account = ctx.account.0;
    ctx.read(move |c| {
        let email_id: Option<String> = c
            .query_row(
                "SELECT email_id FROM message \
                 WHERE id = ?1 AND account_id = ?2 AND tombstoned_at IS NULL",
                [message, account],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                tagmail_store::rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        let Some(email_id) = email_id else {
            return Ok(None);
        };
        let mut stmt = c.prepare_cached(
            "SELECT mb.name, l.uid FROM location l JOIN mailbox mb ON mb.id = l.mailbox_id \
             WHERE l.message_id = ?1 \
             ORDER BY (mb.name = 'INBOX' COLLATE NOCASE) DESC, mb.id",
        )?;
        let locations = stmt
            .query_map([message], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<tagmail_store::rusqlite::Result<_>>()?;
        Ok(Some(Source {
            email_id,
            locations,
        }))
    })
    .await
}

/// Fetches message `message` of the account into the raw cache. `Err` only
/// for errors that leave the session unusable; everything else is the
/// returned [`FetchRawError`].
pub async fn fetch_raw<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    message: i64,
    cap: u64,
    cache_max: u64,
) -> Result<Result<(), FetchRawError>> {
    let Some(src) = source(ctx, message).await? else {
        return Ok(Err(FetchRawError::NotFound));
    };
    let Ok(email_id) = EmailId::parse(&src.email_id) else {
        return Ok(Err(FetchRawError::Unavailable("invalid email id".into())));
    };
    let key = BlobKey::for_message(ctx.account, &email_id);
    let cached = {
        let (blobs, key) = (ctx.blobs.clone(), key.clone());
        tokio::task::spawn_blocking(move || blobs.open_raw(&key).map(|r| r.is_some()))
            .await
            .map_err(|_| tagmail_store::Error::Closed)??
    };
    if cached {
        return Ok(Ok(()));
    }
    let unavailable = |e: &dyn std::fmt::Display| FetchRawError::Unavailable(e.to_string());
    let mut last = FetchRawError::NotFound;
    for (mailbox, uid) in src.locations {
        if s.selected() != Some(mailbox.as_str()) {
            match s.select(&mailbox, SelectOptions::default()).await {
                Ok(_) => {}
                Err(e) if e.is_fatal() => return Err(e.into()),
                Err(e) => {
                    last = unavailable(&e);
                    continue;
                }
            }
        }
        let temp = tempfile::tempfile().map_err(tagmail_store::Error::from)?;
        let mut file = tokio::fs::File::from_std(temp);
        let fetched = match s.fetch_body(Uid(uid), "", &mut file, cap).await {
            Ok(f) => f,
            Err(e) if e.is_fatal() => return Err(e.into()),
            Err(tagmail_imap::Error::NotFound) => continue,
            Err(e) => {
                last = unavailable(&e);
                continue;
            }
        };
        if fetched.truncated {
            return Ok(Err(FetchRawError::TooLarge));
        }
        if fetched.bytes == 0 {
            continue;
        }
        file.flush().await.map_err(tagmail_store::Error::from)?;
        let mut temp = file.into_std().await;
        let blobs = ctx.blobs.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            temp.rewind()?;
            let mut writer = blobs.raw_writer(key, cap)?;
            let mut buf = vec![0; COPY_CHUNK];
            loop {
                let n = temp.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.write_chunk(&buf[..n])?;
            }
            writer.finish()?;
            blobs.evict_raw(cache_max)?;
            Ok::<_, tagmail_store::Error>(())
        })
        .await
        .map_err(|_| tagmail_store::Error::Closed)??;
        return Ok(Ok(()));
    }
    Ok(Err(last))
}
