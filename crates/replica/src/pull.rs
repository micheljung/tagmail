//! Applies the server's `/api/changes` to the replica (§4): changed ids are
//! re-fetched as records, deletes and merges applied locally, the server's
//! entries copied into the local `change_log`, and the server cursor saved
//! with the last batch of each page.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use reqwest::Method;
use tagmail_api_core::dto::{AddressDto, ChangeDto, ChangeOpDto, ChangesDto, EntityDto};
use tagmail_api_core::replica_dto::{MAX_ROWS_IDS, ReplicaRowsDto};
use tagmail_api_core::sqlite_store::SqliteStore;
use tagmail_core::{ChangeCursor, ChangeFilter};
use tagmail_store::changes::{self, ChangeOp, Entity, NewChange};

use crate::apply;
use crate::bootstrap::INCLUDE_CALENDAR;
use crate::calendar;
use crate::client::ServerClient;
use crate::db;
use crate::error::{Error, Result};
use crate::import::{Arrival, Imported, RecordReader, Target, import};

pub const PAGE: usize = 5000;

#[derive(Debug, Default)]
pub struct Pulled {
    /// The server cursor now stored in the replica.
    pub cursor: Option<ChangeCursor>,
    /// Head of the local `change_log` after the last page, if any applied.
    pub local_head: Option<ChangeCursor>,
    /// New unread INBOX mail from other senders; never from bulk refreshes.
    pub arrivals: Vec<Arrival>,
    /// `op`, `outbox`, `cal_op` and `reminder` changes: for the durable
    /// intents, the outbox and notifications.
    pub writes: Vec<ChangeDto>,
}

#[derive(Default)]
struct Plan {
    accounts: BTreeSet<i64>,
    identities: BTreeSet<i64>,
    mailboxes: BTreeSet<i64>,
    labels: BTreeSet<i64>,
    threads: BTreeSet<i64>,
    messages: BTreeSet<i64>,
    calendars: BTreeSet<i64>,
    events: BTreeSet<i64>,
    /// Mailboxes of `bulk` changes with the disjoint UID ranges they
    /// added, `None` for all of the mailbox.
    bulk: BTreeMap<i64, Option<Vec<[u32; 2]>>>,
    deletes: Vec<(EntityDto, i64)>,
    merges: Vec<(i64, i64)>,
    log: Vec<NewChange>,
}

impl Plan {
    fn new(changes: &[ChangeDto], writes: &mut Vec<ChangeDto>) -> Plan {
        let mut plan = Plan::default();
        for c in changes {
            if c.entity == EntityDto::Unknown {
                continue;
            }
            let Some(entity) = local_entity(c.entity) else {
                writes.push(c.clone());
                continue;
            };
            match c.op {
                ChangeOpDto::Upsert => {
                    if let Some(ids) = plan.ids(c.entity) {
                        ids.insert(c.id);
                    }
                }
                ChangeOpDto::Delete => plan.deletes.push((c.entity, c.id)),
                ChangeOpDto::Merged { target } => {
                    plan.merges.push((c.id, target));
                    plan.threads.insert(target);
                }
                ChangeOpDto::Bulk { mailbox } => {
                    let added = bulk_uids(c.payload.as_ref());
                    let ranges = plan.bulk.entry(mailbox).or_insert(Some(Vec::new()));
                    match (ranges.as_mut(), added) {
                        (Some(ranges), Some(added)) => add_range(ranges, added),
                        _ => *ranges = None,
                    }
                }
            }
            plan.log.push(NewChange {
                entity,
                entity_id: c.id,
                op: match c.op {
                    ChangeOpDto::Upsert => ChangeOp::Upsert,
                    ChangeOpDto::Delete => ChangeOp::Delete,
                    ChangeOpDto::Merged { target } => ChangeOp::Merged { target },
                    ChangeOpDto::Bulk { mailbox } => ChangeOp::Bulk { mailbox },
                },
                payload: c.payload.as_ref().map(|p| p.to_string()),
            });
        }
        for (entity, id) in plan.deletes.clone() {
            if let Some(ids) = plan.ids(entity) {
                ids.remove(&id);
            }
        }
        plan
    }

