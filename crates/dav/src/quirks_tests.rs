use url::Url;

use crate::mock::{Mock, Rule, options, url, well_known};
use crate::{
    Client, DavError, Discovery, DiscoveryEnv, DiscoveryTarget, MultigetItem, PutCondition,
    SchedulingMode, UidConflictKind, discover,
};

const HORDE_HOST: &str = "webmail.your-server.de";
const HORDE_IMAP: &str = "mail.your-server.de";
const HORDE_MAIL_DOMAIN: &str = "micheljung.ch";
const HORDE_HOME: &str = "/rpc/calendars/petition@micheljung.ch/";
const HORDE_CAL: &str = "/rpc/calendars/petition@micheljung.ch/calendar~XxTGk_wzksKr_bm2FWE92Mq/";
use crate::mock::DAV_HOST;

fn horde_rules() -> Vec<Rule> {
    vec![
        Rule::redirect(
            "PROPFIND",
            HORDE_IMAP,
            "/.well-known/caldav",
            "http://webmail.your-server.de/rpc/",
        ),
        Rule::new("PROPFIND", HORDE_HOST, "/rpc/")
            .challenge(fixture!("horde_sabredav18/unauthenticated.xml"))
            .body(fixture!("horde_sabredav18/root_principal.xml")),
        Rule::new("PROPFIND", HORDE_HOST, "/rpc/principals/petition@micheljung.ch/")
            .body(fixture!("horde_sabredav18/principal.xml")),
        Rule::new("OPTIONS", HORDE_HOST, HORDE_HOME).status(200).header(
            "dav",
            "1, 3, extended-mkcol, calendar-access, calendar-proxy, addressbook, 2, access-control, calendarserver-principal-property-search",
        ),
        Rule::new("PROPFIND", HORDE_HOST, HORDE_HOME).body(fixture!("horde_sabredav18/home_depth1.xml")),
        Rule::new("PROPFIND", HORDE_HOST, HORDE_CAL).body(fixture!("horde_sabredav18/etags_depth1.xml")),
        Rule::new("REPORT", HORDE_HOST, HORDE_CAL)
            .when_body("calendar-multiget")
            .body(fixture!("horde_sabredav18/multiget.xml")),
    ]
}

fn horde_env(mock: &Mock) -> DiscoveryEnv {
    well_known(
        mock.env(&[HORDE_HOST, HORDE_IMAP, HORDE_MAIL_DOMAIN, "your-server.de"]),
        &[HORDE_MAIL_DOMAIN, HORDE_IMAP, "your-server.de"],
    )
}

fn horde_target() -> DiscoveryTarget<'static> {
    DiscoveryTarget {
        email: "petition@micheljung.ch",
        imap_host: HORDE_IMAP,
        manual_url: None,
        confirmed_host: None,
    }
}

#[tokio::test]
async fn horde_sabredav18_discovery() {
    let mock = Mock::start(horde_rules()).await;
    let found = discover(horde_target(), &options(), &horde_env(&mock))
        .await
        .unwrap();
    let Discovery::Found(account) = found else {
        panic!("expected Found, got {found:?}");
    };
    assert_eq!(account.host, HORDE_HOST);
    assert_eq!(
        account.principal.path(),
        "/rpc/principals/petition@micheljung.ch/"
    );
    assert_eq!(account.home.path(), HORDE_HOME);
    assert_eq!(account.scheduling.inbox, None);
    assert!(account.scheduling.outbox.is_some());
    assert!(!account.scheduling.auto_schedule);
    assert_eq!(account.scheduling.detected(), SchedulingMode::Client);
    assert_eq!(account.calendars.len(), 1, "{:?}", account.calendars);
    let cal = &account.calendars[0];
    assert_eq!(cal.href.path(), HORDE_CAL);
    assert_eq!(
        cal.display_name.as_deref(),
        Some("Calendar of petition@micheljung.ch")
    );
    assert_eq!(cal.color.as_deref(), Some("#0d5c9dff"));
    assert_eq!(cal.ctag, None);
    assert_eq!(cal.sync_token, None);
    assert!(!cal.supports_sync_collection);

    let seen = mock.seen();
    assert!(
        seen.iter()
            .filter(|s| s.host != HORDE_HOST)
            .all(|s| !s.authorized),
        "credentials left the confirmed host: {seen:?}"
    );
    assert!(seen.iter().any(|s| s.host == HORDE_MAIL_DOMAIN));
}

