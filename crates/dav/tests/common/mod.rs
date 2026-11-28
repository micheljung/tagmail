//! Shared real-server CalDAV checks for `radicale.rs` and `stalwart.rs`.
//! Each test owns one calendar collection, `dav-<test>`, recreated first.

#![allow(dead_code, reason = "each server suite uses a subset")]

use std::sync::Arc;

use reqwest::Method;
use tagmail_dav::{
    Client, ClientOptions, DavError, Discovery, DiscoveryEnv, DiscoveryTarget, MultigetItem,
    PutCondition,
};
use url::Url;

const MKCALENDAR_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:prop></d:set></c:mkcalendar>"#;
const HOST: &str = "127.0.0.1";

pub struct Server {
    pub addr: String,
    pub user: &'static str,
    pub password: &'static str,
    pub home: String,
}

impl Server {
    pub fn base(&self) -> Url {
        Url::parse(&format!("http://{}/", self.addr)).unwrap()
    }

    pub fn options(&self) -> ClientOptions {
        ClientOptions {
            username: self.user.to_owned(),
            password: self.password.to_owned().into(),
            tls: rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
            allow_insecure: true,
        }
    }

    pub fn client(&self) -> Client {
        Client::new(&self.options(), HOST, &DiscoveryEnv::system()).unwrap()
    }

    pub fn target(&self) -> DiscoveryTarget<'static> {
        DiscoveryTarget {
            email: "unused@tagmail.test",
            imap_host: HOST,
            manual_url: None,
            confirmed_host: None,
        }
    }

    /// An empty calendar collection named after `test`.
    pub async fn calendar(&self, test: &str) -> Url {
        let url = self
            .base()
            .join(&format!("{}dav-{test}/", self.home))
            .unwrap();
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(self.options().tls)
            .build()
            .unwrap();
        http.delete(url.clone())
            .basic_auth(self.user, Some(self.password))
            .send()
            .await
            .unwrap();
        let created = http
            .request(Method::from_bytes(b"MKCALENDAR").unwrap(), url.clone())
            .basic_auth(self.user, Some(self.password))
            .header("content-type", "application/xml; charset=utf-8")
            .body(MKCALENDAR_BODY)
            .send()
            .await
            .unwrap();
        assert!(
            created.status().is_success(),
            "MKCALENDAR {url}: {}",
            created.status()
        );
        url
    }
}

pub fn event(uid: &str, summary: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//dav tests//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261125T100000Z\r\nDTSTART:20261201T090000Z\r\nDTEND:20261201T100000Z\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

pub async fn manual_url(server: &Server, test: &str) {
    let calendar = server.calendar(test).await;
    let manual = server.base();
    let target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..server.target()
    };
    let found = tagmail_dav::discover(target, &server.options(), &DiscoveryEnv::system())
        .await
        .unwrap();
    let Discovery::Found(account) = found else {
        panic!("expected Found, got {found:?}");
    };
    assert_eq!(account.host, HOST);
    assert_eq!(account.home.path(), server.home);
    assert_discovered(server, &account.calendars, &calendar).await;
}

pub async fn assert_discovered(
    server: &Server,
    calendars: &[tagmail_dav::CalendarInfo],
    calendar: &Url,
) {
    assert!(
        calendars
            .iter()
            .any(|c| tagmail_dav::same_resource(&c.href, calendar)),
        "{calendar} not in {calendars:?}"
    );
    let listed = server
        .client()
        .list_calendars(&server.base().join(&server.home).unwrap())
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|c| tagmail_dav::same_resource(&c.href, calendar))
    );
}

pub async fn put_get_delete_with_if_match(server: &Server, test: &str) {
    let calendar = server.calendar(test).await;
    let client = server.client();
    let href = calendar.join("put-get-delete.ics").unwrap();
    let ics = event("put-get-delete@tagmail.test", "First");
    client.put(&href, &ics, PutCondition::Create).await.unwrap();
    assert!(matches!(
        client.put(&href, &ics, PutCondition::Create).await,
        Err(DavError::PreconditionFailed)
    ));
    let stored = client.get(&href).await.unwrap();
    assert!(stored.data.contains("SUMMARY:First"));
    let etag = stored.etag.expect("ETag on GET");
    client
        .put(
            &href,
            &event("put-get-delete@tagmail.test", "Second"),
            PutCondition::IfMatch(etag.clone()),
        )
        .await
        .unwrap();
    let current = client.get(&href).await.unwrap();
    assert!(current.data.contains("SUMMARY:Second"));
    assert!(matches!(
        client.delete(&href, Some(&etag), true).await,
        Err(DavError::PreconditionFailed)
    ));
    client
        .delete(&href, current.etag.as_deref(), true)
        .await
        .unwrap();
    assert!(matches!(
        client.get(&href).await,
        Err(DavError::NotFound { .. })
    ));
}

