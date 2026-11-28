//! The calendar sync runner (appointments plan §4 M-B step 4): every
//! account with a discovered calendar home is polled in turn. A calendar
//! with a stored sync token gets a `sync-collection` report; otherwise an
//! unchanged CTag skips it, and without one the etag listing is diffed
//! against the cache (the main path on servers like Horde). Changed
//! resources are fetched by multiget, and their instances are expanded for
//! the window around today.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub(crate) use tagmail_calendar::Window;
use tagmail_core::AccountId;
use tagmail_dav::{CalendarInfo, DavError, MultigetItem, ResourceRef};
use tagmail_store::CacheDb;
use tagmail_store::calendar::{self, CaldavStatus, CalendarRow, EventRow, InstanceRow};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};
use url::Url;

use super::invite::{self, MailAccess};
use super::{CalAccount, CalDav, DavConnector, executor};
use crate::supervisor::{Stores, jitter};

/// Time between two polls of every account.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Time between two refreshes of the authserv-id suggestions.
const SUGGESTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Resources fetched by one multiget and written in one transaction.
const MULTIGET_BATCH: usize = tagmail_dav::MULTIGET_BATCH;
/// Recurring events re-expanded per transaction when the window slides.
const SLIDE_BATCH: usize = 50;
/// `sync-collection` pages followed in one sync before falling back to the
/// etag listing; a server that keeps answering 507 never finishes.
const MAX_SYNC_PAGES: usize = 100;
/// Resources in one `sync-collection` delta from which it is checked
/// against the etag listing.
const FULL_LISTING_THRESHOLD: usize = 500;
/// `sync-collection` answers that mean the report cannot be used on this
/// calendar: bad request, forbidden, not implemented.
const REPORT_UNUSABLE_STATUS: [u16; 3] = [400, 403, 501];