#[tokio::test]
async fn horde_sabredav18_etag_poll_and_multiget() {
    let mock = Mock::start(horde_rules()).await;
    let client = Client::new(&options(), HORDE_HOST, &horde_env(&mock)).unwrap();
    let cal = url(HORDE_HOST, HORDE_CAL);
    let etags = client.list_etags(&cal).await.unwrap();
    assert_eq!(etags.len(), 2);
    assert_eq!(
        etags[0].href.path(),
        format!("{HORDE_CAL}20261201T090000Z-1a2b@micheljung.ch.ics")
    );
    assert_eq!(etags[0].etag.as_deref(), Some("\"9f1c2e\""));
    assert!(matches!(
        client.sync_collection(&cal, None).await,
        Err(DavError::NotFound { .. })
    ));

    let mut hrefs: Vec<Url> = etags.iter().map(|e| e.href.clone()).collect();
    hrefs.push(url(HORDE_HOST, &format!("{HORDE_CAL}never-answered.ics")));
    let items = client.multiget(&cal, &hrefs).await.unwrap();
    assert_eq!(items.len(), 3);
    let MultigetItem::Found(found) = &items[0] else {
        panic!("{items:?}");
    };
    assert!(found.data.contains("SUMMARY:Standup & review\r\n"));
    assert_eq!(found.etag.as_deref(), Some("\"9f1c2e\""));
    assert_eq!(items[1], MultigetItem::Missing(hrefs[1].clone()));
    assert_eq!(items[2], MultigetItem::Missing(hrefs[2].clone()));

    let report = mock
        .seen()
        .into_iter()
        .find(|s| s.method == "REPORT" && s.body.contains("multiget"))
        .unwrap();
    assert!(report.body.contains("<d:href>/rpc/calendars/petition%40micheljung.ch/calendar~XxTGk_wzksKr_bm2FWE92Mq/20261201T090000Z-1a2b%40micheljung.ch.ics</d:href>"));
}

#[tokio::test]
async fn missing_component_set_keeps_the_collection() {
    let home = fixture!("horde_sabredav18/home_depth1.xml").replace(
        r#"<cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set>"#,
        "",
    );
    let mock = Mock::start(vec![
        Rule::new("PROPFIND", HORDE_HOST, HORDE_HOME).body(&home),
    ])
    .await;
    let client = Client::new(&options(), HORDE_HOST, &mock.env(&[HORDE_HOST])).unwrap();
    let calendars = client
        .list_calendars(&url(HORDE_HOST, HORDE_HOME))
        .await
        .unwrap();
    assert_eq!(calendars.len(), 2);
    assert_eq!(calendars[1].components, None);
}

