//! [`Store`] over the server's `config.db` and `cache.db`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufReader, Read};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dto::{
    AccountDto, AccountStateDto, AddressDto, ChangeDto, ChangeOpDto, EntityDto, LabelDto,
    MailboxDto, SpecialUseDto, ThreadRowDto, TlsMode,
};
use crate::signing::{KEY_LEN, UrlKeys};
use crate::{
    AccountPatch, AccountSummary, ChangeFeed, ChangePage, MarkDeleting, MessageRecord, NewAccount,
    Secret, Store, StoreResult, ThreadDetail, ThreadRows, View, ViewIndex,
};
use tagmail_core::query::{Query, Term};
use tagmail_core::{
    AccountId, ChangeCursor, EmailId, Flags, KeywordClass, LabelKey, MessageRowId, SpecialUse,
    ThreadId, classify_keyword, decode_mailbox_name,
};
use tagmail_store::rusqlite::{
    self, Connection, OptionalExtension, named_params, params, params_from_iter,
};
use tagmail_store::{
    BlobKey, BlobStore, CacheDb, Change, ChangeOp, Changes, ConfigDb, DEFAULT_RAW_CAP, Entity,
    MasterKey, NewChange, Sealed, Vault, changes, search,
};
use zeroize::Zeroizing;

/// Senders listed per thread row.
pub const MAX_SENDERS: usize = 4;
/// Age after which the URL signing key is replaced; the previous key keeps
/// verifying the links signed just before.
pub const URL_KEY_ROTATION_SECS: i64 = 7 * 24 * 60 * 60;
/// Longest merge chain followed when resolving a thread id.
const MAX_MERGE_HOPS: usize = 16;
const URL_KEY: &str = "url_key";
const URL_KEY_PREVIOUS: &str = "url_key_previous";
const URL_KEY_CREATED: &str = "url_key_created";

pub struct SqliteStore {
    config: Arc<ConfigDb>,
    cache: Arc<CacheDb>,
    vault: Arc<Vault>,
    blobs: Arc<BlobStore>,
    /// Woken after account changes are logged; set once the feed exists.
    feed: OnceLock<Arc<ChangeFeed>>,
}

const ACCOUNT_COLUMNS: &str = "id, name, imap_host, imap_port, imap_tls, smtp_host, smtp_port, \
     smtp_tls, username, max_connections, email, status";

fn tls_mode(value: &str) -> rusqlite::Result<TlsMode> {
    match value {
        "tls" => Ok(TlsMode::Tls),
        "starttls" => Ok(TlsMode::Starttls),
        other => Err(rusqlite::Error::InvalidColumnType(
            0,
            format!("tls mode {other:?}"),
            rusqlite::types::Type::Text,
        )),
    }
}

/// A row selected with [`ACCOUNT_COLUMNS`].
fn account_dto(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountDto> {
    let status: String = r.get(11)?;
    Ok(AccountDto {
        id: r.get(0)?,
        name: r.get(1)?,
        imap_host: r.get(2)?,
        imap_port: r.get(3)?,
        imap_tls: tls_mode(&r.get::<_, String>(4)?)?,
        smtp_host: r.get(5)?,
        smtp_port: r.get(6)?,
        smtp_tls: tls_mode(&r.get::<_, String>(7)?)?,
        username: r.get(8)?,
        max_connections: r.get(9)?,
        email: r.get(10)?,
        status: AccountStateDto::parse(&status).unwrap_or(AccountStateDto::Disabled),
    })
}

fn read_account(conn: &Connection, id: AccountId) -> rusqlite::Result<Option<AccountDto>> {
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = ?1"),
        [id.0],
        account_dto,
    )
    .optional()
}

/// Points the account's default identity at `email`: an identity with that
/// address becomes the default, else the default's address changes.
fn move_default_identity(conn: &Connection, id: AccountId, email: &str) -> rusqlite::Result<()> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM identity WHERE account_id = ?1 AND address = ?2 COLLATE NOCASE",
            params![id.0, email],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(identity) => {
            conn.execute(
                "UPDATE identity SET is_default = 0 WHERE account_id = ?1 AND id != ?2",
                params![id.0, identity],
            )?;
            conn.execute(
                "UPDATE identity SET is_default = 1 WHERE id = ?1",
                [identity],
            )?;
        }
        None => {
            let changed = conn.execute(
                "UPDATE identity SET address = ?2 WHERE account_id = ?1 AND is_default = 1",
                params![id.0, email],
            )?;
            if changed == 0 {
                conn.execute(
                    "INSERT INTO identity (account_id, address, is_default) VALUES (?1, ?2, 1)",
                    params![id.0, email],
                )?;
            }
        }
    }
    Ok(())
}