pub async fn stale_if_match_is_412(server: &Server, test: &str) {
    let calendar = server.calendar(test).await;
    let client = server.client();
    let href = calendar.join("stale.ics").unwrap();
    client
        .put(
            &href,
            &event("stale@tagmail.test", "One"),
            PutCondition::Create,
        )
        .await
        .unwrap();
    let err = client
        .put(
            &href,
            &event("stale@tagmail.test", "Two"),
            PutCondition::IfMatch("\"not-the-current-etag\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DavError::PreconditionFailed), "{err:?}");
}

pub async fn etag_listing_and_multiget(server: &Server, test: &str) {
    let calendar = server.calendar(test).await;
    let client = server.client();
    let mut hrefs = Vec::new();
    for i in 0..3 {
        let href = calendar.join(&format!("multi-{i}@x.ics")).unwrap();
        client
            .put(
                &href,
                &event(&format!("multi-{i}@tagmail.test"), &format!("Event {i}")),
                PutCondition::Create,
            )
            .await
            .unwrap();
        hrefs.push(href);
    }
    let mut listed = client.list_etags(&calendar).await.unwrap();
    listed.sort_by(|a, b| a.href.as_str().cmp(b.href.as_str()));
    assert_eq!(listed.len(), 3, "{listed:?}");
    for (r, href) in listed.iter().zip(&hrefs) {
        assert!(tagmail_dav::same_resource(&r.href, href), "{r:?} vs {href}");
        assert!(r.etag.is_some());
    }
    let mut wanted: Vec<Url> = listed.iter().map(|r| r.href.clone()).collect();
    wanted.push(calendar.join("missing.ics").unwrap());
    let items = client.multiget(&calendar, &wanted).await.unwrap();
    let found: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            MultigetItem::Found(r) => Some(r),
            MultigetItem::Missing(_) => None,
        })
        .collect();
    assert_eq!(found.len(), 3, "{items:?}");
    for r in &found {
        let etag = &listed.iter().find(|l| l.href == r.href).unwrap().etag;
        assert_eq!(&r.etag, etag);
        assert!(r.data.contains("BEGIN:VEVENT"));
    }
    assert!(
        items
            .iter()
            .any(|i| matches!(i, MultigetItem::Missing(h) if h.path().ends_with("/missing.ics")))
    );
}

/// What a server does with a sync token it never issued.
#[derive(Clone, Copy)]
pub enum UnknownToken {
    /// 403/409 `valid-sync-token` (RFC 6578 §3.2), e.g. Radicale.
    Rejected,
    /// Answers as for an initial sync, e.g. Stalwart.
    FullListing,
}

pub async fn sync_collection_incremental(server: &Server, test: &str, unknown: UnknownToken) {
    let calendar = server.calendar(test).await;
    let client = server.client();
    let first = calendar.join("first.ics").unwrap();
    client
        .put(
            &first,
            &event("first@tagmail.test", "First"),
            PutCondition::Create,
        )
        .await
        .unwrap();
    let state = client.calendar_state(&calendar).await.unwrap();
    assert!(state.supports_sync_collection, "{state:?}");
    let initial = client.sync_collection(&calendar, None).await.unwrap();
    assert!(!initial.truncated);
    assert_eq!(initial.changed.len(), 1, "{initial:?}");
    let token = initial.sync_token.expect("sync token");

    let second = calendar.join("second.ics").unwrap();
    client
        .put(
            &second,
            &event("second@tagmail.test", "Second"),
            PutCondition::Create,
        )
        .await
        .unwrap();
    let first_etag = client.get(&first).await.unwrap().etag;
    client
        .delete(&first, first_etag.as_deref(), true)
        .await
        .unwrap();

    let delta = client
        .sync_collection(&calendar, Some(&token))
        .await
        .unwrap();
    assert!(
        delta
            .changed
            .iter()
            .any(|r| tagmail_dav::same_resource(&r.href, &second)),
        "{delta:?}"
    );
    assert!(
        delta
            .removed
            .iter()
            .any(|h| tagmail_dav::same_resource(h, &first)),
        "{delta:?}"
    );
    assert_ne!(delta.sync_token.as_deref(), Some(token.as_str()));

    let bogus = client
        .sync_collection(&calendar, Some("http://bogus.invalid/sync/1"))
        .await;
    match unknown {
        UnknownToken::Rejected => {
            assert!(
                matches!(bogus, Err(DavError::InvalidSyncToken)),
                "{bogus:?}"
            );
        }
        UnknownToken::FullListing => {
            let page = bogus.unwrap();
            assert!(
                page.changed
                    .iter()
                    .any(|r| tagmail_dav::same_resource(&r.href, &second)),
                "{page:?}"
            );
        }
    }
}
