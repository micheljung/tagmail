//! Browser sessions and device tokens in `config.db`. Only SHA-256 hashes of
//! the random tokens are stored.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tagmail_api_core::auth_dto::{DeviceDto, SessionDto};
use tagmail_store::rusqlite::{OptionalExtension, params};
use tagmail_store::{ConfigDb, rusqlite};

pub const TOKEN_BYTES: usize = 32;
/// `last_seen_at` is written at most this often per session or device.
pub const TOUCH_INTERVAL: i64 = 60;
const MAX_USER_AGENT: usize = 256;

pub type TokenHash = [u8; 32];

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator is available");
    bytes
}

/// A new 256-bit token, base64url.
pub fn new_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<TOKEN_BYTES>())
}

pub fn hash_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

pub fn session_id(hash: &TokenHash) -> String {
    hex::encode(hash)
}

#[derive(Debug, Clone, Copy)]
pub struct Lifetimes {
    pub idle: Duration,
    pub absolute: Duration,
}

impl Lifetimes {
    fn idle_secs(&self) -> i64 {
        self.idle.as_secs() as i64
    }
}

fn session_row(
    r: &rusqlite::Row<'_>,
    lifetimes: Lifetimes,
    current: Option<&TokenHash>,
) -> rusqlite::Result<SessionDto> {
    let hash: Vec<u8> = r.get(0)?;
    let last_seen_at: i64 = r.get(2)?;
    let expires_at: i64 = r.get(3)?;
    Ok(SessionDto {
        id: hex::encode(&hash),
        current: current.is_some_and(|c| c.as_slice() == hash.as_slice()),
        created_at: r.get(1)?,
        last_seen_at,
        expires_at,
        idle_expires_at: (last_seen_at + lifetimes.idle_secs()).min(expires_at),
        user_agent: r.get(4)?,
    })
}

const SESSION_COLUMNS: &str = "id_hash, created_at, last_seen_at, expires_at, user_agent";

/// Creates a session and returns its token. Also drops expired sessions and
/// pairing codes.
pub async fn create_session(
    db: &ConfigDb,
    lifetimes: Lifetimes,
    user_agent: Option<String>,
) -> tagmail_store::Result<(String, SessionDto)> {
    let token = new_token();
    let hash = hash_token(&token);
    let user_agent = user_agent.map(|ua| ua.chars().take(MAX_USER_AGENT).collect::<String>());
    let dto = db
        .write(move |tx| {
            let now = now();
            tx.execute(
                "DELETE FROM session WHERE expires_at <= ?1 OR last_seen_at <= ?1 - ?2",
                params![now, lifetimes.idle_secs()],
            )?;
            tx.execute("DELETE FROM pairing_code WHERE expires_at <= ?1", [now])?;
            tx.execute(
                "INSERT INTO session (id_hash, created_at, last_seen_at, expires_at, user_agent)
                 VALUES (?1, ?2, ?2, ?3, ?4)",
                params![
                    hash.as_slice(),
                    now,
                    now + lifetimes.absolute.as_secs() as i64,
                    user_agent
                ],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM session WHERE id_hash = ?1"),
                [hash.as_slice()],
                |r| session_row(r, lifetimes, Some(&hash)),
            )?)
        })
        .await?;
    Ok((token, dto))
}

/// The live session for `hash`; refreshes its idle timer.
pub async fn session(
    db: &ConfigDb,
    lifetimes: Lifetimes,
    hash: TokenHash,
) -> tagmail_store::Result<Option<SessionDto>> {
    let row = db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {SESSION_COLUMNS} FROM session WHERE id_hash = ?1"),
                    [hash.as_slice()],
                    |r| session_row(r, lifetimes, Some(&hash)),
                )
                .optional()?)
        })
        .await?;
    let Some(mut row) = row else { return Ok(None) };
    let now = now();
    if row.expires_at <= now || row.idle_expires_at <= now {
        db.write(move |tx| {
            tx.execute("DELETE FROM session WHERE id_hash = ?1", [hash.as_slice()])?;
            Ok(())
        })
        .await?;
        return Ok(None);
    }
    if now - row.last_seen_at >= TOUCH_INTERVAL {
        db.write(move |tx| {
            tx.execute(
                "UPDATE session SET last_seen_at = ?2 WHERE id_hash = ?1",
                params![hash.as_slice(), now],
            )?;
            Ok(())
        })
        .await?;
        row.last_seen_at = now;
        row.idle_expires_at = (now + lifetimes.idle_secs()).min(row.expires_at);
    }
    Ok(Some(row))
}

pub async fn sessions(
    db: &ConfigDb,
    lifetimes: Lifetimes,
    current: Option<TokenHash>,
) -> tagmail_store::Result<Vec<SessionDto>> {
    let now = now();
    db.read(move |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM session
             WHERE expires_at > ?1 AND last_seen_at > ?1 - ?2
             ORDER BY last_seen_at DESC, created_at DESC"
        ))?;
        let rows = stmt
            .query_map(params![now, lifetimes.idle_secs()], |r| {
                session_row(r, lifetimes, current.as_ref())
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

/// Deletes a session by its hash; `false` if it did not exist.
pub async fn delete_session(db: &ConfigDb, hash: TokenHash) -> tagmail_store::Result<bool> {
    db.write(move |tx| {
        Ok(tx.execute("DELETE FROM session WHERE id_hash = ?1", [hash.as_slice()])? > 0)
    })
    .await
}

/// The id of the unrevoked device holding the token `hash`.
pub async fn device(db: &ConfigDb, hash: TokenHash) -> tagmail_store::Result<Option<i64>> {
    let row = db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, last_seen_at FROM device WHERE token_hash = ?1 AND revoked_at IS NULL",
                    [hash.as_slice()],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .optional()?)
        })
        .await?;
    let Some((id, last_seen)) = row else {
        return Ok(None);
    };
    let now = now();
    if last_seen.is_none_or(|t| now - t >= TOUCH_INTERVAL) {
        db.write(move |tx| {
            tx.execute(
                "UPDATE device SET last_seen_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            Ok(())
        })
        .await?;
    }
    Ok(Some(id))
}

pub async fn devices(db: &ConfigDb) -> tagmail_store::Result<Vec<DeviceDto>> {
    db.read(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, last_seen_at FROM device
             WHERE revoked_at IS NULL ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DeviceDto {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    created_at: r.get(2)?,
                    last_seen_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

/// Revokes a device; `false` if it does not exist or is already revoked.
pub async fn revoke_device(db: &ConfigDb, id: i64) -> tagmail_store::Result<bool> {
    db.write(move |tx| {
        Ok(tx.execute(
            "UPDATE device SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now()],
        )? > 0)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_base64url_and_hash_stably() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), TOKEN_BYTES);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_eq!(session_id(&hash_token(&a)).len(), 64);
    }
}
