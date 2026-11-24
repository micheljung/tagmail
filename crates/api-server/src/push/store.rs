//! `push_subscription` rows (config V8). A row lives as long as the browser
//! session that registered it; the foreign key cascade removes it on
//! logout, revocation and expiry.

use std::collections::BTreeSet;
use std::time::Duration;

use tagmail_api_core::push_dto::PushSubscriptionDto;
use tagmail_store::rusqlite::{self, OptionalExtension, Row, params};
use tagmail_store::{ConfigDb, Error as StoreError};

use crate::session::{self, TokenHash};

/// Subscriptions over all live sessions.
pub const MAX_SUBSCRIPTIONS: i64 = 20;
/// Consecutive failed sends after which a subscription is dropped.
pub const MAX_FAILURES: i64 = 10;

/// What the dispatcher needs to send to one subscription.
#[derive(Clone)]
pub struct Target {
    pub id: i64,
    pub endpoint: String,
    pub p256dh: Vec<u8>,
    pub auth: [u8; 16],
    pub preview: bool,
    pub account_ids: Option<Vec<i64>>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Target {
    pub fn wants(&self, account_id: i64) -> bool {
        self.account_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&account_id))
    }
}

/// A validated `POST /api/push/subscriptions`.
#[derive(Debug, Clone)]
pub struct NewSubscription {
    pub endpoint: String,
    pub p256dh: Vec<u8>,
    pub auth: [u8; 16],
    pub preview: Option<bool>,
    pub account_ids: Option<Vec<i64>>,
    pub user_agent: Option<String>,
    pub old_endpoint: Option<String>,
}

#[derive(Debug)]
pub enum Upserted {
    Created(PushSubscriptionDto),
    Updated(PushSubscriptionDto),
    TooMany,
}

fn account_ids_json(ids: Option<&[i64]>) -> Option<String> {
    ids.map(|ids| serde_json::to_string(ids).expect("ids serialize"))
}

fn parse_account_ids(json: Option<String>) -> rusqlite::Result<Option<Vec<i64>>> {
    json.map(|j| {
        serde_json::from_str(&j).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

const DTO_COLUMNS: &str = "id, endpoint, preview, account_ids, user_agent, created_at, \
                           last_success_at, session_hash";

fn host_of(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

fn dto(r: &Row<'_>, current: Option<&TokenHash>) -> rusqlite::Result<PushSubscriptionDto> {
    let endpoint: String = r.get(1)?;
    let session: Vec<u8> = r.get(7)?;
    Ok(PushSubscriptionDto {
        id: r.get(0)?,
        endpoint_host: host_of(&endpoint),
        preview: r.get(2)?,
        account_ids: parse_account_ids(r.get(3)?)?,
        user_agent: r.get(4)?,
        created_at: r.get(5)?,
        last_success_at: r.get(6)?,
        current: current.is_some_and(|c| c.as_slice() == session.as_slice()),
    })
}

fn dto_by_id(
    conn: &rusqlite::Connection,
    id: i64,
    current: Option<&TokenHash>,
) -> rusqlite::Result<Option<PushSubscriptionDto>> {
    conn.query_row(
        &format!("SELECT {DTO_COLUMNS} FROM push_subscription WHERE id = ?1"),
        [id],
        |r| dto(r, current),
    )
    .optional()
}

pub async fn list(
    db: &ConfigDb,
    current: Option<TokenHash>,
) -> Result<Vec<PushSubscriptionDto>, StoreError> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {DTO_COLUMNS} FROM push_subscription ORDER BY id"
        ))?;
        let rows = stmt.query_map([], |r| dto(r, current.as_ref()))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })
    .await
}

/// Registers `sub` for `session`, or moves a known endpoint to it with the
/// new keys, keeping its settings unless given. A new endpoint replaces the
/// row of a known `old_endpoint` and inherits its settings the same way.
pub async fn upsert(
    db: &ConfigDb,
    session: TokenHash,
    sub: NewSubscription,
    session_idle: Duration,
) -> Result<Upserted, StoreError> {
    db.write(move |tx| {
        let now = session::now();
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM push_subscription WHERE endpoint = ?1",
                [&sub.endpoint],
                |r| r.get(0),
            )
            .optional()?;
        let ids = account_ids_json(sub.account_ids.as_deref());
        let id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE push_subscription SET session_hash = ?2, p256dh = ?3, auth = ?4, \
                     preview = coalesce(?5, preview), \
                     account_ids = CASE WHEN ?6 THEN ?7 ELSE account_ids END, \
                     user_agent = ?8, failures = 0, updated_at = ?9 WHERE id = ?1",
                    params![
                        id,
                        session.as_slice(),
                        sub.p256dh,
                        sub.auth.as_slice(),
                        sub.preview,
                        sub.account_ids.is_some(),
                        ids,
                        sub.user_agent,
                        now
                    ],
                )?;
                id
            }
            None => {
                let renewed: Option<(bool, Option<String>)> = match &sub.old_endpoint {
                    Some(old) => tx
                        .query_row(
                            "DELETE FROM push_subscription WHERE endpoint = ?1 \
                             RETURNING preview, account_ids",
                            [old],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?,
                    None => None,
                };
                let (old_preview, old_ids) = renewed.unzip();
                let preview = sub.preview.or(old_preview).unwrap_or(true);
                let ids = if sub.account_ids.is_some() {
                    ids
                } else {
                    old_ids.flatten()
                };
                let count: i64 = tx.query_row(
                    &format!("SELECT count(*) {LIVE_FROM}"),
                    params![now, idle_secs(session_idle)],
                    |r| r.get(0),
                )?;
                if count >= MAX_SUBSCRIPTIONS {
                    return Ok(Upserted::TooMany);
                }
                tx.execute(
                    "INSERT INTO push_subscription (session_hash, endpoint, p256dh, auth, \
                     preview, account_ids, user_agent, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        session.as_slice(),
                        sub.endpoint,
                        sub.p256dh,
                        sub.auth.as_slice(),
                        preview,
                        ids,
                        sub.user_agent,
                        now
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };
        let dto = dto_by_id(tx, id, Some(&session))?.expect("row just written");
        Ok(match existing {
            Some(_) => Upserted::Updated(dto),
            None => Upserted::Created(dto),
        })
    })
    .await
}