#[tokio::test]
async fn icloud_partition_needs_confirmation_then_follows_pnn() {
    let rules = vec![
        Rule::redirect(
            "PROPFIND",
            "icloud.com",
            "/.well-known/caldav",
            "http://caldav.icloud.com/",
        ),
        Rule::new("PROPFIND", "caldav.icloud.com", "/").body(fixture!("icloud/principal.xml")),
        Rule::new("PROPFIND", "caldav.icloud.com", "/1234567890/principal/")
            .body(fixture!("icloud/principal_props.xml")),
        Rule::new("OPTIONS", "p07-caldav.icloud.com", "/1234567890/calendars/")
            .status(200)
            .header(
                "dav",
                "1, 2, access-control, calendar-access, calendar-schedule, calendar-auto-schedule",
            ),
        Rule::new(
            "PROPFIND",
            "p07-caldav.icloud.com",
            "/1234567890/calendars/",
        )
        .body(fixture!("icloud/home_depth1.xml")),
    ];
    let mock = Mock::start(rules).await;
    let env = well_known(
        mock.env(&[
            "icloud.com",
            "caldav.icloud.com",
            "p07-caldav.icloud.com",
            "imap.mail.me.com",
            "mail.me.com",
        ]),
        &["icloud.com", "imap.mail.me.com", "mail.me.com"],
    );
    let mut target = DiscoveryTarget {
        email: "jane@icloud.com",
        imap_host: "imap.mail.me.com",
        manual_url: None,
        confirmed_host: None,
    };
    let first = discover(target, &options(), &env).await.unwrap();
    let Discovery::NeedsConfirmation { host, .. } = first else {
        panic!("expected NeedsConfirmation, got {first:?}");
    };
    assert_eq!(host, "caldav.icloud.com");
    assert!(mock.seen().iter().all(|s| !s.authorized));

    target.confirmed_host = Some("caldav.icloud.com");
    let Discovery::Found(account) = discover(target, &options(), &env).await.unwrap() else {
        panic!("expected Found");
    };
    assert_eq!(account.home.host_str(), Some("p07-caldav.icloud.com"));
    assert_eq!(account.scheduling.detected(), SchedulingMode::Server);
    assert_eq!(account.calendars.len(), 1);
    assert_eq!(account.calendars[0].ctag.as_deref(), Some("HwoQEgwAAA"));
    assert!(account.calendars[0].supports_sync_collection);
}

#[tokio::test]
async fn google_caldav_v2_principal() {
    let rules = vec![
        Rule::new("PROPFIND", "apidata.googleusercontent.com", "/caldav/v2/")
            .body(fixture!("google/principal.xml")),
        Rule::new(
            "PROPFIND",
            "apidata.googleusercontent.com",
            "/caldav/v2/jane.doe@gmail.com/user",
        )
        .body(fixture!("google/principal_props.xml")),
        Rule::new(
            "OPTIONS",
            "apidata.googleusercontent.com",
            "/caldav/v2/jane.doe@gmail.com/",
        )
        .status(200)
        .header("dav", "1, 2, 3, calendar-access, calendar-schedule"),
        Rule::new(
            "PROPFIND",
            "apidata.googleusercontent.com",
            "/caldav/v2/jane.doe@gmail.com/",
        )
        .body(fixture!("google/home_depth1.xml")),
    ];
    let mock = Mock::start(rules).await;
    let env = well_known(
        mock.env(&[
            "apidata.googleusercontent.com",
            "gmail.com",
            "imap.gmail.com",
        ]),
        &["gmail.com", "imap.gmail.com"],
    );
    let manual = url("apidata.googleusercontent.com", "/caldav/v2/");
    let target = DiscoveryTarget {
        email: "jane.doe@gmail.com",
        imap_host: "imap.gmail.com",
        manual_url: Some(&manual),
        confirmed_host: Some("apidata.googleusercontent.com"),
    };
    let Discovery::Found(account) = discover(target, &options(), &env).await.unwrap() else {
        panic!("expected Found");
    };
    assert_eq!(
        account.principal.path(),
        "/caldav/v2/jane.doe@gmail.com/user"
    );
    assert_eq!(account.scheduling.detected(), SchedulingMode::Client);
    assert_eq!(
        account.calendars[0].href.path(),
        "/caldav/v2/jane.doe@gmail.com/events/"
    );
    assert!(account.calendars[0].supports_sync_collection);
}

fn sync_client(mock: &Mock) -> Client {
    Client::new(&options(), DAV_HOST, &mock.env(&[DAV_HOST, "evil.example"])).unwrap()
}

