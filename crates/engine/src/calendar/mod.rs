//! CalDAV calendars (appointments plan §3.1): discovery for the API and
//! one sync runner for every account. [`CalDav`] is the seam between them
//! and the network; [`tagmail_dav::Client`] implements it.

pub mod alarms;
#[cfg(test)]
mod engine_tests;
pub(crate) mod executor;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod invite;
#[cfg(test)]
mod ops_tests;
pub(crate) mod sync;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use tagmail_api_core::calendar_dto::{
    CaldavDiscoveryDto, CaldavStatusDto, CalendarDto, SchedulingDto,
};
use tagmail_api_core::{BoxFuture, CalendarError, CalendarService};
use tagmail_core::AccountId;
use tagmail_dav::{
    CalendarInfo, Client, ClientOptions, DavError, Discovery, DiscoveryEnv, DiscoveryTarget,
    MultigetItem, PutCondition, Resource, ResourceRef, SchedulingMode, SyncPage, UidMatch,
};
use tagmail_store::calendar::{self, CaldavConfig, CaldavStatus, DiscoveryResult, Scheduling};
use tagmail_store::rusqlite::{Connection, OptionalExtension};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use url::Url;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::supervisor::{ConnectorFactory, Engine, Stores, load_account};

pub(crate) type DavFuture<'a, T> =
    Pin<Box<dyn Future<Output = tagmail_dav::Result<T>> + Send + 'a>>;

/// The CalDAV requests the sync runner makes.
pub(crate) trait CalDav: Send + Sync {
    fn list_calendars<'a>(&'a self, home: &'a Url) -> DavFuture<'a, Vec<CalendarInfo>>;
    fn list_etags<'a>(&'a self, calendar: &'a Url) -> DavFuture<'a, Vec<ResourceRef>>;
    fn multiget<'a>(
        &'a self,
        calendar: &'a Url,
        hrefs: &'a [Url],
    ) -> DavFuture<'a, Vec<MultigetItem>>;
    fn sync_collection<'a>(
        &'a self,
        calendar: &'a Url,
        token: Option<&'a str>,
    ) -> DavFuture<'a, SyncPage>;
    fn get<'a>(&'a self, href: &'a Url) -> DavFuture<'a, Resource>;
    fn put<'a>(
        &'a self,
        href: &'a Url,
        ics: &'a str,
        condition: PutCondition,
    ) -> DavFuture<'a, Option<String>>;
    fn delete<'a>(&'a self, href: &'a Url, etag: Option<&'a str>) -> DavFuture<'a, ()>;
    /// MOVE, falling back to GET + PUT + DELETE where the server cannot.
    fn move_resource<'a>(
        &'a self,
        from: &'a Url,
        to: &'a Url,
        etag: Option<&'a str>,
    ) -> DavFuture<'a, ()>;
    fn find_by_uid<'a>(
        &'a self,
        calendars: &'a [Url],
        uid: &'a str,
    ) -> DavFuture<'a, Option<UidMatch>>;
}

impl CalDav for Client {
    fn list_calendars<'a>(&'a self, home: &'a Url) -> DavFuture<'a, Vec<CalendarInfo>> {
        Box::pin(Client::list_calendars(self, home))
    }

    fn list_etags<'a>(&'a self, calendar: &'a Url) -> DavFuture<'a, Vec<ResourceRef>> {
        Box::pin(Client::list_etags(self, calendar))
    }