/// `None` when there is no such subscription.
pub async fn patch(
    db: &ConfigDb,
    id: i64,
    current: TokenHash,
    preview: Option<bool>,
    account_ids: Option<Option<Vec<i64>>>,
) -> Result<Option<PushSubscriptionDto>, StoreError> {
    db.write(move |tx| {
        let ids = account_ids
            .as_ref()
            .and_then(|ids| account_ids_json(ids.as_deref()));
        let changed = tx.execute(
            "UPDATE push_subscription SET preview = coalesce(?2, preview), \
             account_ids = CASE WHEN ?3 THEN ?4 ELSE account_ids END, updated_at = ?5 \
             WHERE id = ?1",
            params![id, preview, account_ids.is_some(), ids, session::now()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(dto_by_id(tx, id, Some(&current))?)
    })
    .await
}

pub async fn delete(db: &ConfigDb, id: i64) -> Result<bool, StoreError> {
    db.write(move |tx| Ok(tx.execute("DELETE FROM push_subscription WHERE id = ?1", [id])? > 0))
        .await
}

fn target(r: &Row<'_>) -> rusqlite::Result<Target> {
    let auth: Vec<u8> = r.get(3)?;
    Ok(Target {
        id: r.get(0)?,
        endpoint: r.get(1)?,
        p256dh: r.get(2)?,
        auth: auth.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Blob, "auth".into())
        })?,
        preview: r.get(4)?,
        account_ids: parse_account_ids(r.get(5)?)?,
    })
}

/// Rows whose session is neither expired nor idle; binds now as `?1` and
/// the idle timeout in seconds as `?2`, like `session::session`.
const LIVE_FROM: &str = "FROM push_subscription p JOIN session s ON s.id_hash = p.session_hash \
     WHERE s.expires_at > ?1 AND s.last_seen_at > ?1 - ?2";

fn idle_secs(idle: Duration) -> i64 {
    i64::try_from(idle.as_secs()).unwrap_or(i64::MAX)
}

fn target_sql() -> String {
    format!("SELECT p.id, p.endpoint, p.p256dh, p.auth, p.preview, p.account_ids {LIVE_FROM}")
}

/// Subscriptions of live sessions, and the owner's own addresses.
pub async fn targets(
    db: &ConfigDb,
    session_idle: Duration,
) -> Result<(Vec<Target>, BTreeSet<String>), StoreError> {
    db.read(move |conn| {
        let mut stmt = conn.prepare(&format!("{} ORDER BY p.id", target_sql()))?;
        let targets = stmt
            .query_map(params![session::now(), idle_secs(session_idle)], target)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if targets.is_empty() {
            return Ok((targets, BTreeSet::new()));
        }
        let mut stmt = conn.prepare(
            "SELECT lower(address) FROM identity \
             UNION SELECT lower(email) FROM account WHERE email IS NOT NULL",
        )?;
        let own = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok((targets, own))
    })
    .await
}

pub async fn target_by_id(
    db: &ConfigDb,
    id: i64,
    session_idle: Duration,
) -> Result<Option<Target>, StoreError> {
    db.read(move |conn| {
        Ok(conn
            .query_row(
                &format!("{} AND p.id = ?3", target_sql()),
                params![session::now(), idle_secs(session_idle), id],
                target,
            )
            .optional()?)
    })
    .await
}

pub async fn exists(db: &ConfigDb, id: i64) -> Result<bool, StoreError> {
    db.read(move |conn| {
        Ok(conn
            .query_row(
                "SELECT 1 FROM push_subscription WHERE id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    })
    .await
}

/// Accounts among `ids` that do not exist.
pub async fn unknown_accounts(db: &ConfigDb, ids: Vec<i64>) -> Result<Vec<i64>, StoreError> {
    db.read(move |conn| {
        let mut stmt = conn.prepare("SELECT 1 FROM account WHERE id = ?1")?;
        let mut unknown = Vec::new();
        for id in ids {
            if !stmt.exists([id])? {
                unknown.push(id);
            }
        }
        Ok(unknown)
    })
    .await
}

pub async fn record_success(db: &ConfigDb, id: i64) -> Result<(), StoreError> {
    db.write(move |tx| {
        tx.execute(
            "UPDATE push_subscription SET failures = 0, last_success_at = ?2 WHERE id = ?1",
            params![id, session::now()],
        )?;
        Ok(())
    })
    .await
}

/// Counts a failed send; `true` when that dropped the subscription.
pub async fn record_failure(db: &ConfigDb, id: i64) -> Result<bool, StoreError> {
    db.write(move |tx| {
        tx.execute(
            "UPDATE push_subscription SET failures = failures + 1 WHERE id = ?1",
            [id],
        )?;
        Ok(tx.execute(
            "DELETE FROM push_subscription WHERE id = ?1 AND failures >= ?2",
            params![id, MAX_FAILURES],
        )? > 0)
    })
    .await
}
