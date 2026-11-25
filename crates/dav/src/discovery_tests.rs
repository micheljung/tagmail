//! Discovery and credential-safety rules (plan AC 9) against the in-process
//! mock. `well_known_stalwart` and `manual_url` against real servers live in
//! `tests/stalwart.rs` and `tests/radicale.rs`.

use crate::mock::{
    DAV_HOST, Mock, Rule, TLS_HOST, options, tls_options, tls_redirect, url, well_known,
};
use crate::{
    Client, DavError, Discovery, DiscoveryEnv, DiscoveryTarget, discover, host_allowed,
    registrable_domain,
};

const PRINCIPAL: &str = "/rpc/principals/petition@micheljung.ch/";
const HOME: &str = "/rpc/calendars/petition@micheljung.ch/";
const OVERRIDE_BASIC: &str = "Basic Y2FsZW5kYXItb25seUB0YWdtYWlsLnRlc3Q6b3RoZXItc2VjcmV0";

/// A SabreDAV account under `host` whose context URL is `context`.
fn sabre_account(host: &'static str, context: &str) -> Vec<Rule> {
    vec![
        Rule::new("PROPFIND", host, context)
            .challenge(fixture!("horde_sabredav18/unauthenticated.xml"))
            .body(fixture!("horde_sabredav18/root_principal.xml")),
        Rule::new("PROPFIND", host, PRINCIPAL).body(fixture!("horde_sabredav18/principal.xml")),
        Rule::new("OPTIONS", host, HOME)
            .status(200)
            .header("dav", "1, 2, 3, calendar-access"),
        Rule::new("PROPFIND", host, HOME).body(fixture!("horde_sabredav18/home_depth1.xml")),
    ]
}

fn sync_client(mock: &Mock) -> Client {
    Client::new(&options(), DAV_HOST, &mock.env(&[DAV_HOST, "evil.example"])).unwrap()
}

fn target<'a>(email: &'a str, imap_host: &'a str) -> DiscoveryTarget<'a> {
    DiscoveryTarget {
        email,
        imap_host,
        manual_url: None,
        confirmed_host: None,
    }
}

#[tokio::test]
async fn manual_url_is_the_only_candidate() {
    let mock = Mock::start(sabre_account(DAV_HOST, "/dav/")).await;
    let env = well_known(
        mock.env(&[DAV_HOST, "tagmail.test"]),
        &[DAV_HOST, "tagmail.test"],
    );
    let manual = url(DAV_HOST, "/dav/");
    let target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("me@tagmail.test", DAV_HOST)
    };
    let found = discover(target, &options(), &env).await.unwrap();
    assert!(
        matches!(found, Discovery::Found(ref a) if a.context_url == manual),
        "{found:?}"
    );
    assert!(
        mock.seen().iter().all(|s| !s.path.contains(".well-known")),
        "{:?}",
        mock.seen()
    );
}

#[tokio::test]
async fn manual_url_falls_back_to_its_well_known() {
    let mut rules = vec![
        Rule::new("PROPFIND", DAV_HOST, "/")
            .public()
            .status(200)
            .body("<html/>"),
        Rule::redirect("PROPFIND", DAV_HOST, "/.well-known/caldav", "/dav/"),
    ];
    rules.extend(sabre_account(DAV_HOST, "/dav/"));
    let mock = Mock::start(rules).await;
    let manual = url(DAV_HOST, "/");
    let target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("me@tagmail.test", DAV_HOST)
    };
    let found = discover(target, &options(), &mock.env(&[DAV_HOST]))
        .await
        .unwrap();
    assert!(
        matches!(found, Discovery::Found(ref a) if a.context_url.path() == "/dav/"),
        "{found:?}"
    );
}

#[tokio::test]
async fn bare_401_on_a_manual_url_counts() {
    let mut rules = sabre_account(DAV_HOST, "/dav/");
    rules[0] = Rule::new("PROPFIND", DAV_HOST, "/dav/")
        .body(fixture!("horde_sabredav18/root_principal.xml"));
    let mock = Mock::start(rules).await;
    let manual = url(DAV_HOST, "/dav/");
    let target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("me@tagmail.test", DAV_HOST)
    };
    let found = discover(target, &options(), &mock.env(&[DAV_HOST]))
        .await
        .unwrap();
    assert!(matches!(found, Discovery::Found(_)), "{found:?}");
}

