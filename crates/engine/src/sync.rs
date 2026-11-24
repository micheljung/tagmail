//! Mailbox sync over one IMAP session (plan §3.4): resumable newest-first
//! initial sync with a modseq snapshot and catch-up, then incremental sync
//! by QRESYNC, CONDSTORE or a full flag diff.

use std::collections::BTreeSet;

use tagmail_core::SpecialUse;
use tagmail_imap::{
    FetchSpec, ImapSession, MessageMeta, QresyncParams, SearchCriteria, SelectOptions,
    SelectedMailbox, Uid, UidSet, UidValidity, WINDOW, windows_newest_first,
};
use tagmail_store::BlobKey;

use crate::cache::{self, ListedMailbox, MailboxRow, SyncState};
use crate::ctx::Ctx;
use crate::error::Result;
use crate::message::{self, Fetched, LocationState};

/// Initial-sync windows per mailbox that also fetch a snippet; older
/// messages get theirs from the text pass.
pub const SNIPPET_WINDOWS: usize = 2;
/// UIDs per FLAGS-only FETCH when diffing without modseqs.
pub const FLAGS_CHUNK: usize = 5000;
/// Encoded bytes of a text part fetched for the text blob and FTS. Leaves
/// room for transfer encoding and HTML markup above the 64 KiB text cap.
pub const TEXT_FETCH_CAP: u64 = 256 * 1024;

/// How an initial sync ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Done,
    /// Stopped between windows because `interrupt` asked to; the cursor is
    /// committed, so the next run resumes there.
    Paused,
}

/// LISTs the account's mailboxes, reconciles the cache and returns the
/// selectable ones in sync order: INBOX, Sent, Archive/All, Drafts, others.
pub async fn list<S: ImapSession>(s: &mut S, ctx: &Ctx) -> Result<Vec<MailboxRow>> {
    let listed: Vec<ListedMailbox> = s
        .list()
        .await?
        .into_iter()
        .filter(|m| m.selectable)
        .map(|m| ListedMailbox {
            name: m.name,
            delimiter: m.delimiter,
            special_use: m.special_use,
        })
        .collect();
    ctx.write(move |w| cache::sync_mailbox_list(w, &listed))
        .await?;
    ordered(ctx).await
}

/// The account's mailbox rows in sync order.
pub async fn ordered(ctx: &Ctx) -> Result<Vec<MailboxRow>> {
    let account = ctx.account.0;
    let mut rows = ctx.read(move |c| cache::mailboxes(c, account)).await?;
    rows.sort_by_key(|m| (rank(m), m.name.clone()));
    Ok(rows)
}

fn rank(m: &MailboxRow) -> u8 {
    if m.is_inbox() {
        return 0;
    }
    match m.special_use {
        Some(SpecialUse::Sent) => 1,
        Some(SpecialUse::Archive | SpecialUse::All) => 2,
        Some(SpecialUse::Drafts) => 3,
        _ => 4,
    }
}

async fn reload(ctx: &Ctx, mailbox: i64) -> Result<MailboxRow> {
    ctx.read(move |c| cache::mailbox(c, mailbox)).await
}

fn parse(metas: &[MessageMeta]) -> Vec<Fetched> {
    metas.iter().filter_map(message::fetched).collect()
}

fn uid_range(from: u32, to: u32) -> UidSet {
    let mut set = UidSet::new();
    if from <= to && to > 0 {
        set.insert_range(from.max(1), to);
    }
    set
}

fn below(set: &UidSet, bound: u32) -> UidSet {
    if bound <= 1 {
        return UidSet::new();
    }
    let mut out = UidSet::new();
    for &(lo, hi) in set.ranges() {
        if lo < bound {
            out.insert_range(lo, hi.min(bound - 1));
        }
    }
    out
}

/// UIDNEXT, or one past the highest UID when the server omitted it.
pub(crate) async fn uid_next<S: ImapSession>(s: &mut S, sel: &SelectedMailbox) -> Result<u32> {
    if let Some(next) = sel.uid_next {
        return Ok(next.0);
    }
    let all = s.uid_search(SearchCriteria::All).await?;
    Ok(all.max().map_or(1, |u| u.0.saturating_add(1)))
}

fn modseq_usable<S: ImapSession>(s: &S, sel: &SelectedMailbox) -> bool {
    s.capabilities().condstore() && !sel.nomodseq && sel.highest_modseq.is_some()
}