#[tokio::test]
async fn truncated_507_continues() {
    let rules = vec![
        Rule::new("REPORT", DAV_HOST, "/cal/work/")
            .when_body("<d:sync-token></d:sync-token>")
            .body(fixture!("sync/initial.xml")),
        Rule::new("REPORT", DAV_HOST, "/cal/work/")
            .when_body("sync/12")
            .body(fixture!("sync/truncated_507.xml")),
    ];
    let mock = Mock::start(rules).await;
    let client = sync_client(&mock);
    let cal = url(DAV_HOST, "/cal/work/");
    let page = client.sync_collection(&cal, None).await.unwrap();
    assert_eq!(
        page.sync_token.as_deref(),
        Some("http://dav.tagmail.test/sync/12")
    );
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].etag.as_deref(), Some("\"a1\""));
    assert_eq!(page.removed, vec![url(DAV_HOST, "/cal/work/gone.ics")]);
    assert!(!page.truncated);

    let page = client
        .sync_collection(&cal, page.sync_token.as_deref())
        .await
        .unwrap();
    assert!(page.truncated);
    assert_eq!(
        page.sync_token.as_deref(),
        Some("http://dav.tagmail.test/sync/13")
    );
    assert_eq!(page.changed.len(), 1);
}

#[tokio::test]
async fn invalid_sync_token_403_409_full_resync() {
    for status in [403, 409] {
        let mock = Mock::start(vec![
            Rule::new("REPORT", DAV_HOST, "/cal/work/")
                .status(status)
                .body(fixture!("errors/valid_sync_token.xml")),
        ])
        .await;
        let err = sync_client(&mock)
            .sync_collection(&url(DAV_HOST, "/cal/work/"), Some("stale"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, DavError::InvalidSyncToken),
            "{status}: {err:?}"
        );
    }
}

#[tokio::test]
async fn disabled_report_and_properties_seam() {
    let mock = Mock::start(vec![
        Rule::new(
            "PROPFIND",
            "p07-caldav.icloud.com",
            "/1234567890/calendars/",
        )
        .body(fixture!("icloud/home_depth1.xml")),
    ])
    .await;
    let env = mock
        .env(&["p07-caldav.icloud.com"])
        .with_disabled_reports(&["sync-collection"])
        .with_disabled_properties(&["getctag", "sync-token"]);
    let client = Client::new(&options(), "caldav.icloud.com", &env).unwrap();
    let home = url("p07-caldav.icloud.com", "/1234567890/calendars/");
    let cal = &client.list_calendars(&home).await.unwrap()[0];
    assert!(!cal.supports_sync_collection);
    assert_eq!(cal.ctag, None);
    assert_eq!(cal.sync_token, None);
    assert!(matches!(
        client.sync_collection(&cal.href, None).await,
        Err(DavError::ReportUnsupported("sync-collection"))
    ));
}

#[tokio::test]
async fn href_encoding_variants() {
    let collection = "/cal/user@tagmail.test/work/";
    let mock = Mock::start(vec![
        Rule::new("PROPFIND", DAV_HOST, collection).body(fixture!("hrefs/etags_variants.xml")),
    ])
    .await;
    let etags = sync_client(&mock)
        .list_etags(&url(DAV_HOST, collection))
        .await
        .unwrap();
    let paths: Vec<&str> = etags.iter().map(|e| e.href.path()).collect();
    assert_eq!(
        paths,
        vec![
            "/cal/user@tagmail.test/work/space%20here.ics",
            "/cal/user@tagmail.test/work/caf%C3%A9~x.ics",
            "/cal/user@tagmail.test/work/rel%2Fslash.ics",
        ]
    );
}

#[tokio::test]
async fn redirect_loop_stops_after_five_hops() {
    let mock = Mock::start(vec![
        Rule::redirect("PROPFIND", DAV_HOST, "/a", "/b"),
        Rule::redirect("PROPFIND", DAV_HOST, "/b", "/a"),
    ])
    .await;
    let err = sync_client(&mock)
        .list_etags(&url(DAV_HOST, "/a"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DavError::TooManyRedirects { max: 5 }),
        "{err:?}"
    );
    assert_eq!(mock.seen().len(), 6);
}

