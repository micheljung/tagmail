//! Label registry and rename/delete jobs (plan §3.10, Decision d).
//!
//! Display names, colors and order live in `label_registry` (config.db),
//! mirrored to the IMAP server annotation [`METADATA_ENTRY`] when the server
//! has METADATA. A rename or delete re-flags every message: a `label_job`
//! walks the account's mailboxes one at a time, persisting the processed
//! mailbox names, so it resumes after a restart; re-processing a mailbox
//! is harmless because the STOREs are idempotent.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tagmail_api_core::dto::LabelDto;
use tagmail_api_core::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelJobKindDto, LabelJobStatusDto, LabelPatchDto, NewLabelDto,
};
use tagmail_api_core::writes::normalize_color;
use tagmail_api_core::{WriteError, WriteResult};
use tagmail_core::ops::label_spellings;
use tagmail_core::{
    AccountId, DEFAULT_KEYWORD_LIMIT, Keyword, KeywordClass, KeywordSet, LabelKey,
    classify_keyword, encode_label,
};
use tagmail_imap::{Flag, ImapSession, SelectOptions, StoreOp, Uid, UidSet};
use tagmail_store::rusqlite::{self, OptionalExtension, params};
use tagmail_store::{ChangeOp, ConfigDb, Entity};

use crate::cache::SyncState;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::jobs::{JobFuture, Priority};
use crate::supervisor::{ConnectorFactory, Engine, Session};
use crate::sync;

/// Server annotation holding the registry as JSON.
pub const METADATA_ENTRY: &str = "/private/vendor/tagmail/labels";
/// Characters of a label name.
pub const MAX_LABEL_NAME: usize = 200;
const REGISTRY_VERSION: u32 = 1;

pub(crate) async fn display_name(config: &ConfigDb, key: &LabelKey) -> Result<Option<String>> {
    let key = key.as_str().to_owned();
    Ok(config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT display_name FROM label_registry WHERE label_key = ?1 \
                 ORDER BY account_id LIMIT 1",
                [key],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await?)
}

pub(crate) async fn has_jobs(config: &ConfigDb, account: AccountId) -> Result<bool> {
    Ok(config
        .read(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM label_job WHERE account_id = ?1)",
                [account.0],
                |r| r.get(0),
            )?)
        })
        .await?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Rename,
    Delete,
}

#[derive(Debug, Clone)]
struct Job {
    id: i64,
    account: AccountId,
    kind: JobKind,
    from: Keyword,
    to: Option<Keyword>,
    done: Vec<String>,
}

impl Job {
    fn source_key(&self) -> LabelKey {
        key_of(&self.from)
    }
}

fn key_of(k: &Keyword) -> LabelKey {
    match classify_keyword(k) {
        KeywordClass::Label { key, .. } => key,
        KeywordClass::Hidden => LabelKey::of(k.as_str()),
    }
}

fn bad(what: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("label_job: {what}").into(),
    )
}

fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let kind: String = r.get(2)?;
    let progress: String = r.get(5)?;
    Ok(Job {
        id: r.get(0)?,
        account: AccountId(r.get(1)?),
        kind: match kind.as_str() {
            "rename" => JobKind::Rename,
            _ => JobKind::Delete,
        },
        from: Keyword::new(r.get::<_, String>(3)?).map_err(bad)?,
        to: r
            .get::<_, Option<String>>(4)?
            .map(Keyword::new)
            .transpose()
            .map_err(bad)?,
        done: serde_json::from_str(&progress).map_err(bad)?,
    })
}