/// Syncs one mailbox: initial (or resumed) sync, else incremental.
pub async fn sync_mailbox<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    mailbox: &MailboxRow,
    interrupt: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Progress> {
    let result = match mailbox.state {
        SyncState::Live => incremental(s, ctx, mailbox, interrupt).await,
        SyncState::Initial | SyncState::Catchup => initial(s, ctx, mailbox, interrupt).await,
    };
    s.take_events();
    result
}

/// Fetches and stores full metadata for `uids` in newest-first windows.
async fn fetch_new<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    mailbox: i64,
    uids: &UidSet,
    bulk: bool,
) -> Result<()> {
    for window in windows_newest_first(uids, WINDOW) {
        let metas = s.fetch(&window, &FetchSpec::sync_window(false)).await?;
        let fetched = parse(&metas);
        ctx.write(move |w| {
            cache::add_messages(w, mailbox, &fetched)?;
            if bulk {
                w.bulk_added(mailbox, &window);
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

async fn initial<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    mailbox: &MailboxRow,
    interrupt: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Progress> {
    let id = mailbox.id;
    let sel = s.select(&mailbox.name, SelectOptions::default()).await?;
    let mut row = mailbox.clone();
    let fresh = row.uidvalidity != sel.uid_validity.0 || row.uidnext.is_none();
    if fresh {
        if row.uidvalidity != 0 && row.uidvalidity != sel.uid_validity.0 {
            tracing::info!(mailbox = row.name, "UIDVALIDITY changed, resyncing");
            ctx.write(move |w| cache::reset_mailbox(w, id)).await?;
        }
        let bound = uid_next(s, &sel).await?;
        let snapshot = sel
            .highest_modseq
            .filter(|_| modseq_usable(s, &sel))
            .map(|m| m.0);
        let (uidvalidity, nomodseq, kw) = (sel.uid_validity.0, sel.nomodseq, sel.keyword_capable);
        ctx.write(move |w| cache::start_initial(w, id, uidvalidity, bound, snapshot, nomodseq, kw))
            .await?;
        row = reload(ctx, id).await?;
    }
    let bound = row.uidnext.unwrap_or(1);

    if row.state == SyncState::Initial {
        let all = s.uid_search(SearchCriteria::All).await?;
        let limit = row.cursor.map_or(bound, |c| c.min(bound));
        let pending = below(&all, limit);
        let done_windows = usize::from(row.cursor.is_some());
        for (i, window) in windows_newest_first(&pending, WINDOW)
            .into_iter()
            .enumerate()
        {
            if interrupt() {
                return Ok(Progress::Paused);
            }
            let snippet = i + done_windows < SNIPPET_WINDOWS;
            let metas = s.fetch(&window, &FetchSpec::sync_window(snippet)).await?;
            let fetched = parse(&metas);
            let cursor = window.min().map_or(1, |u| u.0);
            ctx.write(move |w| {
                cache::add_messages(w, id, &fetched)?;
                cache::set_cursor(w, id, cursor)?;
                w.bulk_added(id, &window);
                Ok(())
            })
            .await?;
        }
        ctx.write(move |w| cache::set_state(w, id, SyncState::Catchup))
            .await?;
    }

    // Catch-up: whatever changed on the server while the windows ran.
    let sel = s.select(&row.name, SelectOptions::default()).await?;
    if sel.uid_validity.0 != row.uidvalidity {
        ctx.write(move |w| cache::reset_mailbox(w, id)).await?;
        return Box::pin(initial(s, ctx, &reload(ctx, id).await?, interrupt)).await;
    }
    let next = uid_next(s, &sel).await?;
    fetch_new(s, ctx, id, &uid_range(bound, next.saturating_sub(1)), true).await?;
    let old = uid_range(1, bound.saturating_sub(1));
    let mut vanished_known = false;
    match row.snapshot {
        Some(snapshot) if modseq_usable(s, &sel) && !old.is_empty() => {
            let changes = s
                .fetch_changed_since(&old, tagmail_imap::ModSeq(snapshot), &FetchSpec::flags())
                .await?;
            let states = location_states(&changes.messages);
            let vanished = changes.vanished;
            vanished_known = s.qresync_enabled();
            ctx.write(move |w| {
                cache::update_flags(w, id, &states)?;
                cache::remove_uids(w, id, &vanished)?;
                Ok(())
            })
            .await?;
        }
        _ => flag_diff(s, ctx, id).await?,
    }
    if !vanished_known {
        expunge_diff(s, ctx, id).await?;
    }
    let hms = sel
        .highest_modseq
        .filter(|_| modseq_usable(s, &sel))
        .map(|m| m.0);
    let (nomodseq, kw) = (sel.nomodseq, sel.keyword_capable);
    ctx.write(move |w| cache::finish_sync(w, id, hms, next, nomodseq, kw))
        .await?;
    Ok(Progress::Done)
}

fn location_states(metas: &[MessageMeta]) -> Vec<LocationState> {
    parse(metas).into_iter().map(|f| f.location).collect()
}

/// Re-reads FLAGS of every known location in chunks and stores changes.
async fn flag_diff<S: ImapSession>(s: &mut S, ctx: &Ctx, id: i64) -> Result<()> {
    let known = ctx.read(move |c| cache::known_uids(c, id)).await?;
    for chunk in windows_newest_first(&known, FLAGS_CHUNK) {
        let metas = s.fetch(&chunk, &FetchSpec::flags()).await?;
        let states = location_states(&metas);
        ctx.write(move |w| cache::update_flags(w, id, &states).map(drop))
            .await?;
    }
    Ok(())
}

/// Removes locations whose UIDs the server no longer has.
async fn expunge_diff<S: ImapSession>(s: &mut S, ctx: &Ctx, id: i64) -> Result<()> {
    let all = s.uid_search(SearchCriteria::All).await?;
    let known = ctx.read(move |c| cache::known_uids(c, id)).await?;
    let gone = known.difference(&all);
    if !gone.is_empty() {
        ctx.write(move |w| cache::remove_uids(w, id, &gone).map(drop))
            .await?;
    }
    Ok(())
}

async fn incremental<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    row: &MailboxRow,
    interrupt: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Progress> {
    let id = row.id;
    let stored_next = row.uidnext.unwrap_or(1);
    let qresync = s.qresync_enabled() && row.highestmodseq.is_some() && !row.nomodseq;

    if !row.is_inbox()
        && s.capabilities().condstore()
        && let Some(hms) = row.highestmodseq
        && !row.nomodseq
    {
        let status = s.status(&row.name).await?;
        let count = ctx.read(move |c| cache::location_count(c, id)).await?;
        let unchanged = status.highest_modseq.map(|m| m.0) == Some(hms)
            && status.uid_next.map(|u| u.0) == Some(stored_next)
            && status.messages == Some(count)
            && status.uid_validity.is_none_or(|v| v.0 == row.uidvalidity);
        if unchanged {
            ctx.write(move |w| cache::mark_polled(w, id)).await?;
            return Ok(Progress::Done);
        }
    }

    let options = SelectOptions {
        read_only: false,
        qresync: qresync.then(|| QresyncParams {
            uid_validity: UidValidity(row.uidvalidity),
            modseq: tagmail_imap::ModSeq(row.highestmodseq.unwrap_or(0)),
            known_uids: None,
        }),
    };
    let sel = s.select(&row.name, options).await?;
    if sel.uid_validity.0 != row.uidvalidity {
        tracing::info!(mailbox = row.name, "UIDVALIDITY changed, resyncing");
        ctx.write(move |w| cache::reset_mailbox(w, id)).await?;
        let fresh = reload(ctx, id).await?;
        return initial(s, ctx, &fresh, interrupt).await;
    }
    let next = uid_next(s, &sel).await?;
    let usable = modseq_usable(s, &sel);
    let mut vanished_known = false;

    if qresync && usable {
        let states: Vec<LocationState> = location_states(&sel.changed)
            .into_iter()
            .filter(|l| l.uid < stored_next)
            .collect();
        let vanished = sel.vanished.clone();
        vanished_known = true;
        ctx.write(move |w| {
            cache::remove_uids(w, id, &vanished)?;
            cache::update_flags(w, id, &states)?;
            Ok(())
        })
        .await?;
    } else if usable && row.highestmodseq.is_some() {
        let hms = row.highestmodseq.unwrap_or(0);
        if sel.highest_modseq.map(|m| m.0) != Some(hms) {
            let old = uid_range(1, stored_next.saturating_sub(1));
            if !old.is_empty() {
                let changes = s
                    .fetch_changed_since(&old, tagmail_imap::ModSeq(hms), &FetchSpec::flags())
                    .await?;
                let states = location_states(&changes.messages);
                ctx.write(move |w| cache::update_flags(w, id, &states).map(drop))
                    .await?;
            }
        }
    } else {
        flag_diff(s, ctx, id).await?;
    }

    fetch_new(
        s,
        ctx,
        id,
        &uid_range(stored_next, next.saturating_sub(1)),
        false,
    )
    .await?;

    if !vanished_known {
        let count = ctx.read(move |c| cache::location_count(c, id)).await?;
        if count != sel.exists || !usable {
            expunge_diff(s, ctx, id).await?;
        }
    }
    let hms = sel.highest_modseq.filter(|_| usable).map(|m| m.0);
    let (nomodseq, kw) = (sel.nomodseq, sel.keyword_capable);
    ctx.write(move |w| cache::finish_sync(w, id, hms, next, nomodseq, kw))
        .await?;
    Ok(Progress::Done)
}

/// Whether a message outside `skip` still lacks its text.
pub async fn text_pending(ctx: &Ctx, skip: &BTreeSet<i64>) -> Result<bool> {
    let account = ctx.account.0;
    let exclude = skip.clone();
    let pending = ctx
        .read(move |c| cache::pending_text(c, account, 1, &exclude))
        .await?;
    Ok(!pending.is_empty())
}

/// Fetches the text of up to `limit` messages that lack it, newest first:
/// stores the text blob, fills missing snippets and indexes the body.
/// Messages that cannot be fetched now are added to `skip`. Returns how
/// many were attempted.
pub async fn text_pass<S: ImapSession>(
    s: &mut S,
    ctx: &Ctx,
    limit: usize,
    skip: &mut BTreeSet<i64>,
) -> Result<usize> {
    let account = ctx.account;
    let exclude = skip.clone();
    let pending = ctx
        .read(move |c| cache::pending_text(c, account.0, limit, &exclude))
        .await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let rows = ctx.read(move |c| cache::mailboxes(c, account.0)).await?;
    let mut by_mailbox: Vec<(i64, Vec<cache::PendingText>)> = Vec::new();
    for p in &pending {
        match by_mailbox.iter_mut().find(|(m, _)| *m == p.mailbox) {
            Some((_, v)) => v.push(p.clone()),
            None => by_mailbox.push((p.mailbox, vec![p.clone()])),
        }
    }
    for (mailbox, items) in by_mailbox {
        let Some(row) = rows.iter().find(|r| r.id == mailbox) else {
            continue;
        };
        if s.selected() != Some(row.name.as_str()) {
            s.select(&row.name, SelectOptions::default()).await?;
        }
        let uids: UidSet = items.iter().map(|p| Uid(p.uid)).collect();
        let spec = FetchSpec {
            body_structure: true,
            ..FetchSpec::default()
        };
        let metas = s.fetch(&uids, &spec).await?;
        for item in items {
            skip.insert(item.message);
            let body = metas
                .iter()
                .find(|m| m.uid == Some(Uid(item.uid)))
                .and_then(|m| m.body.clone());
            let Some(body) = body else {
                continue;
            };
            let text = match body.text_part() {
                Some(part) => {
                    let mut raw = Vec::new();
                    match s
                        .fetch_body(Uid(item.uid), &part.section, &mut raw, TEXT_FETCH_CAP)
                        .await
                    {
                        Ok(_) => message::decode_part(part, &raw),
                        Err(e) if e.is_fatal() => return Err(e.into()),
                        Err(e) => {
                            tracing::debug!(error = %e, uid = item.uid, "text fetch failed");
                            continue;
                        }
                    }
                }
                None => String::new(),
            };
            let Ok(email_id) = tagmail_core::EmailId::parse(&item.email_id) else {
                continue;
            };
            let key = BlobKey::for_message(account, &email_id);
            let blobs = ctx.blobs.clone();
            {
                let key = key.clone();
                let text = text.clone();
                ctx.write_blobs(move || blobs.put_text(&key, &text)).await?;
            }
            let kept = text[..text.floor_char_boundary(tagmail_store::TEXT_CAP)].to_owned();
            let snippet = message::snippet_of(&kept);
            let message_id = item.message;
            ctx.write(move |w| cache::set_text(w, message_id, key.as_str(), &snippet, &kept))
                .await?;
        }
    }
    Ok(pending.len())
}

/// Deletes orphans whose grace period ended, with their blobs.
pub async fn purge(ctx: &Ctx) -> Result<usize> {
    let purged = ctx.write(cache::purge_orphans).await?;
    let n = purged.len();
    delete_blobs(ctx, purged).await?;
    Ok(n)
}

/// Deletes the text and raw blobs of the account's messages `email_ids`.
pub(crate) async fn delete_blobs(ctx: &Ctx, email_ids: Vec<String>) -> Result<()> {
    if email_ids.is_empty() {
        return Ok(());
    }
    let blobs = ctx.blobs.clone();
    let account = ctx.account;
    ctx.write_blobs(move || {
        for email_id in email_ids {
            if let Ok(id) = tagmail_core::EmailId::parse(&email_id) {
                let key = BlobKey::for_message(account, &id);
                blobs.delete_text(&key)?;
                blobs.delete_raw(&key)?;
            }
        }
        Ok(())
    })
    .await
}
