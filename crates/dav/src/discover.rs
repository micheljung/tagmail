use reqwest::StatusCode;
use url::Url;

use crate::client::{Client, Depth, MAX_SMALL_BODY_BYTES, Req, read_limited};
use crate::env::DiscoveryEnv;
use crate::error::{DavError, Result};
use crate::host::{host_allowed, parent_domain, url_host};
use crate::href::normalize_href;
use crate::model::{ClientOptions, DiscoveredAccount, Discovery, DiscoveryTarget, Scheduling};
use crate::xml;

const WELL_KNOWN: &str = ".well-known/caldav";
const AUTO_SCHEDULE: &str = "calendar-auto-schedule";
const CALENDAR_ACCESS: &str = "calendar-access";

/// Finds the account's CalDAV server: the manual URL (then the well-known
/// path on its host) when the owner gave one, else `.well-known/caldav` on the mail domain, the IMAP host and
/// its parent domain. Probes are unauthenticated; credentials are only sent
/// once the server's registrable domain matches the IMAP host's or a
/// confirmed host.
pub async fn discover(
    target: DiscoveryTarget<'_>,
    options: &ClientOptions,
    env: &DiscoveryEnv,
) -> Result<Discovery> {
    let probe = Client::build(options, None, env)?;
    let mut last_error = None;
    let manual = target.manual_url.is_some();
    for candidate in candidates(&target, env) {
        let context = match probe_url(&probe, &candidate, manual).await {
            Ok(Some(url)) => url,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(url = %candidate, error = %e, "CalDAV probe failed");
                last_error = Some(e);
                continue;
            }
        };
        let host = url_host(&context).ok_or_else(|| DavError::InsecureUrl {
            url: context.clone(),
        })?;
        let trusted = host_allowed(target.imap_host, &host)
            || target
                .confirmed_host
                .is_some_and(|c| host_allowed(c, &host));
        if !trusted {
            return Ok(Discovery::NeedsConfirmation { host, url: context });
        }
        let client = Client::build(options, Some(&host), env)?;
        match explore(&client, host, context).await {
            Ok(account) => return Ok(Discovery::Found(Box::new(account))),
            Err(DavError::Unauthorized) => return Err(DavError::Unauthorized),
            Err(e) => {
                tracing::debug!(url = %candidate, error = %e, "CalDAV candidate unusable");
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or(DavError::NotDiscovered))
}

fn candidates(target: &DiscoveryTarget<'_>, env: &DiscoveryEnv) -> Vec<Url> {
    if let Some(manual) = target.manual_url {
        let mut urls = vec![manual.clone()];
        if let Ok(origin_well_known) = manual.join(&format!("/{WELL_KNOWN}"))
            && origin_well_known != *manual
        {
            urls.push(origin_well_known);
        }
        return urls;
    }
    let mut domains: Vec<String> = Vec::new();
    if let Some((_, domain)) = target.email.rsplit_once('@') {
        domains.push(domain.to_ascii_lowercase());
    }
    domains.push(target.imap_host.to_ascii_lowercase());
    domains.extend(parent_domain(target.imap_host));
    let mut urls = Vec::new();
    for domain in domains {
        if let Some(url) = env
            .well_known_base(&domain)
            .and_then(|base| base.join(WELL_KNOWN).ok())
            && !urls.contains(&url)
        {
            urls.push(url);
        }
    }
    urls
}

/// The URL a candidate ends at if it shows a CalDAV signal: a multistatus,
/// a `DAV` header with `calendar-access`, or a 401 carrying a `DAV:error`
/// body. A bare 401 only counts on the owner's manual URL or at the end of a
/// `.well-known/caldav` redirect (RFC 6764), which Radicale and iCloud need;
/// a guessed URL answering 401 itself never gets the IMAP login.
async fn probe_url(probe: &Client, url: &Url, manual: bool) -> Result<Option<Url>> {
    let (response, final_url) = probe
        .send(
            Req::propfind(url, Depth::Zero, "<d:current-user-principal/>"),
            false,
        )
        .await?;
    let status = response.status();
    let calendar_access = response
        .headers()
        .get_all("DAV")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(CALENDAR_ACCESS));
    let is_dav = if status == StatusCode::MULTI_STATUS || calendar_access {
        true
    } else if status == StatusCode::UNAUTHORIZED {
        manual
            || final_url != *url
            || read_limited(response, MAX_SMALL_BODY_BYTES)
                .await
                .is_ok_and(|body| xml::is_dav_error(&body))
    } else {
        false
    };
    Ok(is_dav.then_some(final_url))
}

async fn explore(client: &Client, host: String, context: Url) -> Result<DiscoveredAccount> {
    let (base, responses) = client
        .propfind_all(Req::propfind(
            &context,
            Depth::Zero,
            "<d:current-user-principal/>",
        ))
        .await?;
    let principal = match responses
        .into_iter()
        .find_map(|(_, r)| r.props.current_user_principal)
    {
        Some(href) => normalize_href(&base, &href)?,
        None => base.clone(),
    };
    let (base, responses) = client
        .propfind_all(Req::propfind(
            &principal,
            Depth::Zero,
            "<c:calendar-home-set/><c:schedule-inbox-URL/><c:schedule-outbox-URL/>",
        ))
        .await?;
    let props = responses
        .into_iter()
        .map(|(_, r)| r.props)
        .find(|p| !p.calendar_home_set.is_empty())
        .ok_or(DavError::NoCalendarHome)?;
    let resolve = |href: Option<String>| -> Result<Option<Url>> {
        href.map(|h| normalize_href(&base, &h)).transpose()
    };
    let home = normalize_href(&base, &props.calendar_home_set[0])?;
    let inbox = resolve(props.schedule_inbox)?;
    let outbox = resolve(props.schedule_outbox)?;
    let auto_schedule = match client.capabilities(&home).await {
        Ok(tokens) => tokens.iter().any(|t| t.eq_ignore_ascii_case(AUTO_SCHEDULE)),
        Err(DavError::Unauthorized) => return Err(DavError::Unauthorized),
        Err(e) => {
            tracing::debug!(error = %e, "CalDAV OPTIONS failed");
            false
        }
    };
    let calendars = client.list_calendars(&home).await?;
    Ok(DiscoveredAccount {
        host,
        context_url: context,
        principal,
        home,
        scheduling: Scheduling {
            auto_schedule,
            inbox,
            outbox,
        },
        calendars,
    })
}
