//! Routes of the desktop replica, for device tokens only: the NDJSON
//! snapshot and row re-fetch (see [`tagmail_api_core::replica_dto`]), raw
//! messages and remote images.
//!
//! Snapshot and rows read `cache.db` on a dedicated read-only connection,
//! in one read transaction, so a long stream neither occupies the shared
//! reader pool nor sees a half-applied sync. Lines are sent in chunks
//! through a bounded channel, which keeps memory flat whatever the size.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, header};
use axum::response::Response as HttpResponse;
use base64::Engine as _;
use futures_util::stream;
use tagmail_api_core::api::{API_CSP, OCTET_STREAM, offline_error};
use tagmail_api_core::compose_dto::IdentityDto;
use tagmail_api_core::replica_dto::{
    DEFAULT_WINDOW_DAYS, EndRecord, ImageSenderRecord, LabelRecord, LabelRegistryRecord,
    LocationRecord, MAX_ROWS_IDS, MailboxRecord, MessageRowRecord, NDJSON, ReplicaImageDto,
    ReplicaRecord, ReplicaRowsDto, SNAPSHOT_FORMAT, SnapshotHeader, TextRecord, ThreadRecord,
};
use tagmail_api_core::{API_VERSION, ProxyError, Response, Store};
use tagmail_core::Flags;
use tagmail_store::rusqlite::{self, Connection, OpenFlags, OptionalExtension, ToSql};
use tagmail_store::{BlobKey, BlobStore, CONFIG_SCHEMA, changes};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::auth::{Principal, error, invalid};
use crate::http::{AppState, to_http};

/// Snapshots streaming at once over all devices; one per device.
pub const MAX_SNAPSHOTS: usize = 2;
pub const MAX_WINDOW_DAYS: u32 = 3650;
const RETRY_AFTER_SECS: u64 = 30;
const CHUNK: usize = 64 * 1024;
const CHUNKS_IN_FLIGHT: usize = 4;
const READ_CACHE_KIB: i64 = 2048;
const BUSY_TIMEOUT_MS: i64 = 5000;
const DAY_SECS: i64 = 86_400;
/// Six id lists of [`MAX_ROWS_IDS`] each.
const MAX_ROWS_BODY: usize = 256 * 1024;

/// Shared state of the replica routes.
pub struct Replica {
    cache_path: PathBuf,
    snapshots: Arc<Semaphore>,
    streaming: Arc<Mutex<HashSet<i64>>>,
}