impl SqliteStore {
    /// Unlocks the credential data key in `config`, creating it on first
    /// start.
    pub async fn open(
        config: ConfigDb,
        cache: CacheDb,
        blobs: BlobStore,
        master: &MasterKey,
    ) -> tagmail_store::Result<SqliteStore> {
        let vault = Vault::unlock(&config, master).await?;
        Ok(SqliteStore {
            config: Arc::new(config),
            cache: Arc::new(cache),
            vault: Arc::new(vault),
            blobs: Arc::new(blobs),
            feed: OnceLock::new(),
        })
    }

    /// The feed to wake after account changes; the first call wins.
    pub fn set_feed(&self, feed: Arc<ChangeFeed>) {
        let _ = self.feed.set(feed);
    }

    /// Logs an account upsert and wakes the event streams.
    async fn emit_account(&self, account: &AccountDto) -> StoreResult<()> {
        let change = NewChange {
            entity: Entity::Account,
            entity_id: account.id,
            op: ChangeOp::Upsert,
            payload: Some(serde_json::to_string(account)?),
        };
        let epoch = self.cache.epoch();
        let head = self
            .cache
            .write(move |tx| {
                changes::append(tx, &change)?;
                changes::head(tx, epoch)
            })
            .await?;
        if let Some(feed) = self.feed.get() {
            feed.publish(head);
        }
        Ok(())
    }

    /// Message blobs, shared with the engine.
    pub fn blobs(&self) -> &Arc<BlobStore> {
        &self.blobs
    }

    pub fn config(&self) -> &Arc<ConfigDb> {
        &self.config
    }

    pub fn cache(&self) -> &Arc<CacheDb> {
        &self.cache
    }

    /// The unlocked credential vault, shared with the engine.
    pub fn vault(&self) -> &Arc<Vault> {
        &self.vault
    }

    /// Decrypts an account's stored password.
    pub async fn account_secret(&self, id: AccountId) -> StoreResult<crate::Secret> {
        let sealed = self
            .config
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT secret_ct, secret_nonce FROM account WHERE id = ?1",
                    [id.0],
                    |r| {
                        Ok(Sealed {
                            ciphertext: r.get(0)?,
                            nonce: r.get(1)?,
                        })
                    },
                )?)
            })
            .await?;
        let plain = self.vault.open(id, &sealed)?;
        Ok(crate::Secret::new(plain.as_str().to_owned()))
    }

    /// Closes both databases once nothing else (the engine) holds them.
    pub async fn close(self) -> tagmail_store::Result<()> {
        match (Arc::try_unwrap(self.config), Arc::try_unwrap(self.cache)) {
            (Ok(config), Ok(cache)) => {
                config.close().await?;
                cache.close().await
            }
            _ => {
                tracing::warn!("databases still in use at shutdown; not closing explicitly");
                Ok(())
            }
        }
    }
}

fn entity_dto(e: Entity) -> EntityDto {
    match e {
        Entity::Account => EntityDto::Account,
        Entity::Mailbox => EntityDto::Mailbox,
        Entity::Thread => EntityDto::Thread,
        Entity::Message => EntityDto::Message,
        Entity::Label => EntityDto::Label,
        Entity::Outbox => EntityDto::Outbox,
        Entity::Op => EntityDto::Op,
        Entity::LabelJob => EntityDto::LabelJob,
        Entity::Identity => EntityDto::Identity,
    }
}

fn change_dto(c: Change) -> tagmail_store::Result<ChangeDto> {
    let payload = c
        .payload
        .map(|p| {
            serde_json::from_str(&p).map_err(|e| {
                tagmail_store::Error::Corrupt(format!("change {} payload: {e}", c.seq))
            })
        })
        .transpose()?;
    Ok(ChangeDto {
        seq: c.seq,
        entity: entity_dto(c.entity),
        id: c.entity_id,
        op: match c.op {
            ChangeOp::Upsert => ChangeOpDto::Upsert,
            ChangeOp::Delete => ChangeOpDto::Delete,
            ChangeOp::Merged { target } => ChangeOpDto::Merged { target },
            ChangeOp::Bulk { mailbox } => ChangeOpDto::Bulk { mailbox },
        },
        payload,
    })
}