    fn multiget<'a>(
        &'a self,
        calendar: &'a Url,
        hrefs: &'a [Url],
    ) -> DavFuture<'a, Vec<MultigetItem>> {
        Box::pin(Client::multiget(self, calendar, hrefs))
    }

    fn sync_collection<'a>(
        &'a self,
        calendar: &'a Url,
        token: Option<&'a str>,
    ) -> DavFuture<'a, SyncPage> {
        Box::pin(Client::sync_collection(self, calendar, token))
    }

    fn get<'a>(&'a self, href: &'a Url) -> DavFuture<'a, Resource> {
        Box::pin(Client::get(self, href))
    }

    fn put<'a>(
        &'a self,
        href: &'a Url,
        ics: &'a str,
        condition: PutCondition,
    ) -> DavFuture<'a, Option<String>> {
        Box::pin(async move { Client::put(self, href, ics, condition).await })
    }

    fn delete<'a>(&'a self, href: &'a Url, etag: Option<&'a str>) -> DavFuture<'a, ()> {
        Box::pin(Client::delete(self, href, etag))
    }

    fn move_resource<'a>(
        &'a self,
        from: &'a Url,
        to: &'a Url,
        etag: Option<&'a str>,
    ) -> DavFuture<'a, ()> {
        Box::pin(Client::move_resource(self, from, to, etag))
    }

    fn find_by_uid<'a>(
        &'a self,
        calendars: &'a [Url],
        uid: &'a str,
    ) -> DavFuture<'a, Option<UidMatch>> {
        Box::pin(Client::find_by_uid(self, calendars, uid))
    }
}

/// The login and transport rules of one account's CalDAV server.
#[derive(Clone)]
pub(crate) struct DavLogin {
    pub username: String,
    pub password: Zeroizing<String>,
    pub allow_insecure: bool,
}

/// Opens CalDAV connections and runs discovery.
pub(crate) trait DavConnector: Send + Sync + 'static {
    fn connect(
        &self,
        login: &DavLogin,
        confirmed_host: &str,
    ) -> tagmail_dav::Result<Box<dyn CalDav>>;

    fn discover<'a>(
        &'a self,
        target: DiscoveryTarget<'a>,
        login: &'a DavLogin,
    ) -> DavFuture<'a, Discovery>;
}

/// The network as production sees it.
pub(crate) struct SystemConnector {
    env: DiscoveryEnv,
    tls: rustls::ClientConfig,
}

impl SystemConnector {
    fn options(&self, login: &DavLogin) -> ClientOptions {
        ClientOptions {
            username: login.username.clone(),
            password: login.password.clone(),
            tls: self.tls.clone(),
            allow_insecure: login.allow_insecure,
        }
    }
}

impl DavConnector for SystemConnector {
    fn connect(
        &self,
        login: &DavLogin,
        confirmed_host: &str,
    ) -> tagmail_dav::Result<Box<dyn CalDav>> {
        Ok(Box::new(Client::new(
            &self.options(login),
            confirmed_host,
            &self.env,
        )?))
    }

    fn discover<'a>(
        &'a self,
        target: DiscoveryTarget<'a>,
        login: &'a DavLogin,
    ) -> DavFuture<'a, Discovery> {
        Box::pin(
            async move { tagmail_dav::discover(target, &self.options(login), &self.env).await },
        )
    }
}

struct Running {
    connector: Arc<dyn DavConnector>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// The calendar runner of an engine.
#[derive(Default)]
pub(crate) struct State {
    wake: Arc<Notify>,
    /// Runs due ops without a poll.
    ops: Arc<Notify>,
    /// Re-arms the reminder timer.
    alarms: Arc<Notify>,
    /// The owner's time zone changed; the runner recomputes alarms.
    rezone: Arc<std::sync::atomic::AtomicBool>,
    running: Mutex<Option<Running>>,
    removing: sync::Removing,
}

/// Wakes the runner when mail sync queued invitations.
struct InviteWake(Arc<Notify>);

impl crate::calendar_hook::InviteHook for InviteWake {
    fn queued(&self) {
        self.0.notify_one();
    }
}

impl State {
    pub(crate) fn invite_hook(&self) -> Arc<dyn crate::calendar_hook::InviteHook> {
        Arc::new(InviteWake(self.wake.clone()))
    }

    /// Stops calendar writes for `account` before its cache rows are
    /// purged.
    pub(crate) fn removing(&self, account: AccountId) {
        self.removing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(account);
    }

