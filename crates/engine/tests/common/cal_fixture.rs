//! A per-test CalDAV account: the user's IMAP mailbox and calendar home are
//! emptied, then `default` is created again.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::cal_users::{self, STALWART_ORGANIZER_OFFSET};
use super::{PASSWORD, Server, reset_user_at, stalwart};

const RADICALE_ADDR: &str = "127.0.0.1:10232";
const STALWART_ADDR: &str = "127.0.0.1:10080";
const DEFAULT_CALENDAR: &str = "default";
const FORBIDDEN: u16 = 403;
const PROPFIND_HOME_DEPTH: &str = "1";
const COLLECTIONS_ONLY: &str = r#"<propfind xmlns="DAV:"><prop><resourcetype/></prop></propfind>"#;

pub struct DavResponse {
    pub status: u16,
    pub head: String,
    pub body: String,
}

pub struct CalFixture {
    pub user: &'static str,
    pub organizer: Option<String>,
}

/// `TAGMAIL_CALDAV_ADDR`, else Stalwart's or Radicale's published port.
pub fn caldav_addr() -> String {
    std::env::var("TAGMAIL_CALDAV_ADDR").unwrap_or_else(|_| {
        if stalwart() {
            STALWART_ADDR
        } else {
            RADICALE_ADDR
        }
        .into()
    })
}

pub fn caldav_base() -> String {
    format!("http://{}", caldav_addr())
}

/// The calendar home path of `user`, with trailing slash.
pub fn home_path(user: &str) -> String {
    if stalwart() {
        format!("/dav/cal/{user}/")
    } else {
        format!("/{user}/")
    }
}

pub async fn dav(
    user: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> DavResponse {
    let mut stream = TcpStream::connect(caldav_addr())
        .await
        .expect("CalDAV server");
    let auth = STANDARD.encode(format!("{user}:{PASSWORD}"));
    let mut request = format!(
        "{method} {path} HTTP/1.0\r\nHost: caldav\r\nAuthorization: Basic {auth}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    DavResponse {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

/// The text of every `href` element, in document order.
fn hrefs(xml: &str) -> Vec<String> {
    xml.split('<')
        .filter_map(|tag| {
            let (name, text) = tag.split_once('>')?;
            (name.rsplit(':').next() == Some("href")).then(|| text.trim().to_owned())
        })
        .collect()
}

async fn children(user: &str, path: &str) -> Vec<String> {
    let listing = dav(
        user,
        "PROPFIND",
        path,
        &[
            ("Depth", PROPFIND_HOME_DEPTH),
            ("Content-Type", "application/xml"),
        ],
        COLLECTIONS_ONLY,
    )
    .await;
    if listing.status != 207 {
        return Vec::new();
    }
    hrefs(&listing.body)
        .into_iter()
        .filter(|href| href.starts_with(path) && href != path)
        .collect()
}

/// Deletes `href`; a collection the server refuses to delete (Stalwart's
/// default calendar) is emptied instead.
async fn remove(user: &str, href: &str) {
    let deleted = dav(user, "DELETE", href, &[], "").await;
    if deleted.status == FORBIDDEN {
        for child in children(user, href).await {
            let deleted = dav(user, "DELETE", &child, &[], "").await;
            assert!(deleted.status < 300, "DELETE {child}: {}", deleted.head);
        }
    } else {
        assert!(
            deleted.status < 300 || deleted.status == 404,
            "DELETE {href}: {}",
            deleted.head
        );
    }
}

async fn reset_calendar_home(user: &str) {
    let home = home_path(user);
    for href in children(user, &home).await {
        remove(user, &href).await;
    }
    let default = format!("{home}{DEFAULT_CALENDAR}/");
    let exists = dav(user, "PROPFIND", &default, &[("Depth", "0")], "").await;
    if exists.status == 404 {
        let created = dav(user, "MKCALENDAR", &default, &[], "").await;
        assert_eq!(
            created.status, 201,
            "MKCALENDAR for {user}: {}",
            created.head
        );
    }
}

impl CalFixture {
    /// The user `test` owns in [`cal_users::CAL_USERS`], emptied first; on
    /// Stalwart also its organizer partner `calN+1`.
    pub async fn for_test(test: &str) -> CalFixture {
        let user = cal_users::user_of(test);
        reset_user_at(Server::Dovecot, user).await;
        reset_calendar_home(user).await;
        let organizer = if stalwart() {
            let organizer = format!(
                "cal{}",
                cal_users::number_of(user) + STALWART_ORGANIZER_OFFSET
            );
            reset_user_at(Server::Dovecot, &organizer).await;
            reset_calendar_home(&organizer).await;
            Some(organizer)
        } else {
            None
        };
        CalFixture { user, organizer }
    }
}