async fn jobs(config: &ConfigDb, account: Option<AccountId>) -> Result<Vec<Job>> {
    Ok(config
        .read(move |c| {
            let mut stmt = c.prepare(
                "SELECT id, account_id, kind, from_kw, to_kw, progress FROM label_job \
                 WHERE ?1 IS NULL OR account_id = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([account.map(|a| a.0)], job_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?)
}

/// What one job step did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// The mailbox is processed.
    Mailbox(String),
    /// Every mailbox is processed.
    Complete,
}

/// Re-flags the next unprocessed mailbox of `job`.
async fn job_step<S: ImapSession>(s: &mut S, ctx: &Ctx, job: Job) -> Result<Step> {
    let rows = sync::ordered(ctx).await?;
    let Some(row) = rows
        .into_iter()
        .find(|r| r.keyword_capable && !job.done.contains(&r.name))
    else {
        return Ok(Step::Complete);
    };
    let key = job.source_key();
    let id = row.id;
    let cached: Vec<(u32, String)> = ctx
        .read(move |c| {
            let mut stmt = c.prepare_cached(
                "SELECT uid, coalesce(srv_keywords, keywords) FROM location \
                 WHERE mailbox_id = ?1 AND coalesce(srv_keywords, keywords) != ''",
            )?;
            let rows = stmt
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?;
    let mut uids = UidSet::new();
    let mut spellings = BTreeSet::new();
    for (uid, keywords) in cached {
        let found = label_spellings(
            &KeywordSet::from_storage(&keywords).map_err(tagmail_store::Error::from)?,
            &key,
        );
        if !found.is_empty() {
            uids.insert(Uid(uid));
            spellings.extend(found);
        }
    }
    if !uids.is_empty() {
        let sel = s.select(&row.name, SelectOptions::default()).await?;
        if sel.uid_validity.0 != row.uidvalidity {
            return Err(Error::Unavailable(format!(
                "{} changed UIDVALIDITY; waiting for it to resync",
                row.name
            )));
        }
        if let Some(to) = &job.to {
            s.store_flags(
                &uids,
                StoreOp::Add,
                &[Flag::Keyword(to.as_str().to_owned())],
                None,
            )
            .await?;
        }
        let remove: Vec<Flag> = spellings
            .iter()
            .filter(|k| job.to.as_ref() != Some(*k))
            .map(|k| Flag::Keyword(k.as_str().to_owned()))
            .collect();
        if !remove.is_empty() {
            s.store_flags(&uids, StoreOp::Remove, &remove, None).await?;
        }
        if row.state == SyncState::Live {
            sync::sync_mailbox(s, ctx, &row, &|| false).await?;
        }
    }
    Ok(Step::Mailbox(row.name))
}

fn job_step_job<'a, F: ConnectorFactory>(
    s: &'a mut Session<F>,
    ctx: &'a Ctx,
    job: Job,
) -> JobFuture<'a, Step> {
    Box::pin(job_step(s, ctx, job))
}

/// Runs one step of the account's oldest label job; `false` when it could
/// not make progress now (the error is kept for `/api/label-jobs`).
pub(crate) async fn run_step<F: ConnectorFactory>(
    engine: &Engine<F>,
    ctx: &Ctx,
    account: AccountId,
) -> Result<bool> {
    let config = &engine.inner.stores.config;
    let Some(job) = jobs(config, Some(account)).await?.into_iter().next() else {
        return Ok(false);
    };
    let id = job.id;
    let result = engine
        .run_job(account, Priority::Background, {
            let job = job.clone();
            move |s, ctx| job_step_job::<F>(s, ctx, job)
        })
        .await;
    let step = match result {
        Ok(step) => step,
        Err(e) => {
            engine.set_job_error(id, Some(e.to_string()));
            let dto = job_dto(engine, &job).await?;
            ctx.write(move |w| {
                w.emit(Entity::LabelJob, id, ChangeOp::Upsert, Some(json!(dto)));
                Ok(())
            })
            .await?;
            return Ok(false);
        }
    };
    engine.set_job_error(id, None);
    match step {
        Step::Mailbox(name) => {
            let mut done = job.done.clone();
            done.push(name);
            let progress = serde_json::to_string(&done).map_err(|e| {
                Error::Store(tagmail_store::Error::Corrupt(format!("label_job: {e}")))
            })?;
            config
                .write(move |tx| {
                    tx.execute(
                        "UPDATE label_job SET progress = ?2 WHERE id = ?1",
                        params![id, progress],
                    )?;
                    Ok(())
                })
                .await?;
            let dto = job_dto(engine, &Job { done, ..job }).await?;
            ctx.write(move |w| {
                w.emit(Entity::LabelJob, id, ChangeOp::Upsert, Some(json!(dto)));
                Ok(())
            })
            .await?;
        }
        Step::Complete => {
            config
                .write(move |tx| {
                    tx.execute("DELETE FROM label_job WHERE id = ?1", [id])?;
                    Ok(())
                })
                .await?;
            engine.set_job_error(id, None);
            let mut dto = job_dto(engine, &job).await?;
            dto.status = LabelJobStatusDto::Done;
            dto.done = dto.total;
            ctx.write(move |w| {
                w.emit(Entity::LabelJob, id, ChangeOp::Upsert, Some(json!(dto)));
                Ok(())
            })
            .await?;
        }
    }
    Ok(true)
}

async fn job_dto<F: ConnectorFactory>(engine: &Engine<F>, job: &Job) -> Result<LabelJobDto> {
    let account = job.account.0;
    let total: u32 = engine
        .inner
        .stores
        .cache
        .read(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM mailbox WHERE account_id = ?1 AND keyword_capable = 1",
                [account],
                |r| r.get(0),
            )?)
        })
        .await?;
    let error = engine.job_error(job.id);
    Ok(LabelJobDto {
        id: job.id,
        account_id: job.account.0,
        kind: match job.kind {
            JobKind::Rename => LabelJobKindDto::Rename,
            JobKind::Delete => LabelJobKindDto::Delete,
        },
        label: job.source_key().to_string(),
        to: job.to.as_ref().map(|t| key_of(t).to_string()),
        done: u32::try_from(job.done.len()).unwrap_or(u32::MAX),
        total: total.max(u32::try_from(job.done.len()).unwrap_or(u32::MAX)),
        status: if error.is_some() {
            LabelJobStatusDto::Failed
        } else {
            LabelJobStatusDto::Running
        },
        error,
    })
}

