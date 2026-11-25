//! CalDAV settings, discovery and the calendar list (plan §3.5). Discovery
//! talks to the network, so it goes through a [`CalendarService`] (the
//! engine); the rest reads and writes the [`Store`].

use std::fmt;

use tagmail_core::AccountId;

use crate::api::{Api, Response};
use crate::calendar_dto::CaldavDiscoveryDto;
use crate::routes::RouteId;
use crate::store::{BoxFuture, Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    NoAccount,
    /// `confirm-host` named a host discovery did not ask about.
    HostMismatch,
    Unavailable(String),
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalendarError::NoAccount => f.write_str("no such account"),
            CalendarError::HostMismatch => {
                f.write_str("discovery did not ask to confirm this host")
            }
            CalendarError::Unavailable(why) => write!(f, "calendar sync is unavailable: {why}"),
        }
    }
}

/// The engine's calendar side as the API sees it.
pub trait CalendarService: Send + Sync + 'static {
    /// Discovers the account's CalDAV server from its settings and stores
    /// the outcome. Network failures are reported in the result.
    fn discover(&self, account: AccountId) -> BoxFuture<Result<CaldavDiscoveryDto, CalendarError>>;

    /// Records `host`, which discovery asked to confirm, then discovers
    /// again.
    fn confirm_host(
        &self,
        account: AccountId,
        host: String,
    ) -> BoxFuture<Result<CaldavDiscoveryDto, CalendarError>>;

    /// Called after the settings were saved.
    fn settings_changed(&self, account: AccountId);
}

pub(crate) fn feature_disabled() -> Response {
    Response::error(404, "feature_disabled", "calendar support is not built in")
}

impl<S: Store> Api<S> {
    /// Serves calendar requests through `calendar`; without it discovery
    /// answers `503`.
    pub fn with_calendar(mut self, calendar: std::sync::Arc<dyn CalendarService>) -> Api<S> {
        self.calendar = Some(calendar);
        self
    }

    #[cfg(not(feature = "calendar"))]
    pub(crate) async fn calendar_route(
        &self,
        _route: RouteId,
        _params: &[&str],
        _body: &[u8],
    ) -> Result<Response, StoreError> {
        Ok(feature_disabled())
    }

    #[cfg(feature = "calendar")]
    pub(crate) async fn calendar_route(
        &self,
        route: RouteId,
        params: &[&str],
        body: &[u8],
    ) -> Result<Response, StoreError> {
        let served = match route {
            RouteId::CaldavSettings => self.caldav_settings(params[0]).await,
            RouteId::UpdateCaldavSettings => self.update_caldav_settings(params[0], body).await,
            RouteId::DiscoverCaldav => self.discover_caldav(params[0]).await,
            RouteId::ConfirmCaldavHost => self.confirm_caldav_host(params[0], body).await,
            RouteId::Calendars => self.list_calendars().await,
            RouteId::UpdateCalendar => self.update_calendar(body).await,
            _ => unreachable!("not a calendar route: {route:?}"),
        };
        match served {
            Err(e) if e.is::<crate::store::FeatureDisabled>() => Ok(feature_disabled()),
            other => other,
        }
    }
}

#[cfg(feature = "calendar")]
mod handlers {
    use tagmail_core::AccountId;

    use crate::api::{
        Api, Checked, Response, bad_request, check_password, check_username, invalid, no_account,
        valid_host,
    };
    use crate::calendar_dto::{
        CaldavSettingsDto, CaldavSettingsInputDto, CalendarPatchDto, CalendarsDto, ConfirmHostDto,
        SchedulingDto, SchedulingOverrideDto,
    };
    use crate::compose::parse_row_id;
    use crate::store::{CaldavSettingsUpdate, CalendarPatch, Store, StoreError};

    use super::{CalendarError, CalendarService};

    const MAX_URL_LEN: usize = 2048;
    const MAX_HREF_LEN: usize = 2048;
    const MAX_AUTHSERV_ID_LEN: usize = 255;
    const MAX_COLOR_LEN: usize = 32;
    const HTTPS: &str = "https://";
    const HTTP: &str = "http://";