    /// An account id reused by a new account syncs again.
    pub(crate) fn added(&self, account: AccountId) {
        self.removing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&account);
    }

    fn connector(&self) -> Option<Arc<dyn DavConnector>> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|r| r.connector.clone())
    }

    /// Stops the runner and waits for it.
    pub(crate) async fn shutdown(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(running) = running {
            running.stop.send_replace(true);
            if let Err(e) = running.task.await
                && e.is_panic()
            {
                tracing::error!("calendar runner panicked");
            }
        }
    }
}

/// What discovery and sync need of an account besides its `caldav` row.
pub(crate) struct CalAccount {
    pub id: AccountId,
    pub imap_host: String,
    /// The sender address, else the IMAP username.
    pub email: String,
    pub config: CaldavConfig,
    pub login: DavLogin,
    /// The owner's addresses, lowercased, to find their ATTENDEE.
    pub addresses: Vec<String>,
}

fn addresses(conn: &Connection, id: AccountId) -> tagmail_store::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT lower(address) FROM identity WHERE account_id = ?1 \
         UNION SELECT lower(email) FROM account WHERE id = ?1 AND email IS NOT NULL",
    )?;
    Ok(stmt
        .query_map([id.0], |r| r.get(0))?
        .collect::<tagmail_store::rusqlite::Result<_>>()?)
}