#[tokio::test]
async fn bare_401_on_a_guessed_url_is_not_dav() {
    let mock = Mock::start(vec![Rule::new("PROPFIND", DAV_HOST, "/.well-known/caldav")]).await;
    let env = well_known(
        mock.env(&[DAV_HOST, "tagmail.test"]),
        &[DAV_HOST, "tagmail.test"],
    );
    let result = discover(target("me@dav.tagmail.test", DAV_HOST), &options(), &env).await;
    assert!(matches!(result, Err(DavError::NotDiscovered)), "{result:?}");
    assert!(mock.seen().iter().all(|s| !s.authorized));
}

#[tokio::test]
async fn dav_error_401_on_a_guessed_url_counts() {
    let mock = Mock::start(sabre_account(DAV_HOST, "/.well-known/caldav")).await;
    let env = well_known(
        mock.env(&[DAV_HOST, "tagmail.test"]),
        &[DAV_HOST, "tagmail.test"],
    );
    let found = discover(target("me@dav.tagmail.test", DAV_HOST), &options(), &env)
        .await
        .unwrap();
    assert!(
        matches!(found, Discovery::Found(ref a) if a.calendars.len() == 1),
        "{found:?}"
    );
}

#[tokio::test]
async fn calendar_access_header_counts() {
    let mut rules = sabre_account(DAV_HOST, "/.well-known/caldav");
    rules[0] = Rule::new("PROPFIND", DAV_HOST, "/.well-known/caldav")
        .challenge_header("dav", "1, 2, calendar-access")
        .body(fixture!("horde_sabredav18/root_principal.xml"));
    let mock = Mock::start(rules).await;
    let env = well_known(
        mock.env(&[DAV_HOST, "tagmail.test"]),
        &[DAV_HOST, "tagmail.test"],
    );
    let found = discover(target("me@dav.tagmail.test", DAV_HOST), &options(), &env)
        .await
        .unwrap();
    assert!(matches!(found, Discovery::Found(_)), "{found:?}");
}

#[tokio::test]
async fn explore_error_moves_to_next_candidate() {
    let empty = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/.well-known/caldav</d:href><d:propstat><d:prop/><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let mut rules = vec![
        Rule::new("PROPFIND", "tagmail.test", "/.well-known/caldav")
            .public()
            .body(empty),
    ];
    rules.extend(sabre_account("imap.tagmail.test", "/.well-known/caldav"));
    let mock = Mock::start(rules).await;
    let env = well_known(
        mock.env(&["tagmail.test", "imap.tagmail.test"]),
        &["tagmail.test", "imap.tagmail.test"],
    );
    let found = discover(
        target("me@tagmail.test", "imap.tagmail.test"),
        &options(),
        &env,
    )
    .await
    .unwrap();
    assert!(
        matches!(found, Discovery::Found(ref a) if a.host == "imap.tagmail.test"),
        "{found:?}"
    );
}

#[tokio::test]
async fn unauthorized_stops_discovery() {
    let mut rules = vec![
        Rule::new("PROPFIND", "tagmail.test", "/.well-known/caldav")
            .public()
            .status(401)
            .body(fixture!("horde_sabredav18/unauthenticated.xml")),
    ];
    rules.extend(sabre_account("imap.tagmail.test", "/.well-known/caldav"));
    let mock = Mock::start(rules).await;
    let env = well_known(
        mock.env(&["tagmail.test", "imap.tagmail.test"]),
        &["tagmail.test", "imap.tagmail.test"],
    );
    let result = discover(
        target("me@tagmail.test", "imap.tagmail.test"),
        &options(),
        &env,
    )
    .await;
    assert!(matches!(result, Err(DavError::Unauthorized)), "{result:?}");
    assert!(mock.seen().iter().all(|s| s.host != "imap.tagmail.test"));
}

#[tokio::test]
async fn credential_override() {
    let mock = Mock::start(sabre_account(DAV_HOST, "/dav/")).await;
    let manual = url(DAV_HOST, "/dav/");
    let target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("petition@micheljung.ch", DAV_HOST)
    };
    let mut opts = options();
    opts.username = "calendar-only@tagmail.test".to_owned();
    opts.password = "other-secret".to_owned().into();
    discover(target, &opts, &mock.env(&[DAV_HOST]))
        .await
        .unwrap();
    let authorized: Vec<_> = mock.seen().into_iter().filter(|s| s.authorized).collect();
    assert!(!authorized.is_empty());
    for seen in authorized {
        assert_eq!(seen.headers.get("authorization").unwrap(), OVERRIDE_BASIC);
    }
}