#[derive(Debug, thiserror::Error)]
pub(crate) enum SyncError {
    #[error(transparent)]
    Dav(#[from] DavError),
    #[error(transparent)]
    Store(#[from] tagmail_store::Error),
    #[error("{0}")]
    Engine(#[from] crate::Error),
    #[error("the account is being removed")]
    Removed,
}

type SyncResult<T> = Result<T, SyncError>;

/// The durable key of a collection or resource: its path, as the server
/// spells it.
pub(crate) fn href(url: &Url) -> String {
    url.path().to_owned()
}

fn unix(t: OffsetDateTime) -> i64 {
    t.unix_timestamp()
}

/// A calendar object resource ready for `cache.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub event: EventRow,
    pub ics: Vec<u8>,
    pub instances: Vec<InstanceRow>,
}

/// The instances of `cal` in `window`, and the window edge to record: the
/// window's end while a recurring series may continue past it, `None` once
/// all its instances are cached.
fn instances(
    cal: &tagmail_calendar::Calendar,
    window: &Window,
    href: &str,
) -> (Vec<InstanceRow>, Option<i64>) {
    let (rows, truncated, edge) = cal.instances(window);
    if truncated {
        tracing::warn!(
            href,
            "recurrence expansion was truncated; instances may be missing"
        );
    }
    let rows = rows
        .into_iter()
        .map(|i| InstanceRow {
            start: i.start,
            end: i.end,
            recurrence_id: i.recurrence_id,
            all_day: i.all_day,
        })
        .collect();
    (rows, edge)
}

/// Parses one resource. `None` for one without a usable VEVENT (a VTODO
/// list, garbage), which is not cached.
pub(crate) fn parse(
    href: &str,
    etag: Option<String>,
    data: &str,
    me: &[String],
    window: &Window,
) -> Option<Parsed> {
    let cal = match tagmail_calendar::parse(data.as_bytes()) {
        Ok(cal) => cal,
        Err(e) => {
            tracing::debug!(href, error = %e, "calendar resource skipped");
            return None;
        }
    };
    let summary = cal.summary(me)?;
    let (instances, window_edge) = instances(&cal, window, href);
    let event = EventRow {
        href: href.to_owned(),
        etag,
        uid: summary.uid,
        sequence: summary.sequence,
        dtstamp: summary.dtstamp,
        status: summary.status,
        my_partstat: summary.my_partstat,
        organizer: summary.organizer,
        is_recurring: summary.is_recurring,
        first_start: summary.first_start,
        last_end: summary.last_end,
        window_edge,
    };
    Some(Parsed {
        instances,
        event,
        ics: data.as_bytes().to_vec(),
    })
}

/// Accounts being removed; the runner writes nothing more for them. Shared
/// with the engine, checked inside each cache transaction, so a write that
/// commits after the removal's purge started is refused.
pub(crate) type Removing = Arc<Mutex<HashSet<AccountId>>>;

fn removing(removing: &Removing, account: AccountId) -> bool {
    removing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&account)
}

/// Runs `f` in one cache transaction unless `account` is being removed.
async fn write<T, F>(cache: &CacheDb, gone: &Removing, account: AccountId, f: F) -> SyncResult<T>
where
    T: Send + 'static,
    F: FnOnce(&tagmail_store::rusqlite::Transaction<'_>) -> tagmail_store::Result<T>
        + Send
        + 'static,
{
    let gone = gone.clone();
    cache
        .write(move |tx| {
            if removing(&gone, account) {
                return Ok(None);
            }
            f(tx).map(Some)
        })
        .await?
        .ok_or(SyncError::Removed)
}

/// Fetches `changed` from `calendar` in multiget batches and writes each
/// batch in one transaction; returns how many resources were written or
/// removed.
async fn fetch(sync: &CalendarSync<'_>, changed: Vec<Url>) -> SyncResult<usize> {
    let calendar_id = sync.row.id;
    let mut written = 0;
    for chunk in changed.chunks(MULTIGET_BATCH) {
        let items = sync.dav.multiget(&sync.info.href, chunk).await?;
        let me = sync.me.clone();
        let window = sync.window;
        let (puts, skipped, gone) = tokio::task::spawn_blocking(move || {
            let mut puts = Vec::new();
            let mut skipped = Vec::new();
            let mut gone = Vec::new();
            for item in items {
                match item {
                    MultigetItem::Found(r) => {
                        let key = href(&r.href);
                        match parse(&key, r.etag.clone(), &r.data, &me, &window) {
                            Some(parsed) => puts.push(parsed),
                            None => skipped.push((key, r.etag)),
                        }
                    }
                    MultigetItem::Missing(url) => gone.push(href(&url)),
                }
            }
            (puts, skipped, gone)
        })
        .await
        .map_err(|_| tagmail_store::Error::Closed)?;
        written += puts.len() + skipped.len() + gone.len();
        write(sync.cache, sync.removing, sync.row.account, move |tx| {
            for p in &puts {
                calendar::put_event(tx, calendar_id, &p.event, &p.ics, &p.instances)?;
            }
            calendar::skip_resources(tx, calendar_id, &skipped)?;
            calendar::delete_events(tx, calendar_id, &gone)?;
            Ok(())
        })
        .await?;
    }
    Ok(written)
}

async fn cached_etags(
    cache: &CacheDb,
    calendar_id: i64,
) -> SyncResult<HashMap<String, Option<String>>> {
    Ok(cache
        .read(move |c| calendar::event_etags(c, calendar_id))
        .await?)
}

/// Refs whose etag differs from the cached or skipped one.
fn stale(listed: Vec<ResourceRef>, cached: &HashMap<String, Option<String>>) -> Vec<Url> {
    listed
        .into_iter()
        .filter(|r| r.etag.is_none() || cached.get(&href(&r.href)) != Some(&r.etag))
        .map(|r| r.href)
        .collect()
}

/// Errors of `sync-collection` that mean the server cannot run it here.
fn report_unusable(e: &DavError) -> bool {
    match e {
        DavError::ReportUnsupported(_) | DavError::NotFound { .. } | DavError::Forbidden { .. } => {
            true
        }
        DavError::Status { status, .. } => REPORT_UNUSABLE_STATUS.contains(status),
        _ => false,
    }
}

/// A delta whose changed resources number at least
/// [`FULL_LISTING_THRESHOLD`], or include every cached one, is treated as a
/// full listing.
fn full_listing(cached: &HashMap<String, Option<String>>, changed: &HashSet<String>) -> bool {
    changed.len() >= FULL_LISTING_THRESHOLD
        || (!cached.is_empty() && cached.keys().all(|href| changed.contains(href)))
}

/// One calendar's sync, and what it found out about the server.
pub(crate) struct CalendarSync<'a> {
    pub cache: &'a CacheDb,
    pub removing: &'a Removing,
    pub dav: &'a dyn CalDav,
    pub info: &'a CalendarInfo,
    pub row: &'a CalendarRow,
    pub me: &'a Arc<Vec<String>>,
    pub window: Window,
    /// Unix seconds.
    pub now: i64,
}

/// How a calendar was synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Unchanged,
    Incremental(usize),
    Full(usize),
}