    fn calendar_unavailable() -> Response {
        Response::error(
            503,
            "engine_unavailable",
            "no sync engine is running; CalDAV cannot be discovered",
        )
    }

    fn calendar_error(e: CalendarError) -> Response {
        match e {
            CalendarError::NoAccount => no_account(),
            CalendarError::HostMismatch => invalid("host", e.to_string()),
            CalendarError::Unavailable(_) => Response::error(503, "unavailable", e.to_string()),
        }
    }

    fn printable(text: &str) -> bool {
        !text.is_empty() && !text.chars().any(char::is_control)
    }

    /// An absolute http(s) URL with a valid host; `http` only when allowed.
    fn check_url(url: String, allow_insecure: bool) -> Checked<String> {
        let url = url.trim().to_owned();
        let rest = match (url.strip_prefix(HTTPS), url.strip_prefix(HTTP)) {
            (Some(rest), _) => rest,
            (None, Some(rest)) if allow_insecure => rest,
            (None, Some(_)) => {
                return Err(invalid("url", "an http URL needs allowInsecure; use https"));
            }
            _ => return Err(invalid("url", "not an http(s) URL")),
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
            _ => authority,
        };
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if url.len() > MAX_URL_LEN
            || !printable(&url)
            || authority.contains('@')
            || !valid_host(host)
        {
            return Err(invalid("url", "not a valid CalDAV URL"));
        }
        Ok(url)
    }

    fn check_text(field: &str, value: Option<String>, max: usize) -> Checked<Option<String>> {
        let value = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        match value {
            Some(v) if v.len() > max || !printable(&v) => Err(invalid(
                field,
                format!("at most {max} bytes without control characters"),
            )),
            v => Ok(v),
        }
    }

    fn validate(
        account: AccountId,
        dto: CaldavSettingsInputDto,
        stored: &CaldavSettingsDto,
    ) -> Checked<CaldavSettingsUpdate> {
        let url = dto
            .url
            .filter(|u| !u.trim().is_empty())
            .map(|u| check_url(u, dto.allow_insecure))
            .transpose()?;
        let username = dto.username.map(check_username).transpose()?;
        let password = dto.password.map(check_password).transpose()?;
        if username.is_none() && password.is_some() {
            return Err(invalid("username", "a password needs a username"));
        }
        let keeps_password = username.is_some() && password.is_none();
        let same_login = stored.has_password && stored.username == username && stored.url == url;
        if keeps_password && !same_login {
            return Err(invalid(
                "password",
                "enter the password for a new username or server",
            ));
        }
        let fallback_account = dto.fallback_account_id.map(AccountId);
        if fallback_account == Some(account) {
            return Err(invalid(
                "fallbackAccountId",
                "an account cannot fall back to itself",
            ));
        }
        Ok(CaldavSettingsUpdate {
            url,
            username,
            password,
            allow_insecure: dto.allow_insecure,
            scheduling: match dto.scheduling {
                SchedulingOverrideDto::Auto => None,
                SchedulingOverrideDto::Server => Some(SchedulingDto::Server),
                SchedulingOverrideDto::Client => Some(SchedulingDto::Client),
            },
            fallback_account,
            default_calendar_href: check_text(
                "defaultCalendarHref",
                dto.default_calendar_href,
                MAX_HREF_LEN,
            )?,
            authserv_id: dto
                .authserv_id
                .map(|id| check_text("authservId", id, MAX_AUTHSERV_ID_LEN))
                .transpose()?,
        })
    }

    impl<S: Store> Api<S> {
        fn calendar_service(&self) -> Result<&dyn CalendarService, Response> {
            self.calendar.as_deref().ok_or_else(calendar_unavailable)
        }

        pub(super) async fn caldav_settings(&self, id: &str) -> Result<Response, StoreError> {
            let Some(id) = parse_row_id(id).map(AccountId) else {
                return Ok(no_account());
            };
            Ok(match self.store.caldav_settings(id).await? {
                Some(settings) => Response::json(200, &settings),
                None => no_account(),
            })
        }