impl Replica {
    pub fn new(cache_path: PathBuf) -> Replica {
        Replica {
            cache_path,
            snapshots: Arc::new(Semaphore::new(MAX_SNAPSHOTS)),
            streaming: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn start_snapshot(&self, device: i64) -> Option<SnapshotSlot> {
        let permit = self.snapshots.clone().try_acquire_owned().ok()?;
        let mut streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        streaming.insert(device).then(|| SnapshotSlot {
            _permit: permit,
            device,
            streaming: self.streaming.clone(),
        })
    }
}

/// Held while a snapshot streams.
struct SnapshotSlot {
    _permit: OwnedSemaphorePermit,
    device: i64,
    streaming: Arc<Mutex<HashSet<i64>>>,
}

impl Drop for SnapshotSlot {
    fn drop(&mut self) {
        self.streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.device);
    }
}

fn device(principal: &Principal) -> Result<i64, Box<HttpResponse>> {
    match principal {
        Principal::Device(id) => Ok(*id),
        Principal::Session(..) => Err(Box::new(error(
            403,
            "device_only",
            "only the desktop app uses the replica routes",
        ))),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn window(days: Option<u32>) -> Result<(u32, i64), Box<HttpResponse>> {
    let days = days.unwrap_or(DEFAULT_WINDOW_DAYS);
    if !(1..=MAX_WINDOW_DAYS).contains(&days) {
        return Err(Box::new(invalid(
            "windowDays",
            format!("1 to {MAX_WINDOW_DAYS} days"),
        )));
    }
    Ok((days, now() - i64::from(days) * DAY_SECS))
}

/// `GET /api/replica/snapshot?windowDays=`.
pub async fn snapshot(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    RawQuery(query): RawQuery,
) -> HttpResponse {
    let device = match device(&principal) {
        Ok(id) => id,
        Err(resp) => return *resp,
    };
    let days = form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .find(|(k, _)| k == "windowDays")
        .map(|(_, v)| v.parse::<u32>().unwrap_or(0));
    let (window_days, window_start) = match window(days) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    let Some(slot) = state.replica.start_snapshot(device) else {
        let mut resp = Response::error(429, "busy", "too many snapshots are streaming");
        resp.headers
            .push(("retry-after", RETRY_AFTER_SECS.to_string()));
        return to_http(resp);
    };
    let store = state.api.store();
    let config = match config_records(store, None).await {
        Ok(records) => records,
        Err(e) => {
            tracing::error!(error = %e, "reading config for a snapshot");
            return error(500, "internal", "internal error");
        }
    };
    let blobs = store.blobs().clone();
    let path = state.replica.cache_path.clone();
    ndjson(slot, path, move |conn, sink| {
        let tx = conn.unchecked_transaction()?;
        let epoch = changes::read_epoch(&tx)?;
        let cursor = changes::head(&tx, epoch)?;
        let cache_schema: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        sink.record(&ReplicaRecord::Header(SnapshotHeader {
            format: SNAPSHOT_FORMAT,
            api: API_VERSION,
            epoch: epoch.to_string(),
            cursor: cursor.to_string(),
            window_days,
            window_start,
            cache_schema,
            config_schema: CONFIG_SCHEMA,
        }))?;
        for record in &config {
            sink.record(record)?;
        }
        mailboxes(&tx, &Scope::All, sink)?;
        labels(&tx, &Scope::All, sink)?;
        threads(&tx, &Scope::All, sink)?;
        messages(&tx, &Scope::All, sink)?;
        texts(&tx, &blobs, &Scope::All, window_start, sink)?;
        tx.finish()?;
        Ok(())
    })
}

/// `POST /api/replica/rows`.
pub async fn rows(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Body,
) -> HttpResponse {
    if let Err(resp) = device(&principal) {
        return *resp;
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_ROWS_BODY).await else {
        return error(413, "too_large", "the request body is too large");
    };
    let req: ReplicaRowsDto = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return error(400, "bad_request", e.to_string()),
    };
    for (field, ids) in [
        ("accounts", &req.accounts),
        ("identities", &req.identities),
        ("mailboxes", &req.mailboxes),
        ("labels", &req.labels),
        ("threads", &req.threads),
        ("messages", &req.messages),
    ] {
        if ids.len() > MAX_ROWS_IDS {
            return invalid(field, format!("at most {MAX_ROWS_IDS} ids"));
        }
    }
    let (_, window_start) = match window(req.window_days) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    let store = state.api.store();
    let config = if req.accounts.is_empty() && req.identities.is_empty() {
        Vec::new()
    } else {
        match config_records(store, Some((&req.accounts, &req.identities))).await {
            Ok(records) => records,
            Err(e) => {
                tracing::error!(error = %e, "reading config rows");
                return error(500, "internal", "internal error");
            }
        }
    };
    let blobs = store.blobs().clone();
    let path = state.replica.cache_path.clone();
    ndjson((), path, move |conn, sink| {
        let tx = conn.unchecked_transaction()?;
        for record in &config {
            sink.record(record)?;
        }
        mailboxes(&tx, &Scope::Ids(&req.mailboxes), sink)?;
        labels(&tx, &Scope::Ids(&req.labels), sink)?;
        let thread_ids = merge_targets(&tx, &req.threads)?;
        threads(&tx, &Scope::Ids(&thread_ids), sink)?;
        if let Some(mailbox) = req.mailbox {
            threads(&tx, &Scope::Mailbox(mailbox), sink)?;
        }
        messages(&tx, &Scope::Ids(&req.messages), sink)?;
        texts(&tx, &blobs, &Scope::Ids(&req.messages), window_start, sink)?;
        if let Some(mailbox) = req.mailbox {
            messages(&tx, &Scope::Mailbox(mailbox), sink)?;
            texts(&tx, &blobs, &Scope::Mailbox(mailbox), window_start, sink)?;
        }
        tx.finish()?;
        Ok(())
    })
}

/// `GET /api/replica/messages/{id}/raw`.
pub async fn raw(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> HttpResponse {
    if let Err(resp) = device(&principal) {
        return *resp;
    }
    match state.api.raw_message(&id).await {
        Ok(Ok(raw)) => to_http(Response {
            status: 200,
            headers: binary_headers(OCTET_STREAM),
            body: raw,
        }),
        Ok(Err(resp)) => to_http(resp),
        Err(e) => {
            tracing::error!(error = %e, message = id, "reading a raw message");
            error(500, "internal", "internal error")
        }
    }
}

/// `POST /api/replica/image`: the image proxy without a signed token; the
/// fetcher's SSRF checks apply the same way.
pub async fn image(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> HttpResponse {
    if let Err(resp) = device(&principal) {
        return *resp;
    }
    let req: ReplicaImageDto = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return error(400, "bad_request", e.to_string()),
    };
    let Some(proxy) = state.api.image_proxy() else {
        return to_http(offline_error("no image proxy is configured"));
    };
    match proxy.fetch(req.url).await {
        Ok(image) => to_http(Response {
            status: 200,
            headers: binary_headers(image.content_type),
            body: image.bytes,
        }),
        Err(e) => {
            tracing::debug!(error = ?e, "replica image");
            match e {
                ProxyError::Blocked(why) => error(403, "blocked", why),
                ProxyError::Upstream(why) => error(502, "upstream", why),
                ProxyError::TooLarge => error(502, "too_large", "the image is too large"),
                ProxyError::Timeout => error(504, "timeout", "the image server did not answer"),
            }
        }
    }
}

fn binary_headers(content_type: impl Into<String>) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", content_type.into()),
        ("cache-control", "no-store".to_owned()),
        ("x-content-type-options", "nosniff".to_owned()),
        ("content-security-policy", API_CSP.to_owned()),
    ]
}

/// Accounts with their label registry and identities; `only` restricts
/// them to (account ids, identity ids), else all plus the image senders.
async fn config_records(
    store: &tagmail_api_core::sqlite_store::SqliteStore,
    only: Option<(&[i64], &[i64])>,
) -> anyhow::Result<Vec<ReplicaRecord>> {
    let mut accounts = store
        .accounts_full()
        .await
        .map_err(anyhow::Error::from_boxed)?;
    if let Some((ids, _)) = only {
        accounts.retain(|a| ids.contains(&a.id));
    }
    let account_ids: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    let identity_ids = only.map(|(_, ids)| ids.to_vec());
    let all = only.is_none();
    let rest = store
        .config()
        .read(move |conn| {
            let mut out = Vec::new();
            let (cond, param) = match &identity_ids {
                None => ("?1 IS NULL", None),
                Some(ids) => (
                    "id IN (SELECT value FROM json_each(?1))",
                    Some(json_ids(ids)),
                ),
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT id, account_id, address, display_name, signature_html, is_default \
                 FROM identity WHERE {cond} ORDER BY id"
            ))?;
            let mut rows = stmt.query([&param])?;
            while let Some(r) = rows.next()? {
                out.push(ReplicaRecord::Identity(IdentityDto {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    address: r.get(2)?,
                    name: r.get(3)?,
                    signature: r.get(4)?,
                    is_default: r.get(5)?,
                }));
            }
            let accounts_json = json_ids(&account_ids);
            let mut stmt = conn.prepare(
                "SELECT account_id, label_key, display_name, color, sort FROM label_registry \
                 WHERE account_id IN (SELECT value FROM json_each(?1)) \
                 ORDER BY account_id, label_key",
            )?;
            let mut rows = stmt.query([&accounts_json])?;
            while let Some(r) = rows.next()? {
                out.push(ReplicaRecord::LabelRegistry(LabelRegistryRecord {
                    account_id: r.get(0)?,
                    label_key: r.get(1)?,
                    display_name: r.get(2)?,
                    color: r.get(3)?,
                    sort: r.get(4)?,
                }));
            }
            if all {
                let mut stmt = conn.prepare("SELECT address FROM image_sender ORDER BY address")?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    out.push(ReplicaRecord::ImageSender(ImageSenderRecord {
                        address: r.get(0)?,
                    }));
                }
            }
            Ok(out)
        })
        .await?;
    let mut records: Vec<ReplicaRecord> =
        accounts.into_iter().map(ReplicaRecord::Account).collect();
    records.extend(rest);
    Ok(records)
}