#[tokio::test]
async fn https_to_http_redirect_rejected_without_allow_insecure() {
    let addr = tls_redirect("http://secure.tagmail.test/plain/").await;
    let env = DiscoveryEnv::system().with_resolver(TLS_HOST, addr);
    let client = Client::new(&tls_options(), TLS_HOST, &env).unwrap();
    let start = url::Url::parse("https://secure.tagmail.test/dav/").unwrap();
    let err = client.list_etags(&start).await.unwrap_err();
    assert!(
        matches!(err, DavError::InsecureUrl { ref url } if url.scheme() == "http"),
        "{err:?}"
    );
}

#[tokio::test]
async fn ip_literal_and_dot_test_registrable_domain() {
    assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    assert!(!host_allowed("127.0.0.1", "127.0.0.2"));
    assert!(!host_allowed("127.0.0.1", "localhost"));
    assert_eq!(registrable_domain("a.mail.test"), "mail.test");
    assert!(host_allowed("imap.mail.test", "dav.mail.test"));
    assert!(!host_allowed("imap.mail.test", "dav.other.test"));

    let mock = Mock::start(sabre_account("127.0.0.1", "/dav/")).await;
    let manual = url::Url::parse(&format!("http://127.0.0.1:{}/dav/", mock.addr.port())).unwrap();
    let ip_target = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("me@tagmail.test", "127.0.0.1")
    };
    let found = discover(ip_target, &options(), &DiscoveryEnv::system())
        .await
        .unwrap();
    assert!(
        matches!(found, Discovery::Found(ref a) if a.host == "127.0.0.1"),
        "{found:?}"
    );
    let other_ip = DiscoveryTarget {
        manual_url: Some(&manual),
        ..target("me@tagmail.test", "127.0.0.2")
    };
    let result = discover(other_ip, &options(), &DiscoveryEnv::system())
        .await
        .unwrap();
    assert!(
        matches!(result, Discovery::NeedsConfirmation { .. }),
        "{result:?}"
    );
}

#[tokio::test]
async fn redirect_to_unconfirmed_host_refused_before_auth() {
    let mock = Mock::start(vec![
        Rule::redirect(
            "PROPFIND",
            DAV_HOST,
            "/cal/work/",
            "http://evil.example/steal",
        ),
        Rule::new("PROPFIND", "evil.example", "/steal").public(),
    ])
    .await;
    let err = sync_client(&mock)
        .list_etags(&url(DAV_HOST, "/cal/work/"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DavError::UnconfirmedHost { ref host } if host == "evil.example"),
        "{err:?}"
    );
    assert!(mock.seen().iter().all(|s| s.host != "evil.example"));
}

#[tokio::test]
async fn http_rejected_without_allow_insecure() {
    let mut opts = options();
    opts.allow_insecure = false;
    let client = Client::new(&opts, DAV_HOST, &DiscoveryEnv::system()).unwrap();
    let err = client
        .list_etags(&url(DAV_HOST, "/cal/work/"))
        .await
        .unwrap_err();
    assert!(matches!(err, DavError::InsecureUrl { .. }), "{err:?}");
}

#[tokio::test]
async fn cross_domain_requires_confirmation_before_auth() {
    let mock = Mock::start(vec![
        Rule::redirect(
            "PROPFIND",
            "tagmail.test",
            "/.well-known/caldav",
            "http://dav.provider.test/dav/",
        ),
        Rule::new("PROPFIND", "dav.provider.test", "/dav/"),
    ])
    .await;
    let env = well_known(
        mock.env(&["tagmail.test", "dav.provider.test"]),
        &["tagmail.test"],
    );
    let target = DiscoveryTarget {
        email: "me@tagmail.test",
        imap_host: "imap.mailhost.test",
        manual_url: None,
        confirmed_host: None,
    };
    let result = discover(target, &options(), &env).await.unwrap();
    assert!(
        matches!(
            result,
            Discovery::NeedsConfirmation { ref host, .. } if host == "dav.provider.test"
        ),
        "{result:?}"
    );
    assert!(mock.seen().iter().all(|s| !s.authorized));
}