        pub(super) async fn update_caldav_settings(
            &self,
            id: &str,
            body: &[u8],
        ) -> Result<Response, StoreError> {
            let Some(id) = parse_row_id(id).map(AccountId) else {
                return Ok(no_account());
            };
            let dto: CaldavSettingsInputDto = match serde_json::from_slice(body) {
                Ok(dto) => dto,
                Err(e) => return Ok(bad_request(format!("invalid CalDAV settings JSON: {e}"))),
            };
            let Some(stored) = self.store.caldav_settings(id).await? else {
                return Ok(no_account());
            };
            let update = match validate(id, dto, &stored) {
                Ok(update) => update,
                Err(resp) => return Ok(resp),
            };
            if let Some(fallback) = update.fallback_account
                && self.store.caldav_settings(fallback).await?.is_none()
            {
                return Ok(invalid("fallbackAccountId", "no such account"));
            }
            let Some(saved) = self.store.save_caldav_settings(id, update).await? else {
                return Ok(no_account());
            };
            tracing::info!(account = id.0, "CalDAV settings saved");
            if let Some(calendar) = &self.calendar {
                calendar.settings_changed(id);
            }
            Ok(Response::json(200, &saved))
        }

        pub(super) async fn discover_caldav(&self, id: &str) -> Result<Response, StoreError> {
            let service = match self.calendar_service() {
                Ok(s) => s,
                Err(resp) => return Ok(resp),
            };
            let Some(id) = parse_row_id(id).map(AccountId) else {
                return Ok(no_account());
            };
            Ok(match service.discover(id).await {
                Ok(dto) => Response::json(200, &dto),
                Err(e) => calendar_error(e),
            })
        }

        pub(super) async fn confirm_caldav_host(
            &self,
            id: &str,
            body: &[u8],
        ) -> Result<Response, StoreError> {
            let service = match self.calendar_service() {
                Ok(s) => s,
                Err(resp) => return Ok(resp),
            };
            let Some(id) = parse_row_id(id).map(AccountId) else {
                return Ok(no_account());
            };
            let dto: ConfirmHostDto = match serde_json::from_slice(body) {
                Ok(dto) => dto,
                Err(e) => return Ok(bad_request(format!("invalid confirm-host JSON: {e}"))),
            };
            let host = dto.host.trim().to_ascii_lowercase();
            if !valid_host(&host) {
                return Ok(invalid("host", "not a host name or IP address"));
            }
            Ok(match service.confirm_host(id, host).await {
                Ok(dto) => Response::json(200, &dto),
                Err(e) => calendar_error(e),
            })
        }

        pub(super) async fn list_calendars(&self) -> Result<Response, StoreError> {
            let calendars = self.store.calendars().await?;
            Ok(Response::json(200, &CalendarsDto { calendars }))
        }

        pub(super) async fn update_calendar(&self, body: &[u8]) -> Result<Response, StoreError> {
            let dto: CalendarPatchDto = match serde_json::from_slice(body) {
                Ok(dto) => dto,
                Err(e) => return Ok(bad_request(format!("invalid calendar JSON: {e}"))),
            };
            let color = match dto.color {
                Some(color) => match check_text("color", color, MAX_COLOR_LEN) {
                    Ok(color) => Some(color),
                    Err(resp) => return Ok(resp),
                },
                None => None,
            };
            let patch = CalendarPatch {
                account: AccountId(dto.account_id),
                href: dto.href,
                color,
                visible: dto.visible,
                default: dto.default,
                sort: dto.sort,
            };
            Ok(match self.store.update_calendar(patch).await? {
                Some(calendar) => Response::json(200, &calendar),
                None => Response::error(404, "not_found", "no such calendar"),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn urls_must_be_https_unless_insecure_is_allowed() {
            assert!(check_url("https://dav.example.com/rpc/".into(), false).is_ok());
            assert!(check_url("https://[::1]:8443/".into(), false).is_ok());
            assert!(check_url("http://dav.example.com/".into(), false).is_err());
            assert!(check_url("http://127.0.0.1:10232/".into(), true).is_ok());
            for bad in [
                "ftp://dav.example.com/",
                "https://user@dav.example.com/",
                "https:///path",
                "https://bad host/",
                "dav.example.com",
            ] {
                assert!(check_url(bad.into(), true).is_err(), "{bad}");
            }
        }
    }
}