    fn ids(&mut self, entity: EntityDto) -> Option<&mut BTreeSet<i64>> {
        Some(match entity {
            EntityDto::Account => &mut self.accounts,
            EntityDto::Identity => &mut self.identities,
            EntityDto::Mailbox => &mut self.mailboxes,
            EntityDto::Label => &mut self.labels,
            EntityDto::Thread => &mut self.threads,
            EntityDto::Message => &mut self.messages,
            EntityDto::Calendar => &mut self.calendars,
            EntityDto::Event => &mut self.events,
            _ => return None,
        })
    }

    /// Rows requests of at most [`MAX_ROWS_IDS`] ids per kind; `calendar`
    /// adds the accounts' calendar settings.
    fn requests(&self, window_days: u32, calendar: bool) -> Vec<ReplicaRowsDto> {
        let sets = [
            &self.accounts,
            &self.identities,
            &self.mailboxes,
            &self.labels,
            &self.threads,
            &self.messages,
            &self.calendars,
            &self.events,
        ];
        let chunk = |ids: &BTreeSet<i64>, i: usize| -> Vec<i64> {
            ids.iter()
                .skip(i * MAX_ROWS_IDS)
                .take(MAX_ROWS_IDS)
                .copied()
                .collect()
        };
        let n = sets
            .iter()
            .map(|ids| ids.len().div_ceil(MAX_ROWS_IDS))
            .max()
            .unwrap_or(0);
        (0..n)
            .map(|i| ReplicaRowsDto {
                accounts: chunk(&self.accounts, i),
                identities: chunk(&self.identities, i),
                mailboxes: chunk(&self.mailboxes, i),
                labels: chunk(&self.labels, i),
                threads: chunk(&self.threads, i),
                messages: chunk(&self.messages, i),
                mailbox: None,
                uids: None,
                window_days: Some(window_days),
                calendars: chunk(&self.calendars, i),
                events: chunk(&self.events, i),
                include: if calendar {
                    vec![ChangeFilter::INCLUDE_CALENDAR.to_owned()]
                } else {
                    Vec::new()
                },
            })
            .collect()
    }
}

/// Adds `[lo, hi]` to sorted disjoint ranges, joining overlapping and
/// adjacent ones.
fn add_range(ranges: &mut Vec<[u32; 2]>, added: [u32; 2]) {
    ranges.push(added);
    ranges.sort_unstable();
    let mut joined: Vec<[u32; 2]> = Vec::with_capacity(ranges.len());
    for &[lo, hi] in ranges.iter() {
        match joined.last_mut() {
            Some(last) if lo <= last[1].saturating_add(1) => last[1] = last[1].max(hi),
            _ => joined.push([lo, hi]),
        }
    }
    *ranges = joined;
}

fn bulk_uids(payload: Option<&serde_json::Value>) -> Option<[u32; 2]> {
    serde_json::from_value(payload?.get("uids")?.clone()).ok()
}

/// `op`, `outbox`, `cal_op` and label jobs are not replica rows.
fn local_entity(entity: EntityDto) -> Option<Entity> {
    Some(match entity {
        EntityDto::Account => Entity::Account,
        EntityDto::Mailbox => Entity::Mailbox,
        EntityDto::Thread => Entity::Thread,
        EntityDto::Message => Entity::Message,
        EntityDto::Label => Entity::Label,
        EntityDto::Identity => Entity::Identity,
        EntityDto::Calendar => Entity::Calendar,
        EntityDto::Event => Entity::Event,
        EntityDto::Outbox
        | EntityDto::Op
        | EntityDto::LabelJob
        | EntityDto::CalOp
        | EntityDto::Reminder
        | EntityDto::Unknown => return None,
    })
}