#[tokio::test]
async fn put_delete_preconditions() {
    let ev = "/cal/work/e.ics";
    let mock = Mock::start(vec![
        Rule::new("PUT", DAV_HOST, ev)
            .when_body("CREATE")
            .status(201)
            .header("etag", "\"n1\""),
        Rule::new("PUT", DAV_HOST, ev)
            .when_body("STALE")
            .status(412),
        Rule::new("PUT", DAV_HOST, ev)
            .when_body("DUP")
            .status(403)
            .body(fixture!("errors/no_uid_conflict.xml")),
        Rule::new("PUT", DAV_HOST, ev)
            .when_body("SCHED")
            .status(409)
            .body(fixture!("errors/unique_scheduling_object.xml")),
        Rule::new("DELETE", DAV_HOST, ev).status(204),
    ])
    .await;
    let client = sync_client(&mock);
    let href = url(DAV_HOST, ev);
    assert_eq!(
        client
            .put(&href, "CREATE", PutCondition::Create)
            .await
            .unwrap()
            .as_deref(),
        Some("\"n1\"")
    );
    assert!(matches!(
        client
            .put(&href, "STALE", PutCondition::IfMatch("\"old\"".to_owned()))
            .await,
        Err(DavError::PreconditionFailed)
    ));
    let err = client
        .put(&href, "DUP", PutCondition::Overwrite)
        .await
        .unwrap_err();
    let DavError::UidConflict { kind, existing } = err else {
        panic!("{err:?}");
    };
    assert_eq!(kind, UidConflictKind::NoUidConflict);
    assert_eq!(existing.unwrap().path(), "/cal/work/existing@copy.ics");
    let err = client
        .put(&href, "SCHED", PutCondition::Overwrite)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DavError::UidConflict {
            kind: UidConflictKind::UniqueSchedulingObjectResource,
            ..
        }
    ));
    client.delete(&href, Some("\"n1\""), true).await.unwrap();
    assert!(matches!(
        client
            .delete(&url(DAV_HOST, "/cal/work/none.ics"), None, true)
            .await,
        Err(DavError::NotFound { .. })
    ));

    let seen = mock.seen();
    let header = |i: usize, name: &str| {
        seen[i]
            .headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert_eq!(header(0, "if-none-match").as_deref(), Some("*"));
    assert_eq!(header(1, "if-match").as_deref(), Some("\"old\""));
    assert_eq!(header(2, "if-match"), None);
    assert_eq!(header(4, "if-match").as_deref(), Some("\"n1\""));
    assert_eq!(
        header(0, "content-type").as_deref(),
        Some("text/calendar; charset=utf-8")
    );
}

