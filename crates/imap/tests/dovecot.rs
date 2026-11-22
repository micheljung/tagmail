//! Integration tests against the Dovecot test server (`cargo xtask dovecot
//! up`). Run with `cargo test -p tagmail-imap --features dovecot-tests`;
//! `TAGMAIL_IMAP_ADDR` overrides the default `127.0.0.1:10143`.
#![cfg(feature = "dovecot-tests")]

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tagmail_imap::*;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;

const USER: &str = "user2";
const PASS: &str = "pass2";

fn addr() -> (String, u16) {
    let addr = std::env::var("TAGMAIL_IMAP_ADDR").unwrap_or_else(|_| "127.0.0.1:10143".into());
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"))
}

fn config() -> ServerConfig {
    let (host, port) = addr();
    ServerConfig::new(host, port, Security::Plain, USER, Password::new(PASS))
}

async fn connect() -> TokioSession {
    TokioSession::connect(&config()).await.expect("connect")
}

/// A fresh mailbox per test so tests can run in parallel.
async fn fresh_mailbox(s: &mut TokioSession, test: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("it-{test}-{nanos}");
    s.create_mailbox(&name).await.expect("create");
    name
}

fn message(i: usize) -> Vec<u8> {
    format!(
        "From: a@example.com\r\nTo: b@example.com\r\nSubject: msg {i}\r\n\
         Message-ID: <m{i}@it>\r\nDate: Sun, 22 Nov 2026 10:00:00 +0000\r\n\r\nbody {i}\r\n"
    )
    .into_bytes()
}

fn one(uid: Uid) -> UidSet {
    [uid].into_iter().collect()
}

#[tokio::test]
async fn capabilities_and_special_use() {
    let mut s = connect().await;
    let caps = s.capabilities().clone();
    assert!(caps.qresync() && caps.idle() && caps.r#move() && caps.uidplus());
    assert!(!caps.objectid(), "Dovecot 2.4 does not advertise OBJECTID");
    assert!(s.qresync_enabled());
    let boxes = s.list().await.unwrap();
    let sent = boxes.iter().find(|b| b.name == "Sent").unwrap();
    assert_eq!(sent.special_use, Some(SpecialUse::Sent));
    assert!(
        boxes
            .iter()
            .any(|b| b.special_use == Some(SpecialUse::Archive))
    );
}

#[tokio::test]
async fn wrong_password_is_an_auth_error() {
    let mut cfg = config();
    cfg.password = Password::new("wrong");
    let err = TokioSession::connect(&cfg).await.unwrap_err();
    assert!(err.is_auth(), "{err}");
}

#[tokio::test]
async fn qresync_reconnect_yields_vanished_earlier() {
    let mut a = connect().await;
    let mb = fresh_mailbox(&mut a, "qresync").await;
    let mut uids = Vec::new();
    for i in 0..3 {
        uids.push(a.append(&mb, &[], &message(i)).await.unwrap().uid.unwrap());
    }
    let sel = a.select(&mb, SelectOptions::default()).await.unwrap();
    let snapshot = sel.highest_modseq.expect("CONDSTORE modseq");
    a.store_flags(&one(uids[2]), StoreOp::Add, &[Flag::Seen], None)
        .await
        .unwrap();
    a.store_flags(&one(uids[1]), StoreOp::Add, &[Flag::Deleted], None)
        .await
        .unwrap();
    a.move_messages(&one(uids[0]), "Trash").await.unwrap();
    a.logout().await.unwrap();

    let mut b = connect().await;
    let all: UidSet = uids.iter().copied().collect();
    let sel2 = b
        .select(
            &mb,
            SelectOptions {
                read_only: false,
                qresync: Some(QresyncParams {
                    uid_validity: sel.uid_validity,
                    modseq: snapshot,
                    known_uids: Some(all),
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        sel2.vanished,
        one(uids[0]),
        "moved message is VANISHED (EARLIER)"
    );
    let changed: UidSet = sel2.changed.iter().filter_map(|m| m.uid).collect();
    assert!(
        changed.contains(uids[1]) && changed.contains(uids[2]),
        "{changed:?}"
    );
    let seen = sel2
        .changed
        .iter()
        .find(|m| m.uid == Some(uids[2]))
        .unwrap();
    assert!(seen.flags.as_ref().unwrap().contains(&Flag::Seen));
    assert!(seen.modseq.unwrap() > snapshot);

    let changes = b
        .fetch_changed_since(
            &uids.iter().copied().collect(),
            snapshot,
            &FetchSpec::flags(),
        )
        .await
        .unwrap();
    assert_eq!(changes.vanished, one(uids[0]));
    assert_eq!(changes.messages.len(), 2);
}

#[tokio::test]
async fn idle_sees_append_from_another_session() {
    let mut a = connect().await;
    let mb = fresh_mailbox(&mut a, "idle").await;
    a.select(&mb, SelectOptions::default()).await.unwrap();
    let mb2 = mb.clone();
    let appender = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut b = connect().await;
        b.append(&mb2, &[], &message(1)).await.unwrap();
    });
    let wake = Notify::new();
    let started = tokio::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(10), a.idle(&wake))
        .await
        .expect("change within 10 s")
        .unwrap();
    appender.await.unwrap();
    assert_eq!(r.stop, IdleStop::Changed);
    assert!(
        r.events.contains(&MailboxEvent::Exists(1)),
        "{:?}",
        r.events
    );
    assert!(started.elapsed() < Duration::from_secs(10));

    let notify = std::sync::Arc::new(Notify::new());
    let n2 = notify.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        n2.notify_one();
    });
    assert_eq!(a.idle(&notify).await.unwrap().stop, IdleStop::Woken);
    a.noop().await.unwrap();
}