/// Pulls pages from `since` until the server has no more. A 410 surfaces
/// as [`crate::ClientError::Gone`]: bootstrap again. `calendar` pulls
/// calendar changes too.
pub async fn pull(
    client: &ServerClient,
    store: &SqliteStore,
    mut since: ChangeCursor,
    window_days: u32,
    calendar: bool,
) -> Result<Pulled> {
    let mut out = Pulled::default();
    let own = own_addresses(store).await?;
    let include = if calendar { INCLUDE_CALENDAR } else { "" };
    loop {
        let page: ChangesDto = client
            .get_json(&format!("api/changes?since={since}&limit={PAGE}{include}"))
            .await?;
        let next: ChangeCursor = page
            .next
            .parse()
            .map_err(|_| Error::Corrupt(format!("cursor {:?}", page.next)))?;
        let plan = Plan::new(&page.changes, &mut out.writes);
        let target = Target {
            cache: store.cache(),
            config: store.config(),
            blobs: store.blobs(),
        };
        let mut imported = Imported::default();
        for request in plan.requests(window_days, calendar) {
            let part = rows(client, &target, &request).await?;
            imported.threads.extend(part.threads);
            imported.arrivals.extend(part.arrivals);
        }
        for (mailbox, ranges) in &plan.bulk {
            let uids: Vec<Option<[u32; 2]>> = match ranges {
                Some(ranges) => ranges.iter().copied().map(Some).collect(),
                None => vec![None],
            };
            for uids in uids {
                let request = ReplicaRowsDto {
                    mailbox: Some(*mailbox),
                    uids,
                    window_days: Some(window_days),
                    ..ReplicaRowsDto::default()
                };
                imported
                    .threads
                    .extend(rows(client, &target, &request).await?.threads);
            }
        }
        out.local_head = Some(finish(store, plan, imported.threads, next).await?);
        out.cursor = Some(next);
        out.arrivals
            .extend(imported.arrivals.into_iter().filter(|a| !from_own(a, &own)));
        since = next;
        if !page.more {
            return Ok(out);
        }
    }
}

async fn rows(
    client: &ServerClient,
    target: &Target<'_>,
    request: &ReplicaRowsDto,
) -> Result<Imported> {
    let builder = client
        .request(Method::POST, "api/replica/rows")?
        .json(request);
    let resp = client.send(builder).await?;
    let mut reader = RecordReader::new(resp.bytes_stream());
    import(&mut reader, target, false).await
}

/// Deletes, merges, derivations, the local log and the cursor in one
/// transaction per database; config first, as cache rows may refer to it.
async fn finish(
    store: &SqliteStore,
    plan: Plan,
    mut threads: BTreeSet<i64>,
    next: ChangeCursor,
) -> Result<ChangeCursor> {
    let config_deletes: Vec<(EntityDto, i64)> = plan
        .deletes
        .iter()
        .copied()
        .filter(|(e, _)| matches!(e, EntityDto::Account | EntityDto::Identity))
        .collect();
    if !config_deletes.is_empty() {
        store
            .config()
            .write(move |tx| {
                for (entity, id) in config_deletes {
                    match entity {
                        EntityDto::Account => apply::delete_account_config(tx, id)?,
                        _ => apply::delete_identity(tx, id)?,
                    }
                }
                Ok(())
            })
            .await?;
    }
    let blobs = Arc::clone(store.blobs());
    let epoch = store.cache().epoch();
    let head = store
        .cache()
        .write(move |tx| {
            for (entity, id) in &plan.deletes {
                match entity {
                    EntityDto::Message => threads.extend(apply::delete_message(tx, &blobs, *id)?),
                    EntityDto::Thread => {
                        apply::delete_thread(tx, *id)?;
                        threads.remove(id);
                    }
                    EntityDto::Mailbox => apply::delete_mailbox(tx, *id)?,
                    EntityDto::Label => apply::delete_label(tx, *id)?,
                    EntityDto::Account => apply::delete_account_cache(tx, &blobs, *id)?,
                    EntityDto::Calendar => calendar::delete_calendar(tx, *id)?,
                    EntityDto::Event => calendar::delete_event(tx, *id)?,
                    _ => {}
                }
            }
            for (source, target) in &plan.merges {
                threads.extend(apply::merge_thread(tx, *source, *target)?);
            }
            apply::derive_threads(tx, &threads)?;
            changes::append_all(tx, &plan.log)?;
            db::set_cursor(tx, next)?;
            changes::head(tx, epoch)
        })
        .await?;
    Ok(head)
}

async fn own_addresses(store: &SqliteStore) -> Result<BTreeSet<String>> {
    Ok(store
        .config()
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT lower(address) FROM identity UNION SELECT lower(email) FROM account WHERE email IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await?)
}