impl CalendarSync<'_> {
    /// `broken`: this calendar's `sync-collection` failed before; set when
    /// it fails now, so later polls go straight to the etag listing.
    pub(crate) async fn run(&self, broken: &mut bool) -> SyncResult<Outcome> {
        let use_report = !*broken && self.info.supports_sync_collection;
        let mut fell_back = false;
        if use_report && let Some(token) = &self.row.sync_token {
            match self.incremental(token).await {
                Ok(Some(n)) => return Ok(Outcome::Incremental(n)),
                Ok(None) => {
                    tracing::info!(href = %self.info.href, "sync-collection not usable as a delta; full resync");
                }
                Err(SyncError::Dav(DavError::InvalidSyncToken)) => {
                    tracing::info!(href = %self.info.href, "sync token expired; full resync");
                }
                Err(SyncError::Dav(e)) if report_unusable(&e) => {
                    tracing::info!(href = %self.info.href, error = %e, "sync-collection failed; using etags");
                    *broken = true;
                }
                Err(e) => return Err(e),
            }
            fell_back = true;
        }
        let unchanged_ctag = self.info.ctag.is_some()
            && self.info.ctag == self.row.ctag
            && self.row.synced_at.is_some();
        if unchanged_ctag && !fell_back {
            return Ok(Outcome::Unchanged);
        }
        self.full(use_report && !*broken).await.map(Outcome::Full)
    }

    /// Follows `sync-collection` pages from `token`. `None` when the
    /// answer cannot be trusted as a delta, so the etag listing must decide:
    /// a truncated page without a new token, more than [`MAX_SYNC_PAGES`]
    /// pages, or what looks like a full listing (some servers answer a
    /// stale token that way, and then report no deletions).
    async fn incremental(&self, token: &str) -> SyncResult<Option<usize>> {
        let calendar_id = self.row.id;
        let before = cached_etags(self.cache, calendar_id).await?;
        let mut changed = HashSet::new();
        let mut token = token.to_owned();
        let mut written = 0;
        for _ in 0..MAX_SYNC_PAGES {
            let page = self
                .dav
                .sync_collection(&self.info.href, Some(&token))
                .await?;
            let cached = cached_etags(self.cache, calendar_id).await?;
            let removed: Vec<String> = page.removed.iter().map(href).collect();
            written += removed.len();
            changed.extend(page.changed.iter().map(|r| href(&r.href)));
            if !removed.is_empty() {
                write(self.cache, self.removing, self.row.account, move |tx| {
                    calendar::delete_events(tx, calendar_id, &removed)
                })
                .await?;
            }
            written += fetch(self, stale(page.changed, &cached)).await?;
            let next = page.sync_token.filter(|next| *next != token);
            match (page.truncated, next) {
                (false, next) => {
                    if full_listing(&before, &changed) {
                        tracing::info!(href = %self.info.href, "sync-collection answered a full listing");
                        return Ok(None);
                    }
                    self.done(Some(next.unwrap_or(token))).await?;
                    return Ok(Some(written));
                }
                (true, Some(next)) => token = next,
                (true, None) => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Diffs the etag listing against the cache. A token read with the
    /// calendar list predates the listing, so changes made meanwhile are
    /// reported again by the next report.
    async fn full(&self, keep_token: bool) -> SyncResult<usize> {
        let calendar_id = self.row.id;
        let listed = self.dav.list_etags(&self.info.href).await?;
        let cached = cached_etags(self.cache, calendar_id).await?;
        let present: HashSet<String> = listed.iter().map(|r| href(&r.href)).collect();
        let removed: Vec<String> = cached
            .keys()
            .filter(|h| !present.contains(*h))
            .cloned()
            .collect();
        let mut written = removed.len();
        if !removed.is_empty() {
            write(self.cache, self.removing, self.row.account, move |tx| {
                calendar::delete_events(tx, calendar_id, &removed)
            })
            .await?;
        }
        written += fetch(self, stale(listed, &cached)).await?;
        let token = if keep_token {
            self.info.sync_token.clone()
        } else {
            None
        };
        self.done(token).await?;
        Ok(written)
    }

    async fn done(&self, token: Option<String>) -> SyncResult<()> {
        let (id, ctag, now) = (self.row.id, self.info.ctag.clone(), self.now);
        write(self.cache, self.removing, self.row.account, move |tx| {
            calendar::set_sync_state(tx, id, ctag.as_deref(), token.as_deref(), now)
        })
        .await
    }
}

/// Calendars (by account and href) whose `sync-collection` failed.
pub(crate) type BrokenReports = HashSet<(AccountId, String)>;

/// What one account's sync did; failed calendars do not stop the others.
#[derive(Debug, Default)]
pub(crate) struct AccountSync {
    pub written: usize,
    /// (calendar href, error).
    pub failed: Vec<(String, SyncError)>,
}

impl AccountSync {
    fn error_text(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        Some(
            self.failed
                .iter()
                .map(|(href, e)| format!("{href}: {e}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// Syncs every calendar in the account's home.
pub(crate) async fn sync_account(
    cache: &CacheDb,
    removing: &Removing,
    dav: &dyn CalDav,
    account: &CalAccount,
    window: Window,
    now: i64,
    broken: &mut BrokenReports,
) -> SyncResult<AccountSync> {
    let home = account
        .config
        .home_url
        .as_deref()
        .and_then(|h| Url::parse(h).ok())
        .ok_or(DavError::NoCalendarHome)?;
    let infos: Vec<CalendarInfo> = dav
        .list_calendars(&home)
        .await?
        .into_iter()
        .filter(CalendarInfo::supports_events)
        .collect();
    let id = account.id;
    let listed: Vec<(String, Option<String>, Option<String>)> = infos
        .iter()
        .map(|i| (href(&i.href), i.display_name.clone(), i.color.clone()))
        .collect();
    let rows = write(cache, removing, id, move |tx| {
        let hrefs: Vec<String> = listed.iter().map(|(h, ..)| h.clone()).collect();
        for (href, name, color) in &listed {
            calendar::upsert_calendar(tx, id, href, name.as_deref(), color.as_deref(), true)?;
        }
        calendar::retain_calendars(tx, id, &hrefs)?;
        calendar::calendars(tx, Some(id))
    })
    .await?;
    let me = Arc::new(account.addresses.clone());
    let mut report = AccountSync::default();
    for info in &infos {
        let key = href(&info.href);
        let Some(row) = rows.iter().find(|r| r.href == key) else {
            continue;
        };
        let mut report_broken = broken.contains(&(id, key.clone()));
        let sync = CalendarSync {
            cache,
            removing,
            dav,
            info,
            row,
            me: &me,
            window,
            now,
        };
        match sync.run(&mut report_broken).await {
            Ok(Outcome::Unchanged) => {}
            Ok(Outcome::Incremental(n) | Outcome::Full(n)) => report.written += n,
            Err(SyncError::Removed) => return Err(SyncError::Removed),
            Err(e) => {
                tracing::warn!(account = id.0, href = key, error = %e, "calendar sync failed");
                report.failed.push((key.clone(), e));
            }
        }
        if report_broken {
            broken.insert((id, key));
        }
    }
    Ok(report)
}

/// Re-expands the recurring events whose instances end before the
/// window's end, and drops instances before its start. An event that no
/// longer parses keeps its instances and moves its edge, so it is not
/// tried again every poll.
pub(crate) async fn slide(cache: &CacheDb, window: Window) -> SyncResult<usize> {
    let end = window.end_ts();
    let behind = cache.read(move |c| calendar::events_behind(c, end)).await?;
    let mut slid = 0;
    for chunk in behind.chunks(SLIDE_BATCH) {
        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        slid += ids.len();
        cache
            .write(move |tx| {
                for id in ids {
                    let Some(ics) = calendar::event_ics(tx, id)? else {
                        continue;
                    };
                    let text = String::from_utf8_lossy(&ics);
                    match tagmail_calendar::parse(text.as_bytes()) {
                        Ok(cal) => {
                            let (instances, edge) = instances(&cal, &window, &id.to_string());
                            calendar::replace_instances(tx, id, &instances, edge)?;
                        }
                        Err(e) => {
                            tracing::debug!(event = id, error = %e, "cached event no longer parses");
                            calendar::set_window_edge(tx, id, Some(end))?;
                        }
                    }
                }
                Ok(())
            })
            .await?;
    }
    let start = window.start.unix_timestamp();
    cache
        .write(move |tx| calendar::trim_instances(tx, start))
        .await?;
    Ok(slid)
}

async fn shrink(cache: &CacheDb) {
    let shrunk = cache
        .write(|tx| {
            tx.execute_batch("PRAGMA shrink_memory")?;
            Ok(())
        })
        .await;
    if let Err(e) = shrunk {
        tracing::debug!(error = %e, "cannot shrink cache memory");
    }
}

async fn record(stores: &Stores, id: AccountId, error: Option<String>) {
    let status = if error.is_some() {
        CaldavStatus::Error
    } else {
        CaldavStatus::Ok
    };
    let recorded = stores
        .config
        .write(move |tx| calendar::set_caldav_status(tx, id, status, error.as_deref()))
        .await;
    if let Err(e) = recorded {
        tracing::error!(account = id.0, error = %e, "cannot record the CalDAV status");
    }
}

/// What the runner keeps between polls.
pub(crate) struct Runner {
    pub stores: Stores,
    pub connector: Arc<dyn DavConnector>,
    pub removing: Removing,
    pub broken: BrokenReports,
    pub mail: Arc<dyn MailAccess>,
    /// Wakes [`invitations`] after each poll.
    pub inbox: Arc<Notify>,
}

impl Runner {
    fn connect(&self, account: &CalAccount) -> SyncResult<Box<dyn CalDav>> {
        let host = account
            .config
            .confirmed_host
            .clone()
            .or_else(|| {
                account
                    .config
                    .home_url
                    .as_deref()
                    .and_then(|h| Url::parse(h).ok())
                    .and_then(|u| u.host_str().map(str::to_owned))
            })
            .ok_or(DavError::NoCalendarHome)?;
        Ok(self.connector.connect(&account.login, &host)?)
    }

    async fn sync_one(
        &mut self,
        id: AccountId,
        window: Window,
        now: i64,
    ) -> SyncResult<AccountSync> {
        let account = super::load(&self.stores, id)
            .await?
            .ok_or(crate::Error::NoAccount(id.0))?;
        let dav = self.connect(&account)?;
        sync_account(
            &self.stores.cache,
            &self.removing,
            dav.as_ref(),
            &account,
            window,
            now,
            &mut self.broken,
        )
        .await
    }

    /// Runs the calendar ops that are due.
    pub(crate) async fn run_ops(&mut self) {
        let ops = match executor::due(&self.stores).await {
            Ok(ops) => ops,
            Err(e) => {
                tracing::error!(error = %e, "cannot read calendar ops");
                return;
            }
        };
        let mut targets: Vec<AccountId> = ops.iter().map(|o| o.target).collect();
        targets.sort_unstable();
        targets.dedup();
        for target in targets {
            let of_target: Vec<_> = ops.iter().filter(|o| o.target == target).cloned().collect();
            if removing(&self.removing, target) {
                executor::defer(&self.stores, of_target, "the account is being removed").await;
                continue;
            }
            let account = match super::load(&self.stores, target).await {
                Ok(Some(account)) => account,
                Ok(None) => {
                    executor::defer(&self.stores, of_target, "no such account").await;
                    continue;
                }
                Err(e) => {
                    executor::defer(&self.stores, of_target, &e.to_string()).await;
                    continue;
                }
            };
            let dav = match self.connect(&account) {
                Ok(dav) => dav,
                Err(e) => {
                    executor::defer(&self.stores, of_target, &e.to_string()).await;
                    continue;
                }
            };
            if executor::run_account(&self.stores, dav.as_ref(), &account, of_target).await {
                self.mail.outbox_queued();
            }
        }
    }

    /// One poll of every account, then the window slide.
    pub(crate) async fn poll(&mut self) {
        self.run_ops().await;
        let now = OffsetDateTime::now_utc();
        let window = Window::around(now.date());
        let accounts = match self.stores.config.read(calendar::caldav_accounts).await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::error!(error = %e, "cannot list CalDAV accounts");
                return;
            }
        };
        let keep = accounts.clone();
        if let Err(e) = self
            .stores
            .cache
            .write(move |tx| calendar::retain_accounts(tx, &keep))
            .await
        {
            tracing::error!(error = %e, "cannot drop calendars of removed accounts");
        }
        self.broken
            .retain(|(account, _)| accounts.contains(account));
        let mut written = 0;
        for id in accounts {
            if removing(&self.removing, id) {
                continue;
            }
            match self.sync_one(id, window, unix(now)).await {
                Ok(report) => {
                    written += report.written;
                    record(&self.stores, id, report.error_text()).await;
                }
                Err(SyncError::Removed) => {}
                Err(e) => {
                    tracing::warn!(account = id.0, error = %e, "calendar sync failed");
                    record(&self.stores, id, Some(e.to_string())).await;
                }
            }
        }
        match slide(&self.stores.cache, window).await {
            Ok(slid) => written += slid,
            Err(e) => tracing::error!(error = %e, "cannot slide the calendar window"),
        }
        if written > 0 {
            shrink(&self.stores.cache).await;
        }
        self.inbox.notify_one();
    }
}

/// Processes queued invitations after each poll and refreshes the
/// authserv-id suggestions, beside the runner: reading an uncached
/// invitation waits for the account's IMAP worker, which must not hold up
/// calendar ops. Wakes the runner's `ops` when it queued an apply.
pub(crate) async fn invitations(
    stores: Stores,
    mail: Arc<dyn MailAccess>,
    inbox: Arc<Notify>,
    ops: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut suggested: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            () = inbox.notified() => {}
        }
        let work = async {
            let pass = invite::process_inbox(&stores, mail.as_ref()).await;
            if pass.queued {
                ops.notify_one();
            }
            if pass.more {
                inbox.notify_one();
            }
            if suggested.is_none_or(|at| at.elapsed() >= SUGGESTION_INTERVAL) {
                suggested = Some(tokio::time::Instant::now());
                invite::refresh_suggestions(&stores, mail.as_ref()).await;
            }
        };
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            () = work => {}
        }
    }
}

/// Polls until `stop`, every [`POLL_INTERVAL`] (jittered) or when `wake`d;
/// `ops` and a retried op that is due run the ops alone, without a poll. A
/// stop also cancels a poll in progress.
pub(crate) async fn run(
    mut runner: Runner,
    wake: Arc<Notify>,
    ops: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    executor::recover(&runner.stores).await;
    let mut next_poll = tokio::time::Instant::now();
    loop {
        if *stop.borrow() {
            return;
        }
        if tokio::time::Instant::now() >= next_poll {
            tokio::select! {
                () = runner.poll() => {}
                _ = stop.wait_for(|stop| *stop) => return,
            }
            next_poll = tokio::time::Instant::now() + jitter(POLL_INTERVAL);
        } else {
            tokio::select! {
                () = runner.run_ops() => {}
                _ = stop.wait_for(|stop| *stop) => return,
            }
        }
        let until_op = executor::next_due(&runner.stores).await.map(|at| {
            let wait = at.saturating_sub(crate::executor::now_secs()).max(0);
            tokio::time::Instant::now() + Duration::from_secs(wait.unsigned_abs())
        });
        let sleep_until = until_op.map_or(next_poll, |op| op.min(next_poll));
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            () = wake.notified() => next_poll = tokio::time::Instant::now(),
            () = ops.notified() => {}
            () = tokio::time::sleep_until(sleep_until) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use tagmail_dav::SyncPage;
    use tagmail_store::calendar::CaldavConfig;
    use tempfile::TempDir;
    use time::Date;
    use time::macros::date;
    use zeroize::Zeroizing;

    use super::*;
    use crate::calendar::DavLogin;
    use crate::calendar::fake::{FakeDav, HOME, ME, OTHER, TASK, WORK, event, url};

    const TODAY: Date = date!(2026 - 11 - 25);
    const NOW: i64 = 1_795_000_000;
    /// Weekly from 2026-12-01 to the window end, 400 days after [`TODAY`].
    const WEEKLY_IN_WINDOW: usize = 57;
    const FORBIDDEN: u16 = 403;

    struct Fixture {
        cache: CacheDb,
        removing: Removing,
        dav: FakeDav,
        account: CalAccount,
        broken: BrokenReports,
        _dir: TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = TempDir::new().unwrap();
            Fixture {
                cache: CacheDb::open(&dir.path().join("cache.db")).unwrap(),
                removing: Removing::default(),
                dav: FakeDav::default(),
                account: CalAccount {
                    id: AccountId(1),
                    imap_host: "imap.example.com".into(),
                    email: ME.into(),
                    config: CaldavConfig {
                        home_url: Some(url(HOME).to_string()),
                        ..CaldavConfig::default()
                    },
                    login: DavLogin {
                        username: ME.into(),
                        password: Zeroizing::new("secret".into()),
                        allow_insecure: false,
                    },
                    addresses: vec![ME.into()],
                },
                broken: BrokenReports::new(),
                _dir: dir,
            }
        }

        async fn try_sync(&mut self) -> SyncResult<AccountSync> {
            sync_account(
                &self.cache,
                &self.removing,
                &self.dav,
                &self.account,
                Window::around(TODAY),
                NOW,
                &mut self.broken,
            )
            .await
        }

        async fn sync(&mut self) -> usize {
            let report = self.try_sync().await.unwrap();
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            report.written
        }

        async fn events(&self) -> Vec<(String, u32, Option<String>, usize)> {
            self.cache
                .read(|c| {
                    let mut stmt = c.prepare(
                        "SELECT e.uid, e.sequence, e.my_partstat, \
                         (SELECT count(*) FROM cal_instance i WHERE i.event_id = e.id) \
                         FROM cal_event e ORDER BY e.uid",
                    )?;
                    Ok(stmt
                        .query_map([], |r| {
                            let instances: i64 = r.get(3)?;
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, instances as usize))
                        })?
                        .collect::<tagmail_store::rusqlite::Result<_>>()?)
                })
                .await
                .unwrap()
        }

        async fn uids(&self) -> Vec<String> {
            self.events().await.into_iter().map(|e| e.0).collect()
        }

        async fn edges(&self) -> Vec<Option<i64>> {
            self.cache
                .read(|c| {
                    let mut stmt = c.prepare("SELECT window_edge FROM cal_event ORDER BY uid")?;
                    Ok(stmt
                        .query_map([], |r| r.get(0))?
                        .collect::<tagmail_store::rusqlite::Result<_>>()?)
                })
                .await
                .unwrap()
        }

        async fn calendar(&self) -> CalendarRow {
            self.cache
                .read(|c| calendar::calendars(c, None))
                .await
                .unwrap()
                .into_iter()
                .find(|c| c.href == WORK)
                .unwrap()
        }

        fn truncated_page(&self, token: Option<&str>, name: &str) {
            self.dav.put(name, "\"1\"", &event(name, 0, None));
            self.dav.page(Ok(SyncPage {
                sync_token: token.map(str::to_owned),
                changed: vec![ResourceRef {
                    href: url(&format!("{WORK}{name}")),
                    etag: Some("\"1\"".into()),
                }],
                removed: vec![],
                truncated: true,
            }));
        }
    }

    #[tokio::test]
    async fn etag_listing_fetches_changes_and_drops_deletions() {
        let mut f = Fixture::new();
        f.dav.calendar(None, None);
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.dav.put(
            "weekly.ics",
            "\"1\"",
            &event("weekly", 0, Some("FREQ=WEEKLY")),
        );
        f.dav.put("task.ics", "\"1\"", TASK);
        assert_eq!(f.sync().await, 3);
        assert_eq!(
            f.events().await,
            [
                ("one".into(), 0, Some("TENTATIVE".into()), 1),
                (
                    "weekly".into(),
                    0,
                    Some("TENTATIVE".into()),
                    WEEKLY_IN_WINDOW
                ),
            ]
        );
        let row = f.calendar().await;
        assert_eq!((row.href.as_str(), row.synced_at), (WORK, Some(NOW)));
        assert_eq!(row.display_name.as_deref(), Some("Work"));

        f.dav.took();
        assert_eq!(
            f.sync().await,
            0,
            "the skipped task list is not fetched again"
        );
        assert_eq!(f.dav.took(), ["list_calendars", "list_etags"]);

        f.dav.put("one.ics", "\"2\"", &event("one", 1, None));
        f.dav.delete("weekly.ics");
        f.dav.put("task.ics", "\"2\"", &event("task", 0, None));
        f.sync().await;
        assert_eq!(f.uids().await, ["one", "task"]);
        assert_eq!(f.events().await[0].1, 1);
        f.dav.delete("task.ics");
        f.sync().await;
        let instances: i64 = f
            .cache
            .read(|c| Ok(c.query_row("SELECT count(*) FROM cal_instance", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(instances, 1, "deleted events take their instances along");
    }

    #[tokio::test]
    async fn an_unchanged_ctag_skips_the_listing() {
        let mut f = Fixture::new();
        f.dav.calendar(Some("c1"), None);
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.sync().await;
        f.dav.took();
        assert_eq!(f.sync().await, 0);
        assert_eq!(f.dav.took(), ["list_calendars"]);
        f.dav.calendar(Some("c2"), None);
        f.sync().await;
        assert_eq!(f.dav.took(), ["list_calendars", "list_etags"]);
    }

    #[tokio::test]
    async fn sync_collection_follows_the_token_through_truncated_pages() {
        let mut f = Fixture::new();
        f.dav.calendar(None, Some("t0"));
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.sync().await;
        assert_eq!(f.calendar().await.sync_token.as_deref(), Some("t0"));

        f.truncated_page(Some("t1"), "two.ics");
        f.dav.delete("one.ics");
        f.dav.page(Ok(SyncPage {
            sync_token: Some("t2".into()),
            changed: vec![],
            removed: vec![url(&format!("{WORK}one.ics"))],
            truncated: false,
        }));
        f.dav.took();
        assert_eq!(f.sync().await, 2);
        assert_eq!(
            f.dav.took(),
            [
                "list_calendars",
                "sync_collection",
                "multiget",
                "sync_collection"
            ]
        );
        assert_eq!(f.uids().await, ["two.ics"]);
        assert_eq!(f.calendar().await.sync_token.as_deref(), Some("t2"));
    }

    #[tokio::test]
    async fn a_truncated_page_without_a_new_token_falls_back_to_etags() {
        for stalled in [None, Some("t0")] {
            let mut f = Fixture::new();
            f.dav.calendar(None, Some("t0"));
            f.sync().await;
            f.truncated_page(stalled, "two.ics");
            f.dav.took();
            f.sync().await;
            assert_eq!(
                f.dav.took(),
                [
                    "list_calendars",
                    "sync_collection",
                    "multiget",
                    "list_etags"
                ],
                "{stalled:?}"
            );
            assert_eq!(f.calendar().await.sync_token.as_deref(), Some("t0"));
            assert_eq!(f.uids().await, ["two.ics"]);
        }
    }

    #[tokio::test]
    async fn endless_paging_stops_after_the_page_cap() {
        let mut f = Fixture::new();
        f.dav.calendar(None, Some("t0"));
        f.sync().await;
        for page in 1..=MAX_SYNC_PAGES {
            f.truncated_page(Some(&format!("t{page}")), &format!("e{page}.ics"));
        }
        f.dav.took();
        f.sync().await;
        let calls = f.dav.took();
        let reports = calls.iter().filter(|c| **c == "sync_collection").count();
        assert_eq!(reports, MAX_SYNC_PAGES);
        assert_eq!(calls.last(), Some(&"list_etags"));
    }

    #[tokio::test]
    async fn an_invalid_token_resyncs_everything() {
        let mut f = Fixture::new();
        f.dav.calendar(None, Some("t0"));
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.sync().await;
        f.dav.calendar(None, Some("t5"));
        f.dav.page(Err(DavError::InvalidSyncToken));
        f.dav.took();
        f.sync().await;
        assert_eq!(
            f.dav.took(),
            ["list_calendars", "sync_collection", "list_etags"]
        );
        assert_eq!(f.calendar().await.sync_token.as_deref(), Some("t5"));
    }

    #[tokio::test]
    async fn a_failing_report_falls_back_to_etags_for_good() {
        let refusals = [
            DavError::NotFound { url: url(WORK) },
            DavError::Forbidden { url: url(WORK) },
            DavError::Status {
                status: FORBIDDEN,
                url: url(WORK),
            },
        ];
        for refusal in refusals {
            let mut f = Fixture::new();
            f.dav.calendar(None, Some("t0"));
            f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
            f.sync().await;
            let what = refusal.to_string();
            f.dav.page(Err(refusal));
            f.dav.put("two.ics", "\"1\"", &event("two", 0, None));
            f.dav.took();
            f.sync().await;
            assert_eq!(
                f.dav.took(),
                [
                    "list_calendars",
                    "sync_collection",
                    "list_etags",
                    "multiget"
                ],
                "{what}"
            );
            assert_eq!(f.calendar().await.sync_token, None);
            f.sync().await;
            assert_eq!(f.dav.took(), ["list_calendars", "list_etags"]);
            assert_eq!(f.uids().await, ["one", "two"]);
        }
    }

    #[tokio::test]
    async fn one_failing_calendar_does_not_stop_the_others() {
        let mut f = Fixture::new();
        f.dav.calendar(None, None);
        f.dav.add_calendar(OTHER, None, None);
        f.dav
            .put_in(OTHER, "other.ics", "\"1\"", &event("other", 0, None));
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.dav.fail_listing(WORK);
        let report = f.try_sync().await.unwrap();
        assert_eq!(report.written, 1);
        assert_eq!(report.failed.len(), 1);
        assert!(report.error_text().unwrap().starts_with(WORK));
        assert_eq!(f.uids().await, ["other"]);
    }

    #[tokio::test]
    async fn nothing_is_written_for_an_account_being_removed() {
        let mut f = Fixture::new();
        f.dav.calendar(None, None);
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.removing.lock().unwrap().insert(f.account.id);
        assert!(matches!(f.try_sync().await, Err(SyncError::Removed)));
        let calendars = f
            .cache
            .read(|c| calendar::calendars(c, None))
            .await
            .unwrap();
        assert!(calendars.is_empty());
    }

    #[tokio::test]
    async fn the_slide_re_expands_only_open_ended_series_behind_the_window() {
        let mut f = Fixture::new();
        f.dav.calendar(None, None);
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.dav.put(
            "short.ics",
            "\"1\"",
            &event("short", 0, Some("FREQ=WEEKLY;COUNT=2")),
        );
        f.dav.put(
            "weekly.ics",
            "\"1\"",
            &event("weekly", 0, Some("FREQ=WEEKLY")),
        );
        f.sync().await;
        let window = Window::around(TODAY);
        assert_eq!(f.edges().await, [None, None, Some(window.end_ts())]);
        assert_eq!(slide(&f.cache, window).await.unwrap(), 0);

        let later = Window::around(TODAY + time::Duration::days(7));
        assert_eq!(slide(&f.cache, later).await.unwrap(), 1);
        assert_eq!(f.edges().await, [None, None, Some(later.end_ts())]);
        assert_eq!(f.events().await[2].3, WEEKLY_IN_WINDOW + 1);
        assert_eq!(slide(&f.cache, later).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_unparsable_event_still_moves_its_edge() {
        let mut f = Fixture::new();
        f.dav.calendar(None, None);
        f.dav.put(
            "weekly.ics",
            "\"1\"",
            &event("weekly", 0, Some("FREQ=WEEKLY")),
        );
        f.sync().await;
        let calendar_id = f.calendar().await.id;
        f.cache
            .write(move |tx| {
                let garbage = EventRow {
                    href: format!("{WORK}weekly.ics"),
                    uid: "weekly".into(),
                    is_recurring: true,
                    window_edge: Some(0),
                    ..EventRow::default()
                };
                calendar::put_event(tx, calendar_id, &garbage, b"not a calendar", &[])?;
                Ok(())
            })
            .await
            .unwrap();
        let window = Window::around(TODAY);
        assert_eq!(slide(&f.cache, window).await.unwrap(), 1);
        assert_eq!(f.edges().await, [Some(window.end_ts())]);
        assert_eq!(slide(&f.cache, window).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_full_listing_for_a_stale_token_drops_what_it_omits() {
        let mut f = Fixture::new();
        f.dav.calendar(None, Some("t0"));
        f.dav.put("one.ics", "\"1\"", &event("one", 0, None));
        f.dav.put("two.ics", "\"1\"", &event("two", 0, None));
        f.sync().await;
        f.dav.delete("two.ics");
        f.dav.page(Ok(SyncPage {
            sync_token: Some("t9".into()),
            changed: vec![
                ResourceRef {
                    href: url(&format!("{WORK}one.ics")),
                    etag: Some("\"1\"".into()),
                },
                ResourceRef {
                    href: url(&format!("{WORK}two.ics")),
                    etag: Some("\"1\"".into()),
                },
            ],
            removed: vec![],
            truncated: false,
        }));
        f.dav.took();
        f.sync().await;
        assert_eq!(
            f.dav.took(),
            ["list_calendars", "sync_collection", "list_etags"]
        );
        assert_eq!(f.uids().await, ["one"]);
    }
}