#[tokio::test]
async fn move_falls_back_to_put_and_delete() {
    let from = "/cal/inbox/e.ics";
    let to = "/cal/work/e.ics";
    let mock = Mock::start(vec![
        Rule::new("MOVE", DAV_HOST, from).status(502),
        Rule::new("GET", DAV_HOST, from)
            .status(200)
            .header("etag", "\"m1\"")
            .body("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
        Rule::new("PUT", DAV_HOST, to).status(201),
        Rule::new("DELETE", DAV_HOST, from).status(204),
    ])
    .await;
    sync_client(&mock)
        .move_resource(&url(DAV_HOST, from), &url(DAV_HOST, to), Some("\"m1\""))
        .await
        .unwrap();
    let seen = mock.seen();
    let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
    assert_eq!(methods, vec!["MOVE", "GET", "PUT", "DELETE"]);
    assert_eq!(
        seen[0].headers.get("destination").unwrap(),
        "http://dav.tagmail.test/cal/work/e.ics"
    );
    assert_eq!(seen[0].headers.get("overwrite").unwrap(), "F");
    assert_eq!(seen[2].headers.get("if-none-match").unwrap(), "*");
    assert_eq!(seen[3].headers.get("if-match").unwrap(), "\"m1\"");
}

#[tokio::test]
async fn find_by_uid_searches_each_calendar() {
    let hit = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/cal/home/x.ics</d:href><d:propstat><d:prop><d:getetag>"u1"</d:getetag><c:calendar-data>BEGIN:VCALENDAR
UID:abc&lt;1&gt;@tagmail.test
END:VCALENDAR
</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let mock = Mock::start(vec![
        Rule::new("REPORT", DAV_HOST, "/cal/work/").body(r#"<d:multistatus xmlns:d="DAV:"/>"#),
        Rule::new("REPORT", DAV_HOST, "/cal/home/")
            .when_body("abc&lt;1&gt;@tagmail.test")
            .body(hit),
    ])
    .await;
    let calendars = [url(DAV_HOST, "/cal/work/"), url(DAV_HOST, "/cal/home/")];
    let found = sync_client(&mock)
        .find_by_uid(&calendars, "abc<1>@tagmail.test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.calendar, calendars[1]);
    assert_eq!(found.resource.href.path(), "/cal/home/x.ics");
    assert_eq!(found.resource.etag.as_deref(), Some("\"u1\""));
    assert!(found.resource.data.contains("UID:abc<1>@tagmail.test"));
    assert!(
        mock.seen()[0]
            .body
            .contains(r#"<c:text-match collation="i;octet">"#)
    );
}

#[tokio::test]
async fn nothing_found_is_not_discovered() {
    let mock = Mock::start(Vec::new()).await;
    let env = well_known(
        mock.env(&["tagmail.test", "imap.tagmail.test"]),
        &["tagmail.test", "imap.tagmail.test"],
    );
    let target = DiscoveryTarget {
        email: "me@tagmail.test",
        imap_host: "imap.tagmail.test",
        manual_url: None,
        confirmed_host: None,
    };
    assert!(matches!(
        discover(target, &options(), &env).await,
        Err(DavError::NotDiscovered)
    ));
}

const STALWART_CAL: &str = "/dav/cal/user1/dav-etag_listing/";

#[tokio::test]
async fn stalwart_encoded_hrefs_round_trip_through_multiget() {
    let mock = Mock::start(vec![
        Rule::new("PROPFIND", DAV_HOST, STALWART_CAL).body(fixture!("stalwart/etags_depth1.xml")),
        Rule::new("REPORT", DAV_HOST, STALWART_CAL)
            .when_body("multi-0%40x.ics")
            .body(fixture!("stalwart/multiget.xml")),
    ])
    .await;
    let client = Client::new(&options(), DAV_HOST, &mock.env(&[DAV_HOST])).unwrap();
    let cal = url(DAV_HOST, STALWART_CAL);
    let listed = client.list_etags(&cal).await.unwrap();
    assert_eq!(listed.len(), 3, "{listed:?}");
    let first = listed
        .iter()
        .find(|r| r.href.path().ends_with("/multi-0@x.ics"))
        .unwrap();
    let items = client
        .multiget(
            &cal,
            &[
                first.href.clone(),
                url(DAV_HOST, &format!("{STALWART_CAL}missing.ics")),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(&items[0], MultigetItem::Missing(h) if h.path().ends_with("/missing.ics")));
    let MultigetItem::Found(found) = &items[1] else {
        panic!("{items:?}");
    };
    assert_eq!(found.href, first.href);
    assert!(found.data.contains("UID:multi-0@tagmail.test"));
}

#[tokio::test]
async fn stalwart_no_uid_conflict_is_a_412() {
    let mock = Mock::start(vec![
        Rule::new(
            "PUT",
            DAV_HOST,
            "/dav/cal/user1/dav-no_uid_conflict/copy.ics",
        )
        .status(412)
        .body(fixture!("stalwart/no_uid_conflict_412.xml")),
    ])
    .await;
    let client = Client::new(&options(), DAV_HOST, &mock.env(&[DAV_HOST])).unwrap();
    let err = client
        .put(
            &url(DAV_HOST, "/dav/cal/user1/dav-no_uid_conflict/copy.ics"),
            "BEGIN:VCALENDAR",
            PutCondition::Create,
        )
        .await
        .unwrap_err();
    let DavError::UidConflict { kind, existing } = err else {
        panic!("{err:?}");
    };
    assert_eq!(kind, UidConflictKind::NoUidConflict);
    assert_eq!(
        existing.unwrap().path(),
        "/dav/cal/user1/dav-no_uid_conflict/original.ics"
    );
}
