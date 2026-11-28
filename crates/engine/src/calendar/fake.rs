//! In-memory CalDAV server and connector for the calendar tests.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tagmail_dav::{
    CalendarInfo, DavError, Discovery, DiscoveryTarget, MultigetItem, PutCondition, Resource,
    ResourceRef, SyncPage, UidConflictKind, UidMatch,
};
use url::Url;

use super::{CalDav, DavConnector, DavFuture, DavLogin};

pub(crate) const BASE: &str = "https://dav.example.com";
pub(crate) const HOME: &str = "/home/";
pub(crate) const WORK: &str = "/home/work/";
pub(crate) const OTHER: &str = "/home/other/";
pub(crate) const ME: &str = "me@example.com";
const SERVER_ERROR: u16 = 500;
pub(crate) const TASK: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t1\r\n\
     DTSTAMP:20261101T120000Z\r\nSUMMARY:Task\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

pub(crate) fn url(path: &str) -> Url {
    Url::parse(&format!("{BASE}{path}")).unwrap()
}

/// An event on 2026-12-01 09:00 UTC that the owner tentatively accepted.
pub(crate) fn event(uid: &str, sequence: u32, rule: Option<&str>) -> String {
    let rule = rule.map(|r| format!("RRULE:{r}\r\n")).unwrap_or_default();
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T120000Z\r\nSEQUENCE:{sequence}\r\nDTSTART:20261201T090000Z\r\n\
         DTEND:20261201T100000Z\r\n{rule}SUMMARY:{uid}\r\nSTATUS:CONFIRMED\r\n\
         ORGANIZER:mailto:boss@example.com\r\n\
         ATTENDEE;PARTSTAT=TENTATIVE:mailto:{ME}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct State {
    calendars: Mutex<Vec<CalendarInfo>>,
    resources: Mutex<BTreeMap<String, (String, String)>>,
    pages: Mutex<VecDeque<tagmail_dav::Result<SyncPage>>>,
    calls: Mutex<Vec<&'static str>>,
    failing: Mutex<HashSet<String>>,
    blocked: AtomicBool,
    /// `list_calendars` waits for [`FakeDav::release`].
    held: AtomicBool,
    released: tokio::sync::Notify,
    etags: AtomicU64,
    scripted: Mutex<VecDeque<(&'static str, DavError)>>,
    rewrites: Mutex<VecDeque<(String, String)>>,
}

/// One home with the calendar [`WORK`]; clones share the server.
#[derive(Clone, Default)]
pub(crate) struct FakeDav(Arc<State>);

impl FakeDav {
    /// Makes [`WORK`] the only calendar; a sync token means it supports
    /// `sync-collection`.
    pub(crate) fn calendar(&self, ctag: Option<&str>, sync_token: Option<&str>) {
        lock(&self.0.calendars).clear();
        self.add_calendar(WORK, ctag, sync_token);
    }

    pub(crate) fn add_calendar(&self, path: &str, ctag: Option<&str>, sync_token: Option<&str>) {
        lock(&self.0.calendars).push(CalendarInfo {
            href: url(path),
            display_name: Some("Work".into()),
            color: Some("#ff0000".into()),
            ctag: ctag.map(str::to_owned),
            sync_token: sync_token.map(str::to_owned),
            supports_sync_collection: sync_token.is_some(),
            components: Some(vec!["VEVENT".into()]),
        });
    }

    /// Listing the calendar at `path` fails with a server error.
    pub(crate) fn fail_listing(&self, path: &str) {
        lock(&self.0.failing).insert(path.to_owned());
    }

    /// Every later request hangs, like an unresponsive server.
    pub(crate) fn block(&self) {
        self.0.blocked.store(true, Ordering::SeqCst);
    }

    /// Holds the next listings until [`FakeDav::release`].
    pub(crate) fn hold(&self) {
        self.0.held.store(true, Ordering::SeqCst);
    }

    pub(crate) fn release(&self) {
        self.0.held.store(false, Ordering::SeqCst);
        self.0.released.notify_waiters();
    }

    pub(crate) fn calendars(&self) -> Vec<CalendarInfo> {
        lock(&self.0.calendars).clone()
    }

    pub(crate) fn put(&self, name: &str, etag: &str, data: &str) {
        self.put_in(WORK, name, etag, data);
    }

    pub(crate) fn put_in(&self, calendar: &str, name: &str, etag: &str, data: &str) {
        lock(&self.0.resources).insert(
            format!("{calendar}{name}"),
            (etag.to_owned(), data.to_owned()),
        );
    }

    pub(crate) fn delete(&self, name: &str) {
        lock(&self.0.resources).remove(&format!("{WORK}{name}"));
    }

    pub(crate) fn page(&self, page: tagmail_dav::Result<SyncPage>) {
        lock(&self.0.pages).push_back(page);
    }

    /// The next PUT finds the resource at `path` changed to `data` (a new
    /// ETag) and fails with 412, like a concurrent writer.
    pub(crate) fn rewrite_before_next_put(&self, path: &str, data: &str) {
        lock(&self.0.rewrites).push_back((path.to_owned(), data.to_owned()));
    }

    /// The next `method` request fails with `error`, before any effect.
    pub(crate) fn fail_next(&self, method: &'static str, error: DavError) {
        lock(&self.0.scripted).push_back((method, error));
    }

    /// The resource at `path`, if any.
    pub(crate) fn resource(&self, path: &str) -> Option<(String, String)> {
        lock(&self.0.resources).get(path).cloned()
    }

    /// Paths of every resource.
    pub(crate) fn paths(&self) -> Vec<String> {
        lock(&self.0.resources).keys().cloned().collect()
    }

    fn scripted(&self, method: &'static str) -> Option<DavError> {
        let mut scripted = lock(&self.0.scripted);
        let at = scripted.iter().position(|(m, _)| *m == method)?;
        scripted.remove(at).map(|(_, e)| e)
    }

    fn next_etag(&self) -> String {
        format!("\"f{}\"", self.0.etags.fetch_add(1, Ordering::SeqCst))
    }

    fn holds_uid(data: &str, uid: &str) -> bool {
        data.lines().any(|l| l.trim_end() == format!("UID:{uid}"))
    }

    fn uid_of(data: &str) -> Option<String> {
        data.lines()
            .find_map(|l| l.trim_end().strip_prefix("UID:").map(str::to_owned))
    }

    fn write(
        &self,
        href: &Url,
        ics: &str,
        condition: &PutCondition,
    ) -> tagmail_dav::Result<String> {
        let path = href.path().to_owned();
        let mut resources = lock(&self.0.resources);
        let existing = resources.get(&path).map(|(e, _)| e.clone());
        match (condition, &existing) {
            (PutCondition::Create, Some(_)) => return Err(DavError::PreconditionFailed),
            (PutCondition::IfMatch(_), None) => return Err(DavError::PreconditionFailed),
            (PutCondition::IfMatch(want), Some(have)) if want != have => {
                return Err(DavError::PreconditionFailed);
            }
            _ => {}
        }
        if existing.is_none()
            && let Some(uid) = Self::uid_of(ics)
            && let Some(other) = resources
                .iter()
                .find(|(p, (_, d))| **p != path && Self::holds_uid(d, &uid))
                .map(|(p, _)| p.clone())
        {
            return Err(DavError::UidConflict {
                kind: UidConflictKind::NoUidConflict,
                existing: Some(url(&other)),
            });
        }
        let etag = self.next_etag();
        resources.insert(path.clone(), (etag.clone(), ics.to_owned()));
        Ok(etag)
    }

    fn remove(&self, href: &Url, etag: Option<&str>) -> tagmail_dav::Result<()> {
        let path = href.path().to_owned();
        let mut resources = lock(&self.0.resources);
        match resources.get(&path) {
            None => Err(DavError::NotFound { url: href.clone() }),
            Some((have, _)) if etag.is_some_and(|want| want != have) => {
                Err(DavError::PreconditionFailed)
            }
            Some(_) => {
                resources.remove(&path);
                Ok(())
            }
        }
    }

    /// The requests since the last call.
    pub(crate) fn took(&self) -> Vec<&'static str> {
        std::mem::take(&mut *lock(&self.0.calls))
    }

    fn called(&self, what: &'static str) {
        lock(&self.0.calls).push(what);
    }
}

impl CalDav for FakeDav {
    fn list_calendars<'a>(&'a self, _home: &'a Url) -> DavFuture<'a, Vec<CalendarInfo>> {
        self.called("list_calendars");
        if self.0.blocked.load(Ordering::SeqCst) {
            return Box::pin(std::future::pending());
        }
        if self.0.held.load(Ordering::SeqCst) {
            return Box::pin(async move {
                self.0.released.notified().await;
                Ok(self.calendars())
            });
        }
        let calendars = self.calendars();
        Box::pin(async move { Ok(calendars) })
    }

    fn list_etags<'a>(&'a self, calendar: &'a Url) -> DavFuture<'a, Vec<ResourceRef>> {
        self.called("list_etags");
        if lock(&self.0.failing).contains(calendar.path()) {
            let failed = Err(DavError::Status {
                status: SERVER_ERROR,
                url: calendar.clone(),
            });
            return Box::pin(async move { failed });
        }
        let refs = lock(&self.0.resources)
            .iter()
            .filter(|(path, _)| path.starts_with(calendar.path()))
            .map(|(path, (etag, _))| ResourceRef {
                href: url(path),
                etag: Some(etag.clone()),
            })
            .collect();
        Box::pin(async move { Ok(refs) })
    }

    fn multiget<'a>(
        &'a self,
        _calendar: &'a Url,
        hrefs: &'a [Url],
    ) -> DavFuture<'a, Vec<MultigetItem>> {
        self.called("multiget");
        let resources = lock(&self.0.resources);
        let items = hrefs
            .iter()
            .map(|h| match resources.get(h.path()) {
                Some((etag, data)) => MultigetItem::Found(Resource {
                    href: h.clone(),
                    etag: Some(etag.clone()),
                    data: data.clone(),
                }),
                None => MultigetItem::Missing(h.clone()),
            })
            .collect();
        Box::pin(async move { Ok(items) })
    }

    fn sync_collection<'a>(
        &'a self,
        _calendar: &'a Url,
        _token: Option<&'a str>,
    ) -> DavFuture<'a, SyncPage> {
        self.called("sync_collection");
        let page = lock(&self.0.pages)
            .pop_front()
            .unwrap_or(Err(DavError::ReportUnsupported("sync-collection")));
        Box::pin(async move { page })
    }

    fn get<'a>(&'a self, href: &'a Url) -> DavFuture<'a, Resource> {
        self.called("get");
        let found = match self.scripted("get") {
            Some(e) => Err(e),
            None => self
                .resource(href.path())
                .map(|(etag, data)| Resource {
                    href: href.clone(),
                    etag: Some(etag),
                    data,
                })
                .ok_or_else(|| DavError::NotFound { url: href.clone() }),
        };
        Box::pin(async move { found })
    }

    fn put<'a>(
        &'a self,
        href: &'a Url,
        ics: &'a str,
        condition: PutCondition,
    ) -> DavFuture<'a, Option<String>> {
        self.called("put");
        let rewrite = lock(&self.0.rewrites).pop_front();
        if let Some((path, data)) = rewrite {
            let etag = self.next_etag();
            lock(&self.0.resources).insert(path, (etag, data));
            return Box::pin(async { Err(DavError::PreconditionFailed) });
        }
        let put = match self.scripted("put") {
            Some(e) => Err(e),
            None => self.write(href, ics, &condition).map(Some),
        };
        Box::pin(async move { put })
    }

    fn delete<'a>(
        &'a self,
        href: &'a Url,
        etag: Option<&'a str>,
        schedule_reply: bool,
    ) -> DavFuture<'a, ()> {
        self.called(if schedule_reply {
            "delete"
        } else {
            "delete without reply"
        });
        let deleted = match self.scripted("delete") {
            Some(e) => Err(e),
            None => self.remove(href, etag),
        };
        Box::pin(async move { deleted })
    }

    fn move_resource<'a>(
        &'a self,
        from: &'a Url,
        to: &'a Url,
        etag: Option<&'a str>,
    ) -> DavFuture<'a, ()> {
        self.called("move");
        let moved = match self.scripted("move") {
            Some(e) => Err(e),
            None => match self.resource(from.path()) {
                None => Err(DavError::NotFound { url: from.clone() }),
                Some((have, _)) if etag.is_some_and(|want| want != have) => {
                    Err(DavError::PreconditionFailed)
                }
                Some((_, data)) => {
                    let mut resources = lock(&self.0.resources);
                    if resources.contains_key(to.path()) {
                        Err(DavError::PreconditionFailed)
                    } else {
                        resources.remove(from.path());
                        resources.insert(to.path().to_owned(), (self.next_etag(), data));
                        Ok(())
                    }
                }
            },
        };
        Box::pin(async move { moved })
    }

    fn find_by_uid<'a>(
        &'a self,
        calendars: &'a [Url],
        uid: &'a str,
    ) -> DavFuture<'a, Option<UidMatch>> {
        self.called("find_by_uid");
        let resources = lock(&self.0.resources);
        let found = calendars.iter().find_map(|calendar| {
            resources
                .iter()
                .find(|(path, (_, data))| {
                    path.starts_with(calendar.path()) && Self::holds_uid(data, uid)
                })
                .map(|(path, (etag, data))| UidMatch {
                    calendar: calendar.clone(),
                    resource: Resource {
                        href: url(path),
                        etag: Some(etag.clone()),
                        data: data.clone(),
                    },
                })
        });
        Box::pin(async move { Ok(found) })
    }
}

