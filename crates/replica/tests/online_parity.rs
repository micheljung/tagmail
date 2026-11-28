//! Parity row (plan §6, rows 10/11/16/19): a replica of 100k threads over
//! two years lists the same views and, online, answers the benchmark
//! queries with the server's hits, out-of-window ones included.

#[cfg(feature = "calendar")]
mod calendar_seed;
mod fixture;

use fixture::{DAY, Stack, get, ids, now, search_path};
use tagmail_api_core::{PARTIAL_HEADER, Store, View};
use tagmail_core::{MailboxRowId, MessageRowId, ThreadId};
use tagmail_replica::Router;
use tagmail_store::{BlobKey, DEFAULT_RAW_CAP, FtsDocument};

const THREADS: i64 = 100_000;
const SPAN: i64 = 2 * 365 * DAY;
const WINDOW: i64 = 90 * DAY;
const SUBJECTS: [&str; 10] = [
    "quarterly report",
    "invoice",
    "budget review",
    "meeting notes",
    "trip to Zürich",
    "dinner plans",
    "contract draft",
    "server migration",
    "schedule update",
    "the deadline",
];
/// `xtask bench-search`'s queries over the same vocabulary.
const QUERIES: [&str; 30] = [
    "the",
    "meeting",
    "Zürich",
    "\"quarterly report\"",
    "\"the deadline\"",
    "migr*",
    "subject:invoice",
    "subject:\"budget review\"",
    "from:contact42@example.com",
    "from:\"Alice Smith\"",
    "to:user1@tagmail.test",
    "cc:contact7",
    "label:work",
    "label:receipts",
    "in:sent",
    "in:archive budget",
    "is:unread",
    "is:starred",
    "has:attachment",
    "after:2025/01/01 before:2025/02/01",
    "invoice from:contact42@example.com after:2020/01/01",
    "the after:2026/06/01",
    "-label:work",
    "-the",
    "invoice OR contract",
    "(trip OR dinner) is:starred",
    "budget -meeting",
    "label:travel schedule",
    "in:anywhere server",
    "from:contact7 has:attachment is:unread",
];
const OLD_RAW: i64 = 1_001;
/// Ids below this are older than the 90-day window.
const OUT_OF_WINDOW_BELOW: i64 = THREADS - THREADS * WINDOW / SPAN;