fn json_ids(ids: &[i64]) -> String {
    serde_json::to_string(ids).expect("ids serialize")
}

/// Which rows of a table to send.
enum Scope<'a> {
    All,
    Ids(&'a [i64]),
    /// Rows tied to a mailbox through its messages' locations.
    Mailbox(i64),
}

impl Scope<'_> {
    /// `WHERE` clause on `id` with its one parameter; `in_mailbox` selects
    /// the ids of the table tied to mailbox `?1`.
    fn filter(&self, in_mailbox: &str) -> (String, Box<dyn ToSql>) {
        match self {
            Scope::All => ("?1 IS NULL".to_owned(), Box::new(None::<i64>)),
            Scope::Ids(ids) => (
                "id IN (SELECT value FROM json_each(?1))".to_owned(),
                Box::new(json_ids(ids)),
            ),
            Scope::Mailbox(id) => (format!("id IN ({in_mailbox})"), Box::new(*id)),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Scope::Ids(ids) if ids.is_empty())
    }
}

const MESSAGES_IN_MAILBOX: &str = "SELECT message_id FROM location WHERE mailbox_id = ?1";
const THREADS_IN_MAILBOX: &str = "SELECT m.thread_id FROM location l \
     JOIN message m ON m.id = l.message_id WHERE l.mailbox_id = ?1";

