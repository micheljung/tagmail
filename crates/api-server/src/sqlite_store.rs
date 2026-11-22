//! [`Store`] over the server's `config.db` and `cache.db`.

use std::collections::HashMap;
use std::sync::Arc;

use tagmail_api_core::dto::{AddressDto, ChangeDto, ChangeOpDto, EntityDto, ThreadRowDto};
use tagmail_api_core::{
    AccountSummary, ChangePage, NewAccount, Store, StoreResult, ThreadRows, View, ViewIndex,
};
use tagmail_core::{AccountId, ChangeCursor, ThreadId};
use tagmail_store::rusqlite::{self, Connection, params, params_from_iter};
use tagmail_store::{
    CacheDb, Change, ChangeOp, Changes, ConfigDb, Entity, MasterKey, Sealed, Vault, changes,
};

/// Senders listed per thread row.
pub const MAX_SENDERS: usize = 4;

pub struct SqliteStore {
    config: Arc<ConfigDb>,
    cache: Arc<CacheDb>,
    vault: Arc<Vault>,
}

impl SqliteStore {
    /// Unlocks the credential data key in `config`, creating it on first
    /// start.
    pub async fn open(
        config: ConfigDb,
        cache: CacheDb,
        master: &MasterKey,
    ) -> tagmail_store::Result<SqliteStore> {
        let vault = Vault::unlock(&config, master).await?;
        Ok(SqliteStore {
            config: Arc::new(config),
            cache: Arc::new(cache),
            vault: Arc::new(vault),
        })
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
    pub async fn account_secret(&self, id: AccountId) -> StoreResult<tagmail_api_core::Secret> {
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
        Ok(tagmail_api_core::Secret::new(plain.as_str().to_owned()))
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

    Ok(ids.iter().filter_map(|id| rows.remove(id)).collect())
}

impl Store for SqliteStore {
    async fn head(&self) -> StoreResult<ChangeCursor> {
        Ok(self.cache.head().await?)
    }

    async fn view_index(&self, view: View) -> StoreResult<ViewIndex> {
        let epoch = self.cache.epoch();
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
                                 ORDER BY tm.last_date DESC, tm.thread_id",
                            )?;
                            stmt.query_map([], |r| r.get(0).map(ThreadId))?
                                .collect::<rusqlite::Result<Vec<_>>>()?
                        }
                        View::Mailbox(mailbox) => {
                            let mut stmt = conn.prepare_cached(
                                "SELECT thread_id FROM thread_mailbox WHERE mailbox_id = ?1 \
                                 ORDER BY last_date DESC, thread_id",
                            )?;
                            stmt.query_map([mailbox.0], |r| r.get(0).map(ThreadId))?
                                .collect::<rusqlite::Result<Vec<_>>>()?
                        }
                    };
                    Ok(ViewIndex { cursor, ids })
                })
            })
            .await?)
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
                    conn.prepare_cached("SELECT id, name, status FROM account ORDER BY id")?;
                let rows = stmt.query_map([], |r| {
                    Ok(AccountSummary {
                        id: AccountId(r.get(0)?),
                        name: r.get(1)?,
                        status: r.get(2)?,
                        mailboxes_initial: 0,
                        mailboxes_catchup: 0,
                        mailboxes_live: 0,
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
        for (account, state, n) in counts {
            if let Some(a) = accounts.iter_mut().find(|a| a.id.0 == account) {
                match state.as_str() {
                    "initial" => a.mailboxes_initial = n,
                    "catchup" => a.mailboxes_catchup = n,
                    _ => a.mailboxes_live = n,
                }
            }
        }
        Ok(accounts)
    }

    async fn create_account(&self, account: NewAccount) -> StoreResult<AccountId> {
        let vault = self.vault.clone();
        Ok(self
            .config
            .write(move |tx| {
                let id: i64 =
                    tx.query_row("SELECT coalesce(max(id), 0) + 1 FROM account", [], |r| {
                        r.get(0)
                    })?;
                let sealed = vault.seal(AccountId(id), account.password.expose())?;
                tx.execute(
                    "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, smtp_host, \
                     smtp_port, smtp_tls, username, secret_ct, secret_nonce, max_connections) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                    ],
                )?;
                Ok(AccountId(id))
            })
            .await?)
    }
}