/// Loads the account with its CalDAV login: the override when set, else
/// the IMAP login. `None` if the account does not exist.
pub(crate) async fn load(stores: &Stores, id: AccountId) -> Result<Option<CalAccount>> {
    let row = stores
        .config
        .read(move |c| {
            let account: Option<(String, Option<String>, String)> = c
                .query_row(
                    "SELECT imap_host, email, username FROM account WHERE id = ?1",
                    [id.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some(account) = account else {
                return Ok(None);
            };
            let config = calendar::load_caldav(c, id)?.unwrap_or_default();
            Ok(Some((account, config, addresses(c, id)?)))
        })
        .await?;
    let Some(((imap_host, email, imap_username), config, addresses)) = row else {
        return Ok(None);
    };
    let (username, password) = match (&config.username, &config.secret) {
        (Some(username), Some(sealed)) => (
            username.clone(),
            stores
                .vault
                .open_caldav(id, sealed)
                .map_err(tagmail_store::Error::from)?,
        ),
        _ => {
            let imap = load_account(stores, id).await?;
            (
                imap.username,
                Zeroizing::new(imap.password.expose().to_owned()),
            )
        }
    };
    Ok(Some(CalAccount {
        id,
        imap_host,
        email: email.unwrap_or_else(|| imap_username.clone()),
        login: DavLogin {
            username,
            password,
            allow_insecure: config.allow_insecure,
        },
        config,
        addresses,
    }))
}

fn scheduling(mode: SchedulingMode) -> Scheduling {
    match mode {
        SchedulingMode::Server => Scheduling::Server,
        SchedulingMode::Client => Scheduling::Client,
    }
}

fn scheduling_dto(s: Scheduling) -> SchedulingDto {
    match s {
        Scheduling::Server => SchedulingDto::Server,
        Scheduling::Client => SchedulingDto::Client,
    }
}

fn status_dto(status: CaldavStatus) -> CaldavStatusDto {
    match status {
        CaldavStatus::Unconfigured => CaldavStatusDto::Unconfigured,
        CaldavStatus::NeedsConfirmation => CaldavStatusDto::NeedsConfirmation,
        CaldavStatus::Ok => CaldavStatusDto::Ok,
        CaldavStatus::Error => CaldavStatusDto::Error,
    }
}

fn unavailable(e: impl std::fmt::Display) -> CalendarError {
    CalendarError::Unavailable(e.to_string())
}

/// A manual URL is one the owner entered; a discovered one is found again.
fn manual_url(config: &CaldavConfig) -> std::result::Result<Option<Url>, DavError> {
    let manual = matches!(config.discovery, None | Some(calendar::Discovery::Manual));
    match &config.url {
        Some(url) if manual => Url::parse(url)
            .map(Some)
            .map_err(|_| DavError::BadHref { href: url.clone() }),
        _ => Ok(None),
    }
}

impl<F: ConnectorFactory> Engine<F> {
    /// Starts the calendar runner on the system network.
    pub fn start_calendar(&self) -> Result<()> {
        self.start_calendar_with_env(DiscoveryEnv::system())
    }

    /// Like [`Engine::start_calendar`] with test overrides of discovery
    /// and reports.
    #[cfg(feature = "caldav-tests")]
    pub fn start_calendar_with(&self, env: DiscoveryEnv) -> Result<()> {
        self.start_calendar_with_env(env)
    }

    /// Stops only the calendar runner, so tests can change the server
    /// between an op's submission and its run.
    #[cfg(feature = "caldav-tests")]
    pub async fn stop_calendar(&self) {
        self.inner.calendar.shutdown().await;
    }

    /// Queues a full edit of `href` (the `put` op of plan §4 M-C step 1)
    /// conditional on `if_match`, until the edit API exists.
    #[cfg(feature = "caldav-tests")]
    pub async fn queue_calendar_put(
        &self,
        op_id: &str,
        account: AccountId,
        calendar_href: &str,
        href: &str,
        ics: &str,
        if_match: Option<&str>,
    ) -> Result<()> {
        let dir = executor::ics_dir(&self.inner.stores);
        std::fs::create_dir_all(&dir).map_err(tagmail_store::Error::from)?;
        let name = format!("{op_id}.ics");
        std::fs::write(dir.join(&name), ics).map_err(tagmail_store::Error::from)?;
        let mut op = executor::CalOp::new(
            op_id.to_owned(),
            account,
            executor::CalOpKind::Put,
            String::new(),
        );
        op.calendar_href = Some(calendar_href.to_owned());
        op.href = Some(href.to_owned());
        op.if_match = if_match.map(str::to_owned);
        op.ics_file = Some(name);
        self.inner
            .stores
            .config
            .write(move |tx| Ok(executor::insert(tx, &op)?))
            .await?;
        self.wake_calendar_ops();
        Ok(())
    }

    fn start_calendar_with_env(&self, env: DiscoveryEnv) -> Result<()> {
        let tls = tagmail_imap::client_config(None)?;
        self.start_calendar_on(Arc::new(SystemConnector {
            env,
            tls: (*tls).clone(),
        }));
        Ok(())
    }

    pub(crate) fn start_calendar_on(&self, connector: Arc<dyn DavConnector>) {
        let state = &self.inner.calendar;
        let mut running = state.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.is_some() {
            return;
        }
        let (stop, stopped) = watch::channel(false);
        let inbox = Arc::new(Notify::new());
        let runner = sync::Runner {
            stores: self.inner.stores.clone(),
            connector: connector.clone(),
            removing: state.removing.clone(),
            broken: sync::BrokenReports::new(),
            mail: Arc::new(invite::WeakEngine(Arc::downgrade(&self.inner))),
            inbox: inbox.clone(),
            rezone: state.rezone.clone(),
            zone: None,
        };
        let invitations = sync::invitations(
            self.inner.stores.clone(),
            runner.mail.clone(),
            inbox,
            state.ops.clone(),
            stopped.clone(),
        );
        let reminders = alarms::run(
            self.inner.stores.clone(),
            state.alarms.clone(),
            stopped.clone(),
        );
        let runs = sync::run(
            runner,
            state.wake.clone(),
            state.ops.clone(),
            state.alarms.clone(),
            stopped,
        );
        let task = self.inner.runtime.spawn(async move {
            tokio::join!(runs, invitations, reminders);
        });
        *running = Some(Running {
            connector,
            stop,
            task,
        });
    }

    pub(crate) fn wake_calendar(&self) {
        self.inner.calendar.wake.notify_one();
    }

    /// Runs the due calendar ops now.
    pub(crate) fn wake_calendar_ops(&self) {
        self.inner.calendar.ops.notify_one();
    }

    async fn discover_caldav(
        &self,
        id: AccountId,
        confirm: Option<String>,
    ) -> std::result::Result<CaldavDiscoveryDto, CalendarError> {
        let connector = self
            .inner
            .calendar
            .connector()
            .ok_or_else(|| unavailable("the calendar runner is not started"))?;
        let stores = &self.inner.stores;
        let account = match load(stores, id).await {
            Ok(Some(account)) => account,
            Ok(None) | Err(Error::NoAccount(_)) => return Err(CalendarError::NoAccount),
            Err(e) => return Err(unavailable(e)),
        };
        let mut config = account.config;
        let read_url = config.url.clone();
        if let Some(host) = confirm {
            let host = tagmail_dav::normalize_host(&host);
            let asked = config.status == CaldavStatus::NeedsConfirmation
                && config
                    .pending_host
                    .as_deref()
                    .is_some_and(|pending| tagmail_dav::normalize_host(pending) == host);
            if !asked {
                return Err(CalendarError::HostMismatch);
            }
            config.confirmed_host = Some(host);
        }
        let manual = manual_url(&config);
        let outcome = match &manual {
            Ok(manual) => {
                let target = DiscoveryTarget {
                    email: &account.email,
                    imap_host: &account.imap_host,
                    manual_url: manual.as_ref(),
                    confirmed_host: config.confirmed_host.as_deref(),
                };
                connector.discover(target, &account.login).await
            }
            Err(e) => Err(DavError::BadHref {
                href: e.to_string(),
            }),
        };
        let is_manual = matches!(manual, Ok(Some(_)));
        let mut result = DiscoveryResult {
            url: config.url.clone(),
            principal_url: config.principal_url.clone(),
            home_url: config.home_url.clone(),
            confirmed_host: config.confirmed_host.clone(),
            pending_host: None,
            scheduling_detected: config.scheduling_detected,
            discovery: Some(if is_manual {
                calendar::Discovery::Manual
            } else {
                calendar::Discovery::WellKnown
            }),
            status: CaldavStatus::Error,
            last_error: None,
        };
        let mut found = Vec::new();
        match outcome {
            Ok(Discovery::Found(account)) => {
                if !is_manual {
                    result.url = Some(account.context_url.to_string());
                }
                result.principal_url = Some(account.principal.to_string());
                result.home_url = Some(account.home.to_string());
                result.confirmed_host = Some(account.host.clone());
                result.scheduling_detected = Some(scheduling(account.scheduling.detected()));
                result.status = CaldavStatus::Ok;
                found = account.calendars;
            }
            Ok(Discovery::NeedsConfirmation { host, url }) => {
                if !is_manual {
                    result.url = Some(url.to_string());
                }
                result.confirmed_host = None;
                result.pending_host = Some(host);
                result.status = CaldavStatus::NeedsConfirmation;
            }
            Err(e) => {
                tracing::warn!(account = id.0, error = %e, "CalDAV discovery failed");
                result.last_error = Some(e.to_string());
            }
        }
        let dto = CaldavDiscoveryDto {
            status: status_dto(result.status),
            host: result
                .pending_host
                .clone()
                .or_else(|| result.confirmed_host.clone()),
            url: result.url.clone(),
            scheduling_detected: result.scheduling_detected.map(scheduling_dto),
            calendars: Vec::new(),
            error: result.last_error.clone(),
        };
        let recorded = self
            .inner
            .stores
            .config
            .write(move |tx| calendar::record_discovery(tx, id, read_url.as_deref(), &result))
            .await
            .map_err(unavailable)?;
        if !recorded {
            return Err(unavailable(
                "the CalDAV settings changed during discovery; discover again",
            ));
        }
        let calendars = self.save_calendars(id, &found).await?;
        if dto.status == CaldavStatusDto::Ok {
            self.wake_calendar();
        }
        Ok(CaldavDiscoveryDto { calendars, ..dto })
    }

    /// Caches the discovered calendars and returns them with the owner's
    /// preferences.
    async fn save_calendars(
        &self,
        id: AccountId,
        found: &[CalendarInfo],
    ) -> std::result::Result<Vec<CalendarDto>, CalendarError> {
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, Option<String>, Option<String>)> = found
            .iter()
            .map(|c| (sync::href(&c.href), c.display_name.clone(), c.color.clone()))
            .collect();
        let hrefs: Vec<String> = rows.iter().map(|(href, ..)| href.clone()).collect();
        self.inner
            .stores
            .cache
            .write(move |tx| {
                for (href, name, color) in &rows {
                    calendar::upsert_calendar(
                        tx,
                        id,
                        href,
                        name.as_deref(),
                        color.as_deref(),
                        true,
                    )?;
                }
                calendar::retain_calendars(tx, id, &hrefs)?;
                Ok(())
            })
            .await
            .map_err(unavailable)?;
        let (prefs, default) = self
            .inner
            .stores
            .config
            .read(move |c| {
                Ok((
                    calendar::calendar_prefs(c)?,
                    calendar::load_caldav(c, id)?.and_then(|c| c.default_calendar_href),
                ))
            })
            .await
            .map_err(unavailable)?;
        Ok(found
            .iter()
            .map(|c| {
                let href = sync::href(&c.href);
                let pref = prefs.get(&(id, href.clone()));
                CalendarDto {
                    account_id: id.0,
                    display_name: c.display_name.clone(),
                    color: pref
                        .and_then(|p| p.color.clone())
                        .or_else(|| c.color.clone()),
                    visible: pref.is_none_or(|p| p.visible),
                    sort: pref.and_then(|p| p.sort),
                    writable: true,
                    is_default: default.as_deref() == Some(href.as_str()),
                    href,
                }
            })
            .collect())
    }
}

impl<F: ConnectorFactory> CalendarService for Engine<F> {
    fn discover(
        &self,
        account: AccountId,
    ) -> BoxFuture<std::result::Result<CaldavDiscoveryDto, CalendarError>> {
        let engine = self.clone();
        Box::pin(async move { engine.discover_caldav(account, None).await })
    }

    fn confirm_host(
        &self,
        account: AccountId,
        host: String,
    ) -> BoxFuture<std::result::Result<CaldavDiscoveryDto, CalendarError>> {
        let engine = self.clone();
        Box::pin(async move { engine.discover_caldav(account, Some(host)).await })
    }

    fn settings_changed(&self, account: AccountId) {
        tracing::debug!(account = account.0, "CalDAV settings changed");
        self.wake_calendar();
    }

    fn owner_zone_changed(&self) -> BoxFuture<()> {
        let state = &self.inner.calendar;
        state
            .rezone
            .store(true, std::sync::atomic::Ordering::Release);
        state.ops.notify_one();
        Box::pin(std::future::ready(()))
    }

    fn submit(
        &self,
        request: tagmail_api_core::CalRequest,
    ) -> BoxFuture<tagmail_api_core::WriteResult<tagmail_api_core::Submitted>> {
        let engine = self.clone();
        Box::pin(async move { engine.submit_calendar(request).await })
    }

    fn existing(
        &self,
        op_id: String,
    ) -> BoxFuture<tagmail_api_core::WriteResult<Option<tagmail_api_core::ops_dto::OpDto>>> {
        let engine = self.clone();
        Box::pin(async move {
            Ok(match engine.existing_cal(&op_id).await? {
                Some(
                    tagmail_api_core::Submitted::Existing(op)
                    | tagmail_api_core::Submitted::Created(op),
                ) => Some(op),
                None => None,
            })
        })
    }
}