fn special_use_dto(u: SpecialUse) -> SpecialUseDto {
    match u {
        SpecialUse::All => SpecialUseDto::All,
        SpecialUse::Archive => SpecialUseDto::Archive,
        SpecialUse::Drafts => SpecialUseDto::Drafts,
        SpecialUse::Flagged => SpecialUseDto::Flagged,
        SpecialUse::Important => SpecialUseDto::Important,
        SpecialUse::Junk => SpecialUseDto::Junk,
        SpecialUse::Sent => SpecialUseDto::Sent,
        SpecialUse::Trash => SpecialUseDto::Trash,
    }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn snapshot<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> tagmail_store::Result<T>,
) -> tagmail_store::Result<T> {
    let tx = conn.unchecked_transaction()?;
    let out = f(&tx)?;
    tx.finish()?;
    Ok(out)
}

fn senders(from_json: Option<&str>, thread: i64) -> Vec<AddressDto> {
    match from_json.map(serde_json::from_str::<Vec<AddressDto>>) {
        None => Vec::new(),
        Some(Ok(addrs)) => addrs,
        Some(Err(e)) => {
            tracing::warn!(thread, error = %e, "unreadable message from_json");
            Vec::new()
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Columns of [`message_record`], over `message m`.
const MESSAGE_COLUMNS: &str = "m.id, m.account_id, coalesce(m.thread_id, 0), m.date, m.subject, \
     m.snippet, m.from_json, m.to_json, m.cc_json, m.has_attachment, \
     EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & :seen) = 0), \
     EXISTS (SELECT 1 FROM location l WHERE l.message_id = m.id AND (l.flags & :flagged) != 0)";

fn message_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    let id: i64 = r.get(0)?;
    let addresses = |i| -> rusqlite::Result<Vec<AddressDto>> {
        Ok(senders(r.get::<_, Option<String>>(i)?.as_deref(), id))
    };
    Ok(MessageRecord {
        id: MessageRowId(id),
        account_id: AccountId(r.get(1)?),
        thread_id: ThreadId(r.get(2)?),
        date: r.get(3)?,
        subject: r.get(4)?,
        snippet: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        from: addresses(6)?,
        to: addresses(7)?,
        cc: addresses(8)?,
        has_attachment: r.get(9)?,
        unread: r.get(10)?,
        starred: r.get(11)?,
    })
}

const SEEN: i64 = Flags::SEEN.bits() as i64;
const FLAGGED: i64 = Flags::FLAGGED.bits() as i64;

fn random_key() -> Zeroizing<[u8; KEY_LEN]> {
    Zeroizing::new(*MasterKey::generate().expose())
}

fn decode_key(hex_key: &str) -> tagmail_store::Result<Zeroizing<[u8; KEY_LEN]>> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    hex::decode_to_slice(hex_key, key.as_mut_slice())
        .map_err(|_| tagmail_store::Error::Corrupt("url signing key".to_owned()))?;
    Ok(key)
}

fn meta(tx: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    tx.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

fn set_meta(tx: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Reads the URL signing keys, creating the first or rotating a key older
/// than [`URL_KEY_ROTATION_SECS`] at `now`.
pub fn url_keys_at(tx: &Connection, now: i64) -> tagmail_store::Result<UrlKeys> {
    let current = meta(tx, URL_KEY)?;
    let created: Option<i64> = meta(tx, URL_KEY_CREATED)?.and_then(|c| c.parse().ok());
    let due = created.is_none_or(|c| now - c >= URL_KEY_ROTATION_SECS);
    if let Some(current) = current.as_deref().filter(|_| !due) {
        return Ok(UrlKeys {
            current: decode_key(current)?,
            previous: meta(tx, URL_KEY_PREVIOUS)?
                .as_deref()
                .map(decode_key)
                .transpose()?,
        });
    }
    let fresh = random_key();
    set_meta(tx, URL_KEY, &hex::encode(fresh.as_slice()))?;
    set_meta(tx, URL_KEY_CREATED, &now.to_string())?;
    let previous = match current {
        Some(old) => {
            set_meta(tx, URL_KEY_PREVIOUS, &old)?;
            Some(decode_key(&old)?)
        }
        None => None,
    };
    Ok(UrlKeys {
        current: fresh,
        previous,
    })
}

fn thread_rows(conn: &Connection, ids: &[ThreadId]) -> tagmail_store::Result<Vec<ThreadRowDto>> {
    let ids: Vec<i64> = ids.iter().map(|t| t.0).collect();
    let list = placeholders(ids.len());
    let mut rows: HashMap<i64, ThreadRowDto> = HashMap::new();

    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, subject_norm, last_date, msg_count, unread_count, starred \
         FROM thread WHERE merged_into IS NULL AND id IN ({list})"
    ))?;
    let threads = stmt.query_map(params_from_iter(&ids), |r| {
        Ok(ThreadRowDto {
            id: r.get(0)?,
            account_id: r.get(1)?,
            subject: r.get(2)?,
            snippet: String::new(),
            last_date: r.get(3)?,
            message_count: r.get(4)?,
            unread_count: r.get(5)?,
            starred: r.get(6)?,
            has_attachment: false,
            senders: Vec::new(),
            labels: Vec::new(),
            mailbox_ids: Vec::new(),
        })
    })?;
    for t in threads {
        let t = t?;
        rows.insert(t.id, t);
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, subject, snippet, from_json, has_attachment FROM message \
         WHERE tombstoned_at IS NULL AND thread_id IN ({list}) ORDER BY thread_id, date, id"
    ))?;
    let mut messages = stmt.query(params_from_iter(&ids))?;
    while let Some(m) = messages.next()? {
        let Some(row) = rows.get_mut(&m.get::<_, i64>(0)?) else {
            continue;
        };
        if let Some(subject) = m.get::<_, Option<String>>(1)? {
            row.subject = subject;
        }
        row.snippet = m.get::<_, Option<String>>(2)?.unwrap_or_default();
        row.has_attachment |= m.get::<_, bool>(4)?;
        for addr in senders(m.get::<_, Option<String>>(3)?.as_deref(), row.id) {
            if row.senders.len() < MAX_SENDERS
                && !row
                    .senders
                    .iter()
                    .any(|s| s.email.eq_ignore_ascii_case(&addr.email))
            {
                row.senders.push(addr);
            }
        }
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT m.thread_id, l.label_key FROM message m \
         JOIN message_label ml ON ml.message_id = m.id \
         JOIN label l ON l.id = ml.label_id \
         WHERE m.tombstoned_at IS NULL AND m.thread_id IN ({list}) \
         ORDER BY m.thread_id, l.label_key"
    ))?;
    let labels = stmt.query_map(params_from_iter(&ids), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for label in labels {
        let (thread, key) = label?;
        if let Some(row) = rows.get_mut(&thread) {
            row.labels.push(key);
        }
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, mailbox_id FROM thread_mailbox WHERE thread_id IN ({list}) \
         ORDER BY thread_id, mailbox_id"
    ))?;
    let mailboxes = stmt.query_map(params_from_iter(&ids), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })?;
    for m in mailboxes {
        let (thread, mailbox) = m?;
        if let Some(row) = rows.get_mut(&thread) {
            row.mailbox_ids.push(mailbox);
        }
    }

    Ok(ids.iter().filter_map(|id| rows.remove(id)).collect())
}