/// What a discovery was asked, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    pub email: String,
    pub imap_host: String,
    pub manual_url: Option<String>,
    pub confirmed_host: Option<String>,
    pub username: String,
    pub password: String,
}

/// The login of a sync connection, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Connected {
    pub username: String,
    pub password: String,
    pub confirmed_host: String,
}

/// Connects to one [`FakeDav`] and answers discoveries from a script.
#[derive(Clone, Default)]
pub(crate) struct FakeConnector {
    pub dav: FakeDav,
    discoveries: Arc<Mutex<VecDeque<tagmail_dav::Result<Discovery>>>>,
    asked: Arc<Mutex<Vec<Asked>>>,
    connected: Arc<Mutex<Vec<Connected>>>,
}

impl FakeConnector {
    pub(crate) fn discovers(&self, outcome: tagmail_dav::Result<Discovery>) {
        lock(&self.discoveries).push_back(outcome);
    }

    pub(crate) fn asked(&self) -> Vec<Asked> {
        lock(&self.asked).clone()
    }

    pub(crate) fn connected(&self) -> Vec<Connected> {
        lock(&self.connected).clone()
    }
}

impl DavConnector for FakeConnector {
    fn connect(
        &self,
        login: &DavLogin,
        confirmed_host: &str,
    ) -> tagmail_dav::Result<Box<dyn CalDav>> {
        lock(&self.connected).push(Connected {
            username: login.username.clone(),
            password: login.password.as_str().to_owned(),
            confirmed_host: confirmed_host.to_owned(),
        });
        Ok(Box::new(self.dav.clone()))
    }

    fn discover<'a>(
        &'a self,
        target: DiscoveryTarget<'a>,
        login: &'a DavLogin,
    ) -> DavFuture<'a, Discovery> {
        lock(&self.asked).push(Asked {
            email: target.email.to_owned(),
            imap_host: target.imap_host.to_owned(),
            manual_url: target.manual_url.map(Url::to_string),
            confirmed_host: target.confirmed_host.map(str::to_owned),
            username: login.username.clone(),
            password: login.password.as_str().to_owned(),
        });
        let outcome = lock(&self.discoveries)
            .pop_front()
            .unwrap_or(Err(DavError::NotDiscovered));
        Box::pin(async move { outcome })
    }
}