fn from_own(arrival: &Arrival, own: &BTreeSet<String>) -> bool {
    let from: Vec<AddressDto> = arrival
        .from_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    from.iter().any(|a| own.contains(&a.email.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn change(entity: EntityDto, id: i64, op: ChangeOpDto) -> ChangeDto {
        ChangeDto {
            seq: 1,
            entity,
            id,
            op,
            payload: None,
        }
    }

    #[test]
    fn plan_requests_changed_ids_and_skips_deleted_ones() {
        let mut writes = Vec::new();
        let changes = [
            change(EntityDto::Message, 1, ChangeOpDto::Upsert),
            change(EntityDto::Message, 2, ChangeOpDto::Upsert),
            change(EntityDto::Message, 2, ChangeOpDto::Delete),
            change(EntityDto::Thread, 5, ChangeOpDto::Merged { target: 6 }),
            change(EntityDto::Mailbox, 9, ChangeOpDto::Bulk { mailbox: 9 }),
            change(EntityDto::Op, 0, ChangeOpDto::Upsert),
        ];
        let plan = Plan::new(&changes, &mut writes);

        assert_eq!(writes.len(), 1);
        assert_eq!(plan.log.len(), 5);
        assert_eq!(plan.merges, vec![(5, 6)]);
        assert_eq!(plan.bulk, BTreeMap::from([(9, None)]));
        let requests = plan.requests(90, false);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages, vec![1]);
        assert_eq!(requests[0].threads, vec![6]);
        assert_eq!(requests[0].window_days, Some(90));
    }

    #[test]
    fn bulk_changes_that_added_uids_request_only_their_range() {
        let bulk = |mailbox: i64, payload: Option<Value>| ChangeDto {
            payload,
            ..change(EntityDto::Mailbox, mailbox, ChangeOpDto::Bulk { mailbox })
        };
        let changes = [
            bulk(1, Some(json!({ "uids": [601, 1100] }))),
            bulk(1, Some(json!({ "uids": [101, 600] }))),
            bulk(2, Some(json!({ "uids": [5, 9] }))),
            bulk(2, None),
            bulk(3, None),
            bulk(3, Some(json!({ "uids": [5, 9] }))),
            bulk(4, Some(json!({ "uids": [1201, 1700] }))),
            bulk(4, Some(json!({ "uids": [1, 100] }))),
            bulk(4, Some(json!({ "uids": [90, 120] }))),
        ];
        let plan = Plan::new(&changes, &mut Vec::new());

        assert_eq!(
            plan.bulk,
            BTreeMap::from([
                (1, Some(vec![[101, 1100]])),
                (2, None),
                (3, None),
                (4, Some(vec![[1, 120], [1201, 1700]])),
            ])
        );
    }

    #[test]
    fn calendar_changes_request_rows_and_unknown_ones_are_skipped() {
        let mut writes = Vec::new();
        let changes = [
            change(EntityDto::Calendar, 2, ChangeOpDto::Upsert),
            change(EntityDto::Event, 7, ChangeOpDto::Upsert),
            change(EntityDto::Event, 8, ChangeOpDto::Delete),
            change(EntityDto::CalOp, 3, ChangeOpDto::Upsert),
            change(EntityDto::Reminder, 4, ChangeOpDto::Upsert),
            change(EntityDto::Unknown, 5, ChangeOpDto::Upsert),
        ];
        let plan = Plan::new(&changes, &mut writes);

        assert_eq!(
            writes.len(),
            2,
            "cal_op goes to the intents, reminder to notifications"
        );
        assert_eq!(plan.log.len(), 3);
        assert_eq!(plan.deletes, vec![(EntityDto::Event, 8)]);
        let requests = plan.requests(90, true);
        assert_eq!(requests[0].calendars, vec![2]);
        assert_eq!(requests[0].events, vec![7]);
        assert_eq!(requests[0].include, vec!["calendar".to_owned()]);
    }

    #[test]
    fn plan_chunks_large_id_sets() {
        let changes: Vec<ChangeDto> = (0..(MAX_ROWS_IDS as i64 * 2 + 1))
            .map(|id| change(EntityDto::Message, id, ChangeOpDto::Upsert))
            .chain([change(EntityDto::Thread, 1, ChangeOpDto::Upsert)])
            .collect();
        let requests = Plan::new(&changes, &mut Vec::new()).requests(90, false);

        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|r| r.messages.len() <= MAX_ROWS_IDS));
        assert_eq!(requests[0].threads, vec![1]);
        assert!(requests[1].threads.is_empty());
    }

    #[test]
    fn own_senders_are_not_arrivals() {
        let own = BTreeSet::from(["me@example.org".to_owned()]);
        let arrival = |from: &str| Arrival {
            message_id: 1,
            account_id: 1,
            thread_id: None,
            subject: None,
            from_json: Some(format!(r#"[{{"email":"{from}"}}]"#)),
        };
        assert!(from_own(&arrival("Me@Example.org"), &own));
        assert!(!from_own(&arrival("them@example.org"), &own));
    }
}