/// One registry entry as mirrored to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    key: String,
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    sort: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Registry {
    version: u32,
    labels: Vec<Entry>,
}

async fn registry(config: &ConfigDb, account: AccountId) -> Result<Vec<Entry>> {
    Ok(config
        .read(move |c| {
            let mut stmt = c.prepare(
                "SELECT label_key, display_name, color, sort FROM label_registry \
                 WHERE account_id = ?1 ORDER BY sort, label_key",
            )?;
            let rows = stmt
                .query_map([account.0], |r| {
                    Ok(Entry {
                        key: r.get(0)?,
                        name: r.get(1)?,
                        color: r.get(2)?,
                        sort: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?)
}

/// Merges the server's registry into config.db (entries it lacks) and
/// writes the result back, when the server has METADATA.
async fn mirror_job<S: ImapSession>(
    s: &mut S,
    config: Arc<ConfigDb>,
    account: AccountId,
) -> Result<()> {
    if !s.capabilities().metadata() {
        return Ok(());
    }
    let remote = s.get_metadata(METADATA_ENTRY).await?;
    let remote: Vec<Entry> = remote
        .and_then(|bytes| serde_json::from_slice::<Registry>(&bytes).ok())
        .map(|r| r.labels)
        .unwrap_or_default();
    let local = registry(&config, account).await?;
    let known: BTreeSet<String> = local.iter().map(|e| e.key.clone()).collect();
    let pending: BTreeSet<String> = jobs(&config, Some(account))
        .await?
        .iter()
        .map(|j| j.source_key().to_string())
        .collect();
    let imports: Vec<Entry> = remote
        .into_iter()
        .filter(|e| {
            !known.contains(&e.key)
                && !pending.contains(&e.key)
                && LabelKey::of(&e.name).as_str() == e.key
                && encode_label(&e.name, DEFAULT_KEYWORD_LIMIT).is_ok()
        })
        .map(|e| Entry {
            color: e.color.as_deref().and_then(normalize_color),
            ..e
        })
        .collect();
    if !imports.is_empty() {
        let rows = imports.clone();
        config
            .write(move |tx| {
                for e in rows {
                    tx.execute(
                        "INSERT OR IGNORE INTO label_registry \
                         (account_id, label_key, display_name, color, sort) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![account.0, e.key, e.name, e.color, e.sort],
                    )?;
                }
                Ok(())
            })
            .await?;
    }
    let labels = registry(&config, account).await?;
    let body = serde_json::to_vec(&Registry {
        version: REGISTRY_VERSION,
        labels,
    })
    .map_err(|e| Error::Store(tagmail_store::Error::Corrupt(format!("registry: {e}"))))?;
    s.set_metadata(METADATA_ENTRY, Some(&body)).await?;
    Ok(())
}

impl<F: ConnectorFactory> Engine<F> {
    fn set_job_error(&self, id: i64, error: Option<String>) {
        let mut errors = self
            .inner
            .job_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match error {
            Some(e) => errors.insert(id, e),
            None => errors.remove(&id),
        };
    }

    fn job_error(&self, id: i64) -> Option<String> {
        self.inner
            .job_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Syncs the account's registry with the server annotation in the
    /// background; a server without METADATA keeps it in config.db only.
    pub(crate) fn mirror_registry(&self, account: AccountId) {
        let engine = self.clone();
        let config = self.inner.stores.config.clone();
        self.inner.runtime.spawn(async move {
            let result = engine
                .run_job(account, Priority::Background, move |s, _| {
                    Box::pin(mirror_job(s, config, account)) as JobFuture<'_, ()>
                })
                .await;
            if let Err(e) = result {
                tracing::warn!(account = account.0, error = %e, "label registry mirror failed");
            }
        });
    }

    async fn active_accounts(&self) -> Result<Vec<AccountId>> {
        Ok(self
            .inner
            .stores
            .config
            .read(|c| {
                let mut stmt =
                    c.prepare("SELECT id FROM account WHERE status = 'active' ORDER BY id")?;
                let rows = stmt
                    .query_map([], |r| Ok(AccountId(r.get(0)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Where `key` exists: registry entries `(account, name, color)` and
    /// cached keywords `(account, keyword)`.
    async fn find_label(
        &self,
        key: &LabelKey,
    ) -> Result<(Vec<(i64, String, Option<String>)>, Vec<(i64, String)>)> {
        let k = key.as_str().to_owned();
        let registered = self
            .inner
            .stores
            .config
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT account_id, display_name, color FROM label_registry \
                     WHERE label_key = ?1 ORDER BY account_id",
                )?;
                let rows = stmt
                    .query_map([k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?;
        let k = key.as_str().to_owned();
        let cached = self
            .inner
            .stores
            .cache
            .read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT account_id, keyword FROM label \
                     WHERE label_key = ?1 AND kind = 'keyword' ORDER BY account_id",
                )?;
                let rows = stmt
                    .query_map([k], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?;
        Ok((registered, cached))
    }

    async fn label_event(&self, op: ChangeOp, payload: serde_json::Value) -> Result<()> {
        let Some(account) = self.active_accounts().await?.into_iter().next() else {
            return Ok(());
        };
        self.ctx(account)
            .write(move |w| {
                w.emit(Entity::Label, 0, op, Some(payload));
                Ok(())
            })
            .await
    }

    pub(crate) async fn create_label(&self, dto: NewLabelDto) -> WriteResult<LabelDto> {
        let name = valid_name(&dto.name)?;
        let color = match dto.color.as_deref() {
            None | Some("") => None,
            Some(c) => {
                Some(normalize_color(c).ok_or_else(|| WriteError::invalid("color", "#rrggbb"))?)
            }
        };
        let key = LabelKey::of(&name);
        let (registered, cached) = self.find_label(&key).await.map_err(WriteError::internal)?;
        if !registered.is_empty() || !cached.is_empty() {
            return Err(WriteError::Conflict(format!("the label {name:?} exists")));
        }
        let accounts = self.active_accounts().await.map_err(WriteError::internal)?;
        if accounts.is_empty() {
            return Err(WriteError::Conflict(
                "add an account before creating labels".to_owned(),
            ));
        }
        let (k, n, c) = (key.to_string(), name.clone(), color.clone());
        let ids = accounts.clone();
        self.inner
            .stores
            .config
            .write(move |tx| {
                for a in ids {
                    tx.execute(
                        "INSERT INTO label_registry (account_id, label_key, display_name, color, sort) \
                         VALUES (?1, ?2, ?3, ?4, \
                         (SELECT coalesce(max(sort), 0) + 1 FROM label_registry WHERE account_id = ?1))",
                        params![a.0, k, n, c],
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(WriteError::internal)?;
        let label = LabelDto {
            key: key.to_string(),
            name,
            color,
        };
        self.label_event(ChangeOp::Upsert, json!(label))
            .await
            .map_err(WriteError::internal)?;
        for a in accounts {
            self.mirror_registry(a);
        }
        Ok(label)
    }

    pub(crate) async fn update_label(
        &self,
        key: String,
        patch: LabelPatchDto,
    ) -> WriteResult<LabelChangeDto> {
        let key = LabelKey::of(&key);
        let (registered, cached) = self.find_label(&key).await.map_err(WriteError::internal)?;
        if registered.is_empty() && cached.is_empty() {
            return Err(WriteError::NotFound("no such label".to_owned()));
        }
        let current_name = registered
            .first()
            .map(|(_, n, _)| n.clone())
            .or_else(|| {
                cached.first().and_then(|(_, kw)| {
                    Keyword::new(kw.clone())
                        .ok()
                        .map(|kw| match classify_keyword(&kw) {
                            KeywordClass::Label { name, .. } => name,
                            KeywordClass::Hidden => kw.as_str().to_owned(),
                        })
                })
            })
            .unwrap_or_else(|| key.to_string());
        let color = match patch.color.as_deref() {
            None => registered.first().and_then(|(_, _, c)| c.clone()),
            Some("") => None,
            Some(c) => {
                Some(normalize_color(c).ok_or_else(|| WriteError::invalid("color", "#rrggbb"))?)
            }
        };
        let name = match &patch.name {
            Some(n) => valid_name(n)?,
            None => current_name,
        };
        let new_key = LabelKey::of(&name);
        let mut accounts: BTreeSet<i64> = registered.iter().map(|(a, _, _)| *a).collect();
        accounts.extend(cached.iter().map(|(a, _)| *a));
        let mut job_ids = Vec::new();
        if new_key != key {
            let (r2, c2) = self
                .find_label(&new_key)
                .await
                .map_err(WriteError::internal)?;
            if !r2.is_empty() || !c2.is_empty() {
                return Err(WriteError::Conflict(format!("the label {name:?} exists")));
            }
            let to = encode_label(&name, DEFAULT_KEYWORD_LIMIT)
                .map_err(|e| WriteError::invalid("name", e.to_string()))?;
            job_ids = self
                .replace_label(
                    &key,
                    Some((&new_key, &name, &color, to)),
                    &cached,
                    &accounts,
                )
                .await
                .map_err(WriteError::internal)?;
            self.label_event(ChangeOp::Delete, json!({ "key": key.as_str() }))
                .await
                .map_err(WriteError::internal)?;
        } else {
            let (k, n, c) = (key.to_string(), name.clone(), color.clone());
            let ids: Vec<i64> = accounts.iter().copied().collect();
            self.inner
                .stores
                .config
                .write(move |tx| {
                    for a in ids {
                        tx.execute(
                            "INSERT INTO label_registry (account_id, label_key, display_name, color) \
                             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (account_id, label_key) \
                             DO UPDATE SET display_name = excluded.display_name, color = excluded.color",
                            params![a, k, n, c],
                        )?;
                    }
                    Ok(())
                })
                .await
                .map_err(WriteError::internal)?;
        }
        let label = LabelDto {
            key: new_key.to_string(),
            name,
            color,
        };
        self.label_event(ChangeOp::Upsert, json!(label))
            .await
            .map_err(WriteError::internal)?;
        for a in accounts {
            self.mirror_registry(AccountId(a));
        }
        Ok(LabelChangeDto {
            label: Some(label),
            job_id: job_ids.first().copied(),
            job_ids,
        })
    }

    pub(crate) async fn delete_label(&self, key: String) -> WriteResult<LabelChangeDto> {
        let key = LabelKey::of(&key);
        let (registered, cached) = self.find_label(&key).await.map_err(WriteError::internal)?;
        if registered.is_empty() && cached.is_empty() {
            return Err(WriteError::NotFound("no such label".to_owned()));
        }
        let mut accounts: BTreeSet<i64> = registered.iter().map(|(a, _, _)| *a).collect();
        accounts.extend(cached.iter().map(|(a, _)| *a));
        let job_ids = self
            .replace_label(&key, None, &cached, &accounts)
            .await
            .map_err(WriteError::internal)?;
        self.label_event(ChangeOp::Delete, json!({ "key": key.as_str() }))
            .await
            .map_err(WriteError::internal)?;
        for a in accounts {
            self.mirror_registry(AccountId(a));
        }
        Ok(LabelChangeDto {
            label: None,
            job_id: job_ids.first().copied(),
            job_ids,
        })
    }

    /// Moves the registry entries of `key` to the new label (or drops
    /// them) and queues a job per account whose messages carry `key`.
    async fn replace_label(
        &self,
        key: &LabelKey,
        to: Option<(&LabelKey, &str, &Option<String>, Keyword)>,
        cached: &[(i64, String)],
        accounts: &BTreeSet<i64>,
    ) -> Result<Vec<i64>> {
        let old = key.to_string();
        let target = to.map(|(k, n, c, kw)| (k.to_string(), n.to_owned(), c.clone(), kw));
        let jobs: Vec<(i64, String)> = cached.to_vec();
        let ids: Vec<i64> = accounts.iter().copied().collect();
        let job_ids = self
            .inner
            .stores
            .config
            .write(move |tx| {
                tx.execute("DELETE FROM label_registry WHERE label_key = ?1", [&old])?;
                if let Some((k, n, c, _)) = &target {
                    for a in &ids {
                        tx.execute(
                            "INSERT INTO label_registry (account_id, label_key, display_name, color) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![a, k, n, c],
                        )?;
                    }
                }
                let mut out = Vec::new();
                for (account, from) in jobs {
                    tx.execute(
                        "INSERT INTO label_job (account_id, kind, from_kw, to_kw) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            account,
                            if target.is_some() { "rename" } else { "delete" },
                            from,
                            target.as_ref().map(|t| t.3.as_str().to_owned()),
                        ],
                    )?;
                    out.push(tx.last_insert_rowid());
                }
                Ok(out)
            })
            .await?;
        for (account, _) in cached {
            self.wake(AccountId(*account)).notify_one();
        }
        Ok(job_ids)
    }

    pub(crate) async fn label_job_dtos(&self) -> WriteResult<Vec<LabelJobDto>> {
        let all = jobs(&self.inner.stores.config, None)
            .await
            .map_err(WriteError::internal)?;
        let mut out = Vec::with_capacity(all.len());
        for job in &all {
            out.push(job_dto(self, job).await.map_err(WriteError::internal)?);
        }
        Ok(out)
    }
}

fn valid_name(name: &str) -> WriteResult<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_LABEL_NAME
        || name.chars().any(char::is_control)
    {
        return Err(WriteError::invalid(
            "name",
            format!("1 to {MAX_LABEL_NAME} printable characters"),
        ));
    }
    encode_label(name, DEFAULT_KEYWORD_LIMIT)
        .map_err(|e| WriteError::invalid("name", e.to_string()))?;
    Ok(name.to_owned())
}