/// The given thread ids plus every thread they were merged into.
fn merge_targets(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT merged_into FROM thread WHERE id = ?1")?;
    let mut out = BTreeSet::new();
    for &id in ids {
        let mut next = Some(id);
        while let Some(id) = next {
            if !out.insert(id) {
                break;
            }
            next = stmt
                .query_row([id], |r| r.get::<_, Option<i64>>(0))
                .optional()?
                .flatten();
        }
    }
    Ok(out.into_iter().collect())
}

fn mailboxes(conn: &Connection, scope: &Scope<'_>, sink: &mut Sink) -> anyhow::Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    let (cond, param) = scope.filter("SELECT ?1");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, name, delimiter, special_use, uidvalidity, keyword_capable, \
         sync_state FROM mailbox WHERE {cond} ORDER BY id"
    ))?;
    let mut rows = stmt.query([&param])?;
    while let Some(r) = rows.next()? {
        sink.record(&ReplicaRecord::Mailbox(MailboxRecord {
            id: r.get(0)?,
            account_id: r.get(1)?,
            name: r.get(2)?,
            delimiter: r.get(3)?,
            special_use: r.get(4)?,
            uidvalidity: r.get(5)?,
            keyword_capable: r.get(6)?,
            sync_state: r.get(7)?,
        }))?;
    }
    Ok(())
}

fn labels(conn: &Connection, scope: &Scope<'_>, sink: &mut Sink) -> anyhow::Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    let (cond, param) = scope.filter("SELECT id FROM label WHERE mailbox_id = ?1");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, keyword, label_key, kind, mailbox_id FROM label \
         WHERE {cond} ORDER BY id"
    ))?;
    let mut rows = stmt.query([&param])?;
    while let Some(r) = rows.next()? {
        sink.record(&ReplicaRecord::Label(LabelRecord {
            id: r.get(0)?,
            account_id: r.get(1)?,
            keyword: r.get(2)?,
            label_key: r.get(3)?,
            kind: r.get(4)?,
            mailbox_id: r.get(5)?,
        }))?;
    }
    Ok(())
}

