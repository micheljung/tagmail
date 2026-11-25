//! In-memory CalDAV server and connector for the calendar tests.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tagmail_dav::{
    CalendarInfo, DavError, Discovery, DiscoveryTarget, MultigetItem, Resource, ResourceRef,
    SyncPage,
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