/// `account.server_saves_sent` for providers known to file submitted mail
/// in Sent themselves (Gmail); `None` = probe on the first send.
fn saves_sent_preset(imap_host: &str) -> Option<bool> {
    let host = imap_host.trim_end_matches('.').to_ascii_lowercase();
    ["imap.gmail.com", "imap.googlemail.com"]
        .contains(&host.as_str())
        .then_some(true)
}

impl SqliteStore {
    /// Label keys `(from, to)` of the running rename (`to`) and delete
    /// jobs: a renamed label's view also shows messages still carrying the
    /// old keyword, and labels being removed are no longer listed.
    async fn label_jobs(&self) -> StoreResult<Vec<(String, Option<String>)>> {
        let rows: Vec<(String, Option<String>)> = self
            .config
            .read(|conn| {
                let mut stmt = conn.prepare_cached("SELECT from_kw, to_kw FROM label_job")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?;
        let key = |kw: &str| {
            tagmail_core::Keyword::new(kw)
                .ok()
                .map(|k| match classify_keyword(&k) {
                    KeywordClass::Label { key, .. } => key.to_string(),
                    KeywordClass::Hidden => LabelKey::of(kw).to_string(),
                })
        };
        Ok(rows
            .into_iter()
            .filter_map(|(from, to)| Some((key(&from)?, to.as_deref().and_then(key))))
            .collect())
    }
}

impl Store for SqliteStore {
    async fn head(&self) -> StoreResult<ChangeCursor> {
        Ok(self.cache.head().await?)
    }

    async fn view_index(&self, view: View) -> StoreResult<ViewIndex> {
        let epoch = self.cache.epoch();
        let (account, view) = match view {
            View::Account(id, inner) => (Some(id), *inner),
            view => (None, view),
        };
        let renamed_from: Vec<String> = match &view {
            View::Label(key) => self
                .label_jobs()
                .await?
                .into_iter()
                .filter(|(_, to)| to.as_deref() == Some(key.as_str()))
                .map(|(from, _)| from)
                .collect(),
            _ => Vec::new(),
        };
        Ok(self
            .cache
            .read(move |conn| {
                snapshot(conn, |conn| {
                    let cursor = changes::head(conn, epoch)?;
                    let ids = match view {
                        View::Inbox => {
                            let mut stmt = conn.prepare_cached(
                                "SELECT tm.thread_id FROM thread_mailbox tm \
                                 JOIN mailbox mb ON mb.id = tm.mailbox_id \
                                 WHERE mb.name = 'INBOX' COLLATE NOCASE \
                                 AND (?1 IS NULL OR mb.account_id = ?1) \
                                 ORDER BY tm.last_date DESC, tm.thread_id",
                            )?;
                            stmt.query_map([account.map(|a| a.0)], |r| r.get(0).map(ThreadId))?
                                .collect::<rusqlite::Result<Vec<_>>>()?
                        }
                        View::Label(key) => {
                            let label = std::iter::once(key.as_str().to_owned())
                                .chain(renamed_from)
                                .map(|k| Query::Term(Term::Label(k)))
                                .collect::<Vec<_>>();
                            let label = if label.len() == 1 {
                                label.into_iter().next().unwrap_or(Query::And(Vec::new()))
                            } else {
                                Query::Or(label)
                            };
                            search::search_threads(conn, &label, account, search::MAX_HITS)?.threads
                        }
                        View::Search(query) => {
                            search::search_threads(conn, &query, account, search::MAX_HITS)?.threads
                        }
                        View::Mailbox(mailbox) => {
                            let mut stmt = conn.prepare_cached(
                                "SELECT thread_id FROM thread_mailbox WHERE mailbox_id = ?1 \
                                 ORDER BY last_date DESC, thread_id",
                            )?;
                            stmt.query_map([mailbox.0], |r| r.get(0).map(ThreadId))?
                                .collect::<rusqlite::Result<Vec<_>>>()?
                        }
                        View::Account(..) => Vec::new(),
                    };
                    Ok(ViewIndex { cursor, ids })
                })
            })
            .await?)
    }

    async fn mailboxes(&self) -> StoreResult<Vec<MailboxDto>> {
        Ok(self
            .cache
            .read(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, account_id, name, delimiter, special_use FROM mailbox \
                     ORDER BY account_id, id",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(MailboxDto {
                        id: r.get(0)?,
                        account_id: r.get(1)?,
                        name: decode_mailbox_name(&r.get::<_, String>(2)?),
                        delimiter: r.get(3)?,
                        special_use: r
                            .get::<_, Option<String>>(4)?
                            .and_then(|s| s.parse().ok())
                            .map(special_use_dto),
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?)
    }

    async fn labels(&self) -> StoreResult<Vec<LabelDto>> {
        let derived = self
            .cache
            .read(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT l.label_key, l.keyword, m.name, l.account_id FROM label l \
                     LEFT JOIN mailbox m ON m.id = l.mailbox_id",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let registry = self
            .config
            .read(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT label_key, display_name, color, account_id FROM label_registry \
                     ORDER BY account_id",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let mut labels: BTreeMap<String, LabelDto> = BTreeMap::new();
        let mut accounts: HashMap<String, BTreeSet<i64>> = HashMap::new();
        for (key, keyword, mailbox, account) in derived {
            let name = match (keyword, mailbox) {
                (Some(keyword), _) => match tagmail_core::Keyword::new(keyword.clone()) {
                    Ok(kw) => match classify_keyword(&kw) {
                        KeywordClass::Label { name, .. } => name,
                        KeywordClass::Hidden => continue,
                    },
                    Err(_) => keyword,
                },
                (None, Some(mailbox)) => decode_mailbox_name(&mailbox),
                (None, None) => key.clone(),
            };
            accounts.entry(key.clone()).or_default().insert(account);
            labels.entry(key.clone()).or_insert(LabelDto {
                key,
                name,
                color: None,
                account_ids: Vec::new(),
            });
        }
        let mut registered = HashSet::new();
        for (key, name, color, account) in registry {
            accounts.entry(key.clone()).or_default().insert(account);
            if registered.insert(key.clone()) {
                labels.insert(
                    key.clone(),
                    LabelDto {
                        key,
                        name,
                        color,
                        account_ids: Vec::new(),
                    },
                );
            }
        }
        for label in labels.values_mut() {
            if let Some(ids) = accounts.remove(&label.key) {
                label.account_ids = ids.into_iter().collect();
            }
        }
        for (from, _) in self.label_jobs().await? {
            if !registered.contains(&from) {
                labels.remove(&from);
            }
        }
        Ok(labels.into_values().collect())
    }

    async fn threads(&self, ids: Vec<ThreadId>) -> StoreResult<ThreadRows> {
        let epoch = self.cache.epoch();
        Ok(self
            .cache
            .read(move |conn| {
                snapshot(conn, |conn| {
                    Ok(ThreadRows {
                        cursor: changes::head(conn, epoch)?,
                        rows: thread_rows(conn, &ids)?,
                    })
                })
            })
            .await?)
    }

    async fn changes_since(&self, since: ChangeCursor, limit: usize) -> StoreResult<ChangePage> {
        Ok(match self.cache.changes_since(since, limit).await? {
            Changes::Gone => ChangePage::Gone,
            Changes::Page {
                changes,
                next,
                more,
            } => ChangePage::Page {
                changes: changes
                    .into_iter()
                    .map(change_dto)
                    .collect::<tagmail_store::Result<_>>()?,
                next,
                more,
            },
        })
    }

    async fn accounts(&self) -> StoreResult<Vec<AccountSummary>> {
        let mut accounts = self
            .config
            .read(|conn| {
                let mut stmt =
                    conn.prepare_cached("SELECT id, name, status, email FROM account ORDER BY id")?;
                let rows = stmt.query_map([], |r| {
                    Ok(AccountSummary {
                        id: AccountId(r.get(0)?),
                        name: r.get(1)?,
                        status: r.get(2)?,
                        email: r.get(3)?,
                        mailboxes_initial: 0,
                        mailboxes_catchup: 0,
                        mailboxes_live: 0,
                        text_pending: 0,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let counts = self
            .cache
            .read(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT account_id, sync_state, count(*) FROM mailbox GROUP BY 1, 2",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u32>(2)?,
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let text_pending = self
            .cache
            .read(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT account_id, count(*) FROM message \
                     WHERE text_blob IS NULL AND tombstoned_at IS NULL GROUP BY account_id",
                )?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        for (account, state, n) in counts {
            if let Some(a) = accounts.iter_mut().find(|a| a.id.0 == account) {
                match state.as_str() {
                    "initial" => a.mailboxes_initial = n,
                    "catchup" => a.mailboxes_catchup = n,
                    _ => a.mailboxes_live = n,
                }
            }
        }
        for (account, n) in text_pending {
            if let Some(a) = accounts.iter_mut().find(|a| a.id.0 == account) {
                a.text_pending = n;
            }
        }
        Ok(accounts)
    }

    async fn message(&self, id: MessageRowId) -> StoreResult<Option<MessageRecord>> {
        Ok(self
            .cache
            .read(move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {MESSAGE_COLUMNS} FROM message m \
                     WHERE m.id = :id AND m.tombstoned_at IS NULL"
                ))?;
                Ok(stmt
                    .query_row(
                        named_params! {":id": id.0, ":seen": SEEN, ":flagged": FLAGGED},
                        message_record,
                    )
                    .optional()?)
            })
            .await?)
    }

    async fn thread_detail(&self, id: ThreadId) -> StoreResult<Option<ThreadDetail>> {
        Ok(self
            .cache
            .read(move |conn| {
                snapshot(conn, |conn| {
                    let mut thread = id;
                    for _ in 0..MAX_MERGE_HOPS {
                        let merged: Option<Option<i64>> = conn
                            .query_row(
                                "SELECT merged_into FROM thread WHERE id = ?1",
                                [thread.0],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match merged {
                            None => return Ok(None),
                            Some(Some(target)) => thread = ThreadId(target),
                            Some(None) => break,
                        }
                    }
                    let Some(row) = thread_rows(conn, &[thread])?.pop() else {
                        return Ok(None);
                    };
                    let mut stmt = conn.prepare_cached(&format!(
                        "SELECT {MESSAGE_COLUMNS} FROM message m \
                         WHERE m.thread_id = :id AND m.tombstoned_at IS NULL \
                         ORDER BY m.date, m.id"
                    ))?;
                    let messages = stmt
                        .query_map(
                            named_params! {":id": thread.0, ":seen": SEEN, ":flagged": FLAGGED},
                            message_record,
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok((!messages.is_empty()).then_some(ThreadDetail { row, messages }))
                })
            })
            .await?)
    }

    async fn raw_message(&self, id: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        let identity: Option<(i64, String)> = self
            .cache
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT account_id, email_id FROM message WHERE id = ?1",
                        [id.0],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?)
            })
            .await?;
        let Some((account, email_id)) = identity else {
            return Ok(None);
        };
        let email_id = EmailId::parse(&email_id)?;
        let key = BlobKey::for_message(AccountId(account), &email_id);
        let blobs = self.blobs.clone();
        let raw = tokio::task::spawn_blocking(move || {
            let Some(reader) = blobs.open_raw(&key)? else {
                return Ok(None);
            };
            let mut raw = Vec::new();
            reader.take(DEFAULT_RAW_CAP + 1).read_to_end(&mut raw)?;
            if raw.len() as u64 > DEFAULT_RAW_CAP {
                return Err(tagmail_store::Error::BlobTooLarge {
                    cap: DEFAULT_RAW_CAP,
                });
            }
            Ok::<_, tagmail_store::Error>(Some(raw))
        })
        .await??;
        Ok(raw)
    }

    async fn render_source(&self, id: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        let identity: Option<(i64, String)> = self
            .cache
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT account_id, email_id FROM message WHERE id = ?1",
                        [id.0],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?)
            })
            .await?;
        let Some((account, email_id)) = identity else {
            return Ok(None);
        };
        let email_id = EmailId::parse(&email_id)?;
        let key = BlobKey::for_message(AccountId(account), &email_id);
        let blobs = self.blobs.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let Some(reader) = blobs.open_raw(&key)? else {
                return Ok(None);
            };
            let reduced =
                crate::mime::reduce::reduce(BufReader::new(reader.take(DEFAULT_RAW_CAP)))?;
            Ok::<_, tagmail_store::Error>(Some(reduced))
        })
        .await??)
    }

    async fn url_keys(&self) -> StoreResult<UrlKeys> {
        let now = now_secs();
        Ok(self.config.write(move |tx| url_keys_at(tx, now)).await?)
    }

    async fn image_senders(&self) -> StoreResult<Vec<String>> {
        Ok(self
            .config
            .read(|conn| {
                let mut stmt =
                    conn.prepare_cached("SELECT address FROM image_sender ORDER BY address")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?)
            })
            .await?)
    }

    async fn set_image_sender(&self, address: String, allow: bool) -> StoreResult<()> {
        Ok(self
            .config
            .write(move |tx| {
                if allow {
                    tx.execute(
                        "INSERT INTO image_sender (address) VALUES (?1) ON CONFLICT DO NOTHING",
                        [&address],
                    )?;
                } else {
                    tx.execute("DELETE FROM image_sender WHERE address = ?1", [&address])?;
                }
                Ok(())
            })
            .await?)
    }

    async fn create_account(&self, account: NewAccount) -> StoreResult<AccountId> {
        let vault = self.vault.clone();
        let account = self
            .config
            .write(move |tx| {
                let id: i64 =
                    tx.query_row("SELECT coalesce(max(id), 0) + 1 FROM account", [], |r| {
                        r.get(0)
                    })?;
                let sealed = vault.seal(AccountId(id), account.password.expose())?;
                let saves_sent = saves_sent_preset(&account.imap_host);
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce, max_connections, \
                     server_saves_sent, email) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        id,
                        account.name,
                        account.imap_host,
                        account.imap_port,
                        account.imap_tls.as_str(),
                        account.smtp_host,
                        account.smtp_port,
                        account.smtp_tls.as_str(),
                        account.username,
                        sealed.ciphertext,
                        sealed.nonce,
                        account.max_connections,
                        saves_sent,
                        account.email,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO identity (account_id, address, display_name, is_default) \
                     VALUES (?1, ?2, ?3, 1)",
                    params![id, account.email, account.display_name],
                )?;
                Ok(read_account(tx, AccountId(id))?)
            })
            .await?;
        let account = account.ok_or("the new account vanished")?;
        self.emit_account(&account).await?;
        Ok(AccountId(account.id))
    }

    async fn accounts_full(&self) -> StoreResult<Vec<AccountDto>> {
        Ok(self
            .config
            .read(|conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {ACCOUNT_COLUMNS} FROM account ORDER BY id"
                ))?;
                let rows = stmt.query_map([], account_dto)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?)
    }

    async fn account_settings(&self, id: AccountId) -> StoreResult<Option<NewAccount>> {
        let row = self
            .config
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {ACCOUNT_COLUMNS}, secret_ct, secret_nonce FROM account \
                             WHERE id = ?1 AND status != '{}'",
                            AccountStateDto::Deleting.as_str()
                        ),
                        [id.0],
                        |r| {
                            Ok((
                                account_dto(r)?,
                                Sealed {
                                    ciphertext: r.get(12)?,
                                    nonce: r.get(13)?,
                                },
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;
        let Some((a, sealed)) = row else {
            return Ok(None);
        };
        let password = self.vault.open(id, &sealed)?;
        Ok(Some(NewAccount {
            name: a.name,
            imap_host: a.imap_host,
            imap_port: a.imap_port,
            imap_tls: a.imap_tls,
            smtp_host: a.smtp_host,
            smtp_port: a.smtp_port,
            smtp_tls: a.smtp_tls,
            username: a.username,
            password: Secret::new(password.as_str().to_owned()),
            max_connections: a.max_connections,
            email: a.email.unwrap_or_default(),
            display_name: None,
        }))
    }

    async fn update_account(
        &self,
        id: AccountId,
        patch: AccountPatch,
    ) -> StoreResult<Option<AccountDto>> {
        let sealed = patch
            .password
            .as_ref()
            .map(|p| self.vault.seal(id, p.expose()))
            .transpose()?;
        let account = self
            .config
            .write(move |tx| {
                let status: Option<String> = tx
                    .query_row("SELECT status FROM account WHERE id = ?1", [id.0], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if status.is_none_or(|s| s == AccountStateDto::Deleting.as_str()) {
                    return Ok(None);
                }
                tx.execute(
                    "UPDATE account SET name = coalesce(?2, name), \
                     imap_host = coalesce(?3, imap_host), imap_port = coalesce(?4, imap_port), \
                     imap_tls = coalesce(?5, imap_tls), smtp_host = coalesce(?6, smtp_host), \
                     smtp_port = coalesce(?7, smtp_port), smtp_tls = coalesce(?8, smtp_tls), \
                     username = coalesce(?9, username), \
                     max_connections = coalesce(?10, max_connections), \
                     status = coalesce(?11, status), email = coalesce(?12, email), \
                     secret_ct = coalesce(?13, secret_ct), \
                     secret_nonce = coalesce(?14, secret_nonce) \
                     WHERE id = ?1",
                    params![
                        id.0,
                        patch.name,
                        patch.imap_host,
                        patch.imap_port,
                        patch.imap_tls.map(TlsMode::as_str),
                        patch.smtp_host,
                        patch.smtp_port,
                        patch.smtp_tls.map(TlsMode::as_str),
                        patch.username,
                        patch.max_connections,
                        patch.status.map(AccountStateDto::as_str),
                        patch.email,
                        sealed.as_ref().map(|s| &s.ciphertext),
                        sealed.as_ref().map(|s| &s.nonce),
                    ],
                )?;
                if let Some(email) = &patch.email {
                    move_default_identity(tx, id, email)?;
                }
                Ok(read_account(tx, id)?)
            })
            .await?;
        if let Some(account) = &account {
            self.emit_account(account).await?;
        }
        Ok(account)
    }

    async fn mark_deleting(&self, id: AccountId) -> StoreResult<MarkDeleting> {
        let (marked, account) = self
            .config
            .write(move |tx| {
                let marked = tx.execute(
                    "UPDATE account SET status = ?2, secret_ct = x'', secret_nonce = x'' \
                     WHERE id = ?1 AND status != ?2",
                    params![id.0, AccountStateDto::Deleting.as_str()],
                )?;
                Ok((marked > 0, read_account(tx, id)?))
            })
            .await?;
        Ok(match (marked, account) {
            (true, Some(account)) => {
                self.emit_account(&account).await?;
                MarkDeleting::Marked
            }
            (false, Some(_)) => MarkDeleting::AlreadyDeleting,
            (_, None) => MarkDeleting::Missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_saves_sent_itself() {
        assert_eq!(saves_sent_preset("imap.gmail.com"), Some(true));
        assert_eq!(saves_sent_preset("IMAP.googlemail.com."), Some(true));
        assert_eq!(saves_sent_preset("imap.example.com"), None);
    }
}