fn threads(conn: &Connection, scope: &Scope<'_>, sink: &mut Sink) -> anyhow::Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    let (cond, param) = scope.filter(THREADS_IN_MAILBOX);
    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, subject_norm, last_date, msg_count, unread_count, starred, \
         merged_into FROM thread WHERE {cond} ORDER BY id"
    ))?;
    let mut rows = stmt.query([&param])?;
    while let Some(r) = rows.next()? {
        sink.record(&ReplicaRecord::Thread(ThreadRecord {
            id: r.get(0)?,
            account_id: r.get(1)?,
            subject_norm: r.get(2)?,
            last_date: r.get(3)?,
            msg_count: r.get(4)?,
            unread_count: r.get(5)?,
            starred: r.get(6)?,
            merged_into: r.get(7)?,
        }))?;
    }
    Ok(())
}

fn messages(conn: &Connection, scope: &Scope<'_>, sink: &mut Sink) -> anyhow::Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    let (cond, param) = scope.filter(MESSAGES_IN_MAILBOX);
    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, email_id, msgid_hdr, in_reply_to, thread_id, date, \
         subject_norm, from_json, to_json, cc_json, subject, snippet, size, has_attachment, \
         tombstoned_at FROM message WHERE {cond} ORDER BY id"
    ))?;
    let mut locations = conn.prepare(
        "SELECT mailbox_id, uid, flags, keywords, modseq, srv_flags, srv_keywords, hidden \
         FROM location WHERE message_id = ?1 ORDER BY mailbox_id, uid",
    )?;
    let mut labels =
        conn.prepare("SELECT label_id FROM message_label WHERE message_id = ?1 ORDER BY label_id")?;
    let mut rows = stmt.query([&param])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let locations = locations
            .query_map([id], |l| {
                Ok(LocationRecord {
                    mailbox_id: l.get(0)?,
                    uid: l.get(1)?,
                    flags: l.get(2)?,
                    keywords: l.get(3)?,
                    modseq: l.get(4)?,
                    srv_flags: l.get(5)?,
                    srv_keywords: l.get(6)?,
                    hidden: l.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let labels = labels
            .query_map([id], |l| l.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        sink.record(&ReplicaRecord::Message(MessageRowRecord {
            id,
            account_id: r.get(1)?,
            email_id: r.get(2)?,
            msgid_hdr: r.get(3)?,
            in_reply_to: r.get(4)?,
            thread_id: r.get(5)?,
            date: r.get(6)?,
            subject_norm: r.get(7)?,
            from_json: r.get(8)?,
            to_json: r.get(9)?,
            cc_json: r.get(10)?,
            subject: r.get(11)?,
            snippet: r.get(12)?,
            size: r.get(13)?,
            has_attachment: r.get(14)?,
            tombstoned_at: r.get(15)?,
            locations,
            labels,
        }))?;
    }
    Ok(())
}

/// Text blobs of the scope's messages dated from `window_start` or
/// starred in any mailbox.
fn texts(
    conn: &Connection,
    blobs: &BlobStore,
    scope: &Scope<'_>,
    window_start: i64,
    sink: &mut Sink,
) -> anyhow::Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    let (cond, param) = scope.filter(MESSAGES_IN_MAILBOX);
    let mut stmt = conn.prepare(&format!(
        "SELECT id, text_blob FROM message m WHERE ({cond}) AND text_blob IS NOT NULL \
         AND (date >= ?2 OR EXISTS (SELECT 1 FROM location l \
              WHERE l.message_id = m.id AND l.flags & ?3 != 0)) \
         ORDER BY id"
    ))?;
    let flagged = i64::from(Flags::FLAGGED.bits());
    let mut rows = stmt.query(rusqlite::params![&param, window_start, flagged])?;
    let engine = base64::engine::general_purpose::STANDARD;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let key: String = r.get(1)?;
        let Some(compressed) = blobs.read_text_zstd(&BlobKey::parse(&key)?)? else {
            continue;
        };
        sink.record(&ReplicaRecord::Text(TextRecord {
            message_id: id,
            zstd: engine.encode(compressed),
        }))?;
    }
    Ok(())
}

/// Collects NDJSON lines into chunks for the response body.
struct Sink {
    tx: mpsc::Sender<Bytes>,
    buf: Vec<u8>,
    counts: BTreeMap<String, u64>,
}

impl Sink {
    fn record(&mut self, record: &ReplicaRecord) -> anyhow::Result<()> {
        if let Some(kind) = record.kind() {
            *self.counts.entry(kind.to_owned()).or_default() += 1;
        }
        serde_json::to_writer(&mut self.buf, record)?;
        self.buf.push(b'\n');
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        self.tx
            .blocking_send(chunk)
            .map_err(|_| anyhow::Error::new(ClientGone))
    }

    fn end(mut self) -> anyhow::Result<()> {
        let counts = std::mem::take(&mut self.counts);
        self.record(&ReplicaRecord::End(EndRecord { counts }))?;
        self.flush()
    }
}

#[derive(Debug)]
struct ClientGone;

impl std::fmt::Display for ClientGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the client went away")
    }
}