#[tokio::test]
async fn keyword_store_is_visible_to_another_session() {
    let mut a = connect().await;
    let mb = fresh_mailbox(&mut a, "keyword").await;
    let uid = a.append(&mb, &[], &message(1)).await.unwrap().uid.unwrap();
    let sel = a.select(&mb, SelectOptions::default()).await.unwrap();
    assert!(sel.keyword_capable);
    let label = Flag::Keyword("$label_Work".into());
    let r = a
        .store_flags(
            &one(uid),
            StoreOp::Add,
            std::slice::from_ref(&label),
            sel.highest_modseq,
        )
        .await
        .unwrap();
    assert!(r.modified.is_empty());

    let mut b = connect().await;
    b.select(&mb, SelectOptions::default()).await.unwrap();
    let got = b.fetch(&one(uid), &FetchSpec::flags()).await.unwrap();
    assert!(got[0].flags.as_ref().unwrap().contains(&label));
    assert!(got[0].modseq.is_some());

    let stale = a
        .store_flags(&one(uid), StoreOp::Remove, &[label], sel.highest_modseq)
        .await
        .unwrap();
    assert_eq!(
        stale.modified,
        one(uid),
        "UNCHANGEDSINCE rejects a stale store"
    );
}

#[tokio::test]
async fn sync_window_fetch_and_uid_search() {
    let mut s = connect().await;
    let mb = fresh_mailbox(&mut s, "window").await;
    for i in 0..5 {
        s.append(&mb, &[], &message(i)).await.unwrap();
    }
    let sel = s.select(&mb, SelectOptions::default()).await.unwrap();
    let all = s.uid_search(SearchCriteria::All).await.unwrap();
    assert_eq!(all.len(), 5);
    let windows = windows_newest_first(&all, 2);
    assert_eq!(windows[0].max(), all.max());
    let metas = s
        .fetch(&windows[0], &FetchSpec::sync_window(true))
        .await
        .unwrap();
    assert_eq!(metas.len(), 2);
    let m = metas.iter().max_by_key(|m| m.uid).unwrap();
    assert_eq!(
        m.envelope.as_ref().unwrap().subject.as_deref(),
        Some(&b"msg 4"[..])
    );
    assert!(m.snippet.as_ref().unwrap().starts_with(b"body 4"));
    assert!(m.modseq.is_some() && m.email_id.is_none() && !m.degraded);
    assert_eq!(m.body.as_ref().unwrap().parts[0].mime_type, "text/plain");
    let found = s
        .uid_search(SearchCriteria::Header {
            name: "Message-ID".into(),
            value: "<m3@it>".into(),
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert!(sel.uid_next.is_some());
}

#[tokio::test]
async fn append_uid_and_move_with_copyuid() {
    let mut s = connect().await;
    let src = fresh_mailbox(&mut s, "move-src").await;
    let dst = fresh_mailbox(&mut s, "move-dst").await;
    let appended = s.append(&src, &[Flag::Seen], &message(1)).await.unwrap();
    let sel = s.select(&src, SelectOptions::default()).await.unwrap();
    assert_eq!(appended.uid_validity, Some(sel.uid_validity));
    let uid = appended.uid.unwrap();
    let moved = s.move_messages(&one(uid), &dst).await.unwrap();
    let cu = moved.copy_uid.expect("COPYUID");
    assert_eq!(cu.source, vec![uid]);
    let dst_status = s.status(&dst).await.unwrap();
    assert_eq!(Some(cu.uid_validity), dst_status.uid_validity);
    assert_eq!(dst_status.messages, Some(1));
}

#[tokio::test]
async fn without_condstore_no_modseq_is_requested() {
    let mut cfg = config();
    cfg.capability_override.remove = vec!["CONDSTORE".into(), "QRESYNC".into()];
    let mut s = TokioSession::connect(&cfg).await.unwrap();
    assert!(!s.qresync_enabled() && !s.capabilities().condstore());
    let mb = fresh_mailbox(&mut s, "nocondstore").await;
    s.append(&mb, &[], &message(1)).await.unwrap();
    let sel = s.select(&mb, SelectOptions::default()).await.unwrap();
    assert_eq!(sel.highest_modseq, None);
    let all = s.uid_search(SearchCriteria::All).await.unwrap();
    let metas = s.fetch(&all, &FetchSpec::sync_window(false)).await.unwrap();
    assert_eq!(metas[0].modseq, None);
    let err = s
        .fetch_changed_since(&all, ModSeq(1), &FetchSpec::flags())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
}

#[tokio::test]
async fn condstore_without_qresync_reports_expunge_events() {
    let mut cfg = config();
    cfg.capability_override.remove = vec!["QRESYNC".into()];
    let mut s = TokioSession::connect(&cfg).await.unwrap();
    assert!(!s.qresync_enabled() && s.capabilities().condstore());
    let mb = fresh_mailbox(&mut s, "condstore").await;
    let u1 = s.append(&mb, &[], &message(1)).await.unwrap().uid.unwrap();
    let u2 = s.append(&mb, &[], &message(2)).await.unwrap().uid.unwrap();
    let sel = s.select(&mb, SelectOptions::default()).await.unwrap();
    let since = sel.highest_modseq.unwrap();
    s.move_messages(&one(u1), "Trash").await.unwrap();
    assert!(s.take_events().contains(&MailboxEvent::Expunge(1)));
    s.store_flags(&one(u2), StoreOp::Add, &[Flag::Flagged], None)
        .await
        .unwrap();
    let all: UidSet = [u1, u2].into_iter().collect();
    let changes = s
        .fetch_changed_since(&all, since, &FetchSpec::flags())
        .await
        .unwrap();
    assert!(changes.vanished.is_empty(), "no VANISHED without QRESYNC");
    assert_eq!(changes.messages.len(), 1);
    assert_eq!(changes.messages[0].uid, Some(u2));
}

/// Streams a large APPEND literal so the test process never holds it.
async fn append_large(mailbox: &str, size: usize) {
    let (host, port) = addr();
    let mut r = BufReader::new(TcpStream::connect((host, port)).await.unwrap());
    let mut line = String::new();
    let mut expect = async |r: &mut BufReader<TcpStream>, prefix: &str| loop {
        line.clear();
        r.read_line(&mut line).await.unwrap();
        if line.starts_with(prefix) {
            return;
        }
        assert!(line.starts_with('*'), "unexpected {line:?}");
    };
    expect(&mut r, "* OK").await;
    r.get_mut()
        .write_all(format!("a LOGIN {USER} {PASS}\r\n").as_bytes())
        .await
        .unwrap();
    expect(&mut r, "a OK").await;
    let total = large_header().len() + size;
    r.get_mut()
        .write_all(format!("b APPEND {mailbox} {{{total}}}\r\n").as_bytes())
        .await
        .unwrap();
    expect(&mut r, "+").await;
    r.get_mut().write_all(large_header()).await.unwrap();
    let mut sent = 0;
    while sent < size {
        let n = (size - sent).min(LINE.len() * 1024);
        let chunk: Vec<u8> = LINE.iter().copied().cycle().take(n).collect();
        r.get_mut().write_all(&chunk).await.unwrap();
        sent += n;
    }
    r.get_mut().write_all(b"\r\n").await.unwrap();
    expect(&mut r, "b OK").await;
}

const LINE: &[u8] =
    b"QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0\r\n";

fn large_header() -> &'static [u8] {
    b"From: a@example.com\r\nSubject: big\r\nMIME-Version: 1.0\r\n\
      Content-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n"
}

/// Counts bytes and checks them against the generated body.
struct Verify {
    offset: usize,
    header_len: usize,
    mismatch: bool,
}

impl AsyncWrite for Verify {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        for &b in buf {
            let i = self.offset;
            let expected = if i < self.header_len {
                large_header()[i]
            } else {
                LINE[(i - self.header_len) % LINE.len()]
            };
            self.mismatch |= b != expected;
            self.offset += 1;
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap()
}

#[tokio::test]
async fn chunked_fetch_of_25_mb_message_has_bounded_memory() {
    let size = 25 * 1024 * 1024;
    let mut s = connect().await;
    let mb = fresh_mailbox(&mut s, "large").await;
    append_large(&mb, size).await;
    s.select(&mb, SelectOptions::default()).await.unwrap();
    let uid = s
        .uid_search(SearchCriteria::All)
        .await
        .unwrap()
        .max()
        .unwrap();

    let before = peak_rss_kb();
    let mut sink = Verify {
        offset: 0,
        header_len: large_header().len(),
        mismatch: false,
    };
    let got = s.fetch_body(uid, "", &mut sink, u64::MAX).await.unwrap();
    let grown_kb = peak_rss_kb() - before;
    assert_eq!(got.bytes as usize, large_header().len() + size);
    assert!(!got.truncated && !sink.mismatch);
    assert!(grown_kb < 16 * 1024, "peak RSS grew by {grown_kb} kB");

    let mut capped = Verify {
        offset: 0,
        header_len: large_header().len(),
        mismatch: false,
    };
    let got = s.fetch_body(uid, "", &mut capped, 3_000_000).await.unwrap();
    assert_eq!((got.bytes, got.truncated), (3_000_000, true));

    let mut cfg = config();
    cfg.body_chunk = 32 * 1024 * 1024;
    let mut whole = TokioSession::connect(&cfg).await.unwrap();
    whole.select(&mb, SelectOptions::default()).await.unwrap();
    let err = whole
        .fetch_body(uid, "", &mut tokio::io::sink(), u64::MAX)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ResponseTooLong { .. }), "{err}");
    whole
        .noop()
        .await
        .expect("session survives an oversized response");
}

#[tokio::test]
async fn pool_reuses_and_limits_real_sessions() {
    let pool = Pool::new(
        config(),
        PoolConfig {
            max_connections: 2,
            ..PoolConfig::default()
        },
    );
    let a = pool.acquire().await.unwrap();
    let b = pool.acquire().await.unwrap();
    let p = pool.clone();
    let third = tokio::spawn(async move { p.acquire().await.map(|s| s.capabilities().idle()) });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!third.is_finished());
    drop(a);
    assert!(third.await.unwrap().unwrap());
    drop(b);
}