/// Thread `i + 1` holds message `i + 1`; newer ids are newer mail.
async fn seed_100k(stack: &Stack) {
    let account = stack.account;
    let start = now();
    let in_window: Vec<(i64, String)> = stack
        .server()
        .store()
        .cache()
        .write(move |tx| {
            tx.execute_batch(&format!(
                "INSERT INTO mailbox (id, account_id, name, special_use, uidvalidity, sync_state) VALUES
                     (1, {account}, 'INBOX', NULL, 7, 'live'),
                     (2, {account}, 'Sent', '\\Sent', 8, 'live'),
                     (3, {account}, 'Archive', '\\Archive', 9, 'live');
                 INSERT INTO label (id, account_id, keyword, label_key, kind) VALUES
                     (1, {account}, 'Work', 'work', 'keyword'),
                     (2, {account}, 'Travel', 'travel', 'keyword'),
                     (3, {account}, 'Receipts', 'receipts', 'keyword');"
            ))?;
            let mut thread = tx.prepare(
                "INSERT INTO thread (id, account_id, subject_norm, last_date, msg_count, unread_count, starred)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            )?;
            let mut message = tx.prepare(
                "INSERT INTO message (id, account_id, email_id, thread_id, date, subject, snippet,
                     from_json, to_json, cc_json, has_attachment, text_blob)
                 VALUES (?1, ?2, ?3, ?1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let mut location = tx.prepare(
                "INSERT INTO location (mailbox_id, uid, message_id, flags, keywords) VALUES (?1, ?2, ?3, ?4, '')",
            )?;
            let mut thread_mailbox = tx.prepare(
                "INSERT INTO thread_mailbox (thread_id, mailbox_id, last_date) VALUES (?1, ?2, ?3)",
            )?;
            let mut label = tx.prepare("INSERT INTO message_label (message_id, label_id) VALUES (?1, ?2)")?;
            let mut texts = Vec::new();
            for i in 0..THREADS {
                let id = THREADS - i;
                let date = start - i * SPAN / THREADS;
                let subject = format!("{} {id}", SUBJECTS[(i % 10) as usize]);
                let mailbox = match i % 10 {
                    0..=5 => 1,
                    6 | 7 => 2,
                    _ => 3,
                };
                let unread = i % 7 == 0;
                let starred = i % 50 == 0;
                let flags = i64::from(!unread) | if starred { 2 } else { 0 };
                let name = if i % 97 == 0 { "Alice Smith" } else { "Contact" };
                let from = format!("[{{\"name\":\"{name}\",\"email\":\"contact{}@example.com\"}}]", i % 500);
                let cc = (i % 13 == 0).then_some("[{\"email\":\"contact7@example.com\"}]");
                let body = format!("{subject}: the deadline moved, see you at the meeting {}", i % 31);
                let email_id = format!("g:{id}");
                let key = BlobKey::for_message(
                    tagmail_core::AccountId(account),
                    &tagmail_core::EmailId::parse(&email_id)?,
                );
                let text = date >= start - WINDOW || starred;
                thread.execute(tagmail_store::rusqlite::params![
                    id, account, SUBJECTS[(i % 10) as usize], date, i64::from(unread), starred
                ])?;
                message.execute(tagmail_store::rusqlite::params![
                    id,
                    account,
                    email_id,
                    date,
                    subject,
                    &body[..body.len().min(60)],
                    from,
                    "[{\"email\":\"user1@tagmail.test\"}]",
                    cc,
                    i % 11 == 0,
                    text.then(|| key.as_str().to_owned()),
                ])?;
                location.execute(tagmail_store::rusqlite::params![mailbox, id, id, flags])?;
                thread_mailbox.execute(tagmail_store::rusqlite::params![id, mailbox, date])?;
                for (every, l) in [(5, 1), (17, 2), (101, 3)] {
                    if i % every == 0 {
                        label.execute(tagmail_store::rusqlite::params![id, l])?;
                    }
                }
                tagmail_store::fts::index(
                    tx,
                    MessageRowId(id),
                    &FtsDocument {
                        subject: &subject,
                        from: &format!("{name} contact{}@example.com", i % 500),
                        to: "user1@tagmail.test",
                        body: &body,
                    },
                )?;
                if text {
                    texts.push((id, body));
                }
            }
            Ok(texts)
        })
        .await
        .unwrap();
    let blobs = stack.server().store().blobs().clone();
    let key = move |id: i64| {
        BlobKey::for_message(
            tagmail_core::AccountId(account),
            &tagmail_core::EmailId::parse(&format!("g:{id}")).unwrap(),
        )
    };
    tokio::task::spawn_blocking(move || {
        for (id, body) in in_window {
            blobs.put_text(&key(id), &body).unwrap();
        }
        let mut w = blobs.raw_writer(key(OLD_RAW), DEFAULT_RAW_CAP).unwrap();
        w.write_chunk(fixture::raw("Old", "from the archive").as_bytes())
            .unwrap();
        w.finish().unwrap();
    })
    .await
    .unwrap();
}

async fn server_ids(stack: &Stack, path: &str) -> Vec<i64> {
    let resp = stack.owner.get(stack.url(path)).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    tagmail_api_core::id_index::decode(&resp.bytes().await.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_100k_replica_matches_the_server() {
    let stack = Stack::empty().await;
    seed_100k(&stack).await;
    let replica = stack.replica().await;
    let router = Router::new(replica.clone());
    let local = replica.store().unwrap();
    let server = stack.server().store();

    let mut total = 0;
    for view in [
        View::Inbox,
        View::Mailbox(MailboxRowId(2)),
        View::Mailbox(MailboxRowId(3)),
    ] {
        let theirs = server.view_index(view.clone()).await.unwrap().ids;
        let ours = local.view_index(view).await.unwrap().ids;
        assert_eq!(ours, theirs);
        total += ours.len();
    }
    assert_eq!(total, THREADS as usize);
    for id in [1, 99_000, THREADS] {
        let theirs = server.threads(vec![ThreadId(id)]).await.unwrap().rows;
        let ours = local.threads(vec![ThreadId(id)]).await.unwrap().rows;
        assert_eq!(ours, theirs, "thread {id}");
    }

    let (mut answered, mut out_of_window) = (0, false);
    for q in QUERIES {
        let path = search_path(q);
        let resp = get(&router, &path).await;
        assert!(resp.header(PARTIAL_HEADER).is_none(), "{q}");
        let hits = ids(&resp);
        assert_eq!(hits, server_ids(&stack, &path).await, "{q}");
        answered += usize::from(!hits.is_empty());
        out_of_window |= hits.iter().any(|&id| id < OUT_OF_WINDOW_BELOW);
    }
    assert!(answered >= 20, "only {answered} queries had hits");
    assert!(out_of_window);

    let open = get(&router, &format!("/api/messages/{OLD_RAW}/render")).await;
    assert_eq!(open.status, 200, "{}", String::from_utf8_lossy(&open.body));
    assert!(String::from_utf8_lossy(&open.body).contains("from the archive"));

    #[cfg(feature = "calendar")]
    calendar_parity(&stack, &replica).await;
}

/// 2 calendars × 500 events: the replica, which expands them itself,
/// lists the server's instances for a past, a current and a future range.
#[cfg(feature = "calendar")]
async fn calendar_parity(stack: &fixture::Stack, replica: &tagmail_replica::Replica) {
    use tagmail_api_core::calendar_dto::CalendarTargetDto;

    const EVENTS: usize = 500;
    const CALENDARS: [&str; 2] = ["/cal/work/", "/cal/home/"];
    let server = stack.server().store();
    let today = now();
    calendar_seed::seed(server, stack.account, &CALENDARS, EVENTS, today).await;
    replica.gate().await.unwrap();
    assert!(replica.calendar());
    let local = replica.store().unwrap();
    let work = vec![CalendarTargetDto {
        account_id: stack.account,
        calendar_href: CALENDARS[0].into(),
    }];
    let window = tagmail_calendar::Window::around_unix(today);
    for (range, calendars) in [
        (window.start_ts()..today, Vec::new()),
        (today..window.end_ts(), work),
        (window.start_ts()..window.end_ts(), Vec::new()),
    ] {
        let theirs = server
            .events_in(range.clone(), calendars.clone())
            .await
            .unwrap();
        let ours = local.events_in(range.clone(), calendars).await.unwrap();
        assert!(!theirs.is_empty(), "{range:?}");
        assert_eq!(ours, theirs, "{range:?}");
    }
}