impl std::error::Error for ClientGone {}

fn open_cache(path: &std::path::Path) -> anyhow::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    conn.pragma_update(None, "cache_size", -READ_CACHE_KIB)?;
    conn.pragma_update(None, "mmap_size", 0)?;
    Ok(conn)
}

/// Streams what `produce` writes on its own connection to `cache.db`,
/// then an `end` line; on failure the stream stops without one. `guard`
/// lives until the body is done or dropped.
fn ndjson<G, F>(guard: G, path: PathBuf, produce: F) -> HttpResponse
where
    G: Send + 'static,
    F: FnOnce(&Connection, &mut Sink) -> anyhow::Result<()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Bytes>(CHUNKS_IN_FLIGHT);
    tokio::task::spawn_blocking(move || {
        let mut sink = Sink {
            tx,
            buf: Vec::with_capacity(CHUNK),
            counts: BTreeMap::new(),
        };
        let result = open_cache(&path)
            .and_then(|conn| produce(&conn, &mut sink))
            .and_then(|()| sink.end());
        match result {
            Err(e) if e.is::<ClientGone>() => tracing::debug!("replica stream abandoned"),
            Err(e) => tracing::error!(error = %e, "replica stream failed"),
            Ok(()) => {}
        }
    });
    let body = stream::unfold((rx, guard), |(mut rx, guard)| async move {
        let chunk = rx.recv().await?;
        Some((Ok::<_, Infallible>(chunk), (rx, guard)))
    });
    let mut resp = HttpResponse::new(Body::from_stream(body));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(API_CSP),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_snapshot_per_device_and_two_overall() {
        let replica = Replica::new(PathBuf::new());
        let a = replica.start_snapshot(1).expect("first");
        assert!(replica.start_snapshot(1).is_none(), "same device");
        let b = replica.start_snapshot(2).expect("second device");
        assert!(replica.start_snapshot(3).is_none(), "global cap");
        drop(a);
        assert!(replica.start_snapshot(2).is_none(), "still streaming");
        let c = replica.start_snapshot(1).expect("slot freed");
        drop((b, c));
        assert!(replica.start_snapshot(3).is_some());
    }

    #[test]
    fn windows_are_bounded() {
        assert_eq!(window(None).unwrap().0, DEFAULT_WINDOW_DAYS);
        assert!(window(Some(0)).is_err());
        assert!(window(Some(MAX_WINDOW_DAYS + 1)).is_err());
    }
}
