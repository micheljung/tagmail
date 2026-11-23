//! Transcript tests: [`TokioSession`] against scripted servers, including
//! the Exchange/Yahoo/Gmail quirk corpus from the M0 spike.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use crate::testutil::{Step, cmd, config, login, send, serve, session};
use crate::*;

const QRESYNC_CAPS: &str = "IDLE ENABLE CONDSTORE QRESYNC UIDPLUS MOVE SPECIAL-USE";

fn select_ok(extra: &[&str]) -> Vec<String> {
    let mut lines: Vec<String> = [
        "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $label_Work)",
        "* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $label_Work \\*)] ok",
        "* 3 EXISTS",
        "* OK [UIDVALIDITY 1700000000] ok",
        "* OK [UIDNEXT 12] ok",
        "* OK [HIGHESTMODSEQ 40] ok",
    ]
    .map(String::from)
    .to_vec();
    lines.extend(extra.iter().map(|l| (*l).to_owned()));
    lines.push("{tag} OK [READ-WRITE] selected".into());
    lines
}

fn with(mut steps: Vec<Step>, more: Vec<Step>) -> Vec<Step> {
    steps.extend(more);
    steps
}

fn uids(r: &[(u32, u32)]) -> UidSet {
    let mut s = UidSet::new();
    for &(a, b) in r {
        s.insert_range(a, b);
    }
    s
}

#[tokio::test]
async fn login_enables_qresync_and_lists_special_use() {
    let steps = with(
        login(QRESYNC_CAPS),
        vec![cmd(
            "LIST \"\" *",
            &[
                "* LIST (\\HasNoChildren) \"/\" INBOX",
                "* LIST (\\HasNoChildren \\Sent) \"/\" Sent",
                "* LIST (\\Noselect \\HasChildren) \"/\" Folders",
                "* LIST (\\HasNoChildren \\Archive) \"/\" \"Old Stuff\"",
                "{tag} OK listed",
            ],
        )],
    );
    let (mut s, server) = session(steps, &config()).await;
    assert!(s.qresync_enabled());
    assert!(s.capabilities().idle() && s.capabilities().r#move());
    let boxes = s.list().await.unwrap();
    server.await.unwrap();
    assert_eq!(boxes.len(), 4);
    assert_eq!(boxes[1].special_use, Some(SpecialUse::Sent));
    assert_eq!(boxes[1].delimiter, Some('/'));
    assert!(!boxes[2].selectable);
    assert_eq!(boxes[3].name, "Old Stuff");
    assert_eq!(boxes[3].special_use, Some(SpecialUse::Archive));
}

#[tokio::test]
async fn authenticate_plain_with_and_without_sasl_ir() {
    for caps in ["AUTH=PLAIN SASL-IR", "AUTH=PLAIN"] {
        let (s, server) = session(login(caps), &config()).await;
        assert!(!s.qresync_enabled());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn auth_failures_are_classified() {
    for (reply, auth) in [
        ("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials", true),
        ("{tag} NO Invalid credentials", true),
        ("{tag} NO [UNAVAILABLE] Try later", false),
    ] {
        let (io, _server) = serve(vec![
            send(&["* OK ready"]),
            cmd("CAPABILITY", &["* CAPABILITY IMAP4rev1", "{tag} OK done"]),
            cmd("LOGIN", &[reply]),
        ]);
        let err = TokioSession::start(Box::new(io), false, &config())
            .await
            .unwrap_err();
        assert_eq!(err.is_auth(), auth, "{err}");
        assert_eq!(matches!(err, Error::Unavailable(_)), !auth, "{err}");
    }
}

#[tokio::test]
async fn bye_greeting_is_unavailable() {
    let (io, _server) = serve(vec![send(&["* BYE [UNAVAILABLE] Server shutting down"])]);
    let err = TokioSession::start(Box::new(io), false, &config())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err}");
}

#[tokio::test]
async fn refuses_plaintext_login_when_disabled() {
    let (io, _server) = serve(vec![
        send(&["* OK ready"]),
        cmd(
            "CAPABILITY",
            &["* CAPABILITY IMAP4rev1 LOGINDISABLED", "{tag} OK done"],
        ),
    ]);
    let err = TokioSession::start(Box::new(io), false, &config())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
}

#[tokio::test]
async fn qresync_select_reports_vanished_earlier_and_changes() {
    let steps = with(
        login(QRESYNC_CAPS),
        vec![cmd(
            "SELECT INBOX (QRESYNC (1700000000 30 1:11))",
            &select_ok(&[
                "* VANISHED (EARLIER) 2:3,7",
                "* 1 FETCH (UID 1 FLAGS (\\Seen $label_Work) MODSEQ (39))",
            ]),
        )],
    );
    let (mut s, server) = session(steps, &config()).await;
    let sel = s
        .select(
            "INBOX",
            SelectOptions {
                read_only: false,
                qresync: Some(QresyncParams {
                    uid_validity: UidValidity(1_700_000_000),
                    modseq: ModSeq(30),
                    known_uids: Some(uids(&[(1, 11)])),
                }),
            },
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(sel.uid_validity, UidValidity(1_700_000_000));
    assert_eq!(sel.uid_next, Some(Uid(12)));
    assert_eq!(sel.highest_modseq, Some(ModSeq(40)));
    assert_eq!(sel.exists, 3);
    assert!(sel.keyword_capable);
    assert_eq!(sel.vanished.to_string(), "2:3,7");
    assert_eq!(sel.changed.len(), 1);
    assert_eq!(sel.changed[0].modseq, Some(ModSeq(39)));
    assert!(s.take_events().is_empty());
    assert_eq!(s.selected(), Some("INBOX"));
}

#[tokio::test]
async fn nomodseq_mailbox_skips_modseq() {
    let steps = with(
        login("ENABLE CONDSTORE"),
        vec![
            cmd(
                "EXAMINE Archive (CONDSTORE)",
                &[
                    "* 1 EXISTS",
                    "* OK [UIDVALIDITY 5] ok",
                    "* OK [NOMODSEQ] no modseqs",
                    "* OK [PERMANENTFLAGS ()] none",
                    "{tag} OK [READ-ONLY] examined",
                ],
            ),
            cmd(
                "UID FETCH 1 (UID FLAGS)",
                &["* 1 FETCH (UID 1 FLAGS (\\Seen))", "{tag} OK done"],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    let sel = s
        .select(
            "Archive",
            SelectOptions {
                read_only: true,
                qresync: None,
            },
        )
        .await
        .unwrap();
    assert!(sel.nomodseq && sel.read_only && !sel.keyword_capable);
    assert_eq!(sel.highest_modseq, None);
    let err = s
        .fetch_changed_since(&uids(&[(1, 1)]), ModSeq(1), &FetchSpec::flags())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    let got = s
        .fetch(&uids(&[(1, 1)]), &FetchSpec::flags())
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(got[0].flags, Some(vec![Flag::Seen]));
}

#[tokio::test]
async fn capability_override_hides_condstore() {
    let mut cfg = config();
    cfg.capability_override.remove = vec!["CONDSTORE".into()];
    let steps = vec![
        send(&["* OK ready"]),
        cmd(
            "CAPABILITY",
            &[
                "* CAPABILITY IMAP4rev1 ENABLE CONDSTORE QRESYNC",
                "{tag} OK done",
            ],
        ),
        cmd("LOGIN", &["{tag} OK"]),
        cmd(
            "CAPABILITY",
            &[
                "* CAPABILITY IMAP4rev1 ENABLE CONDSTORE QRESYNC",
                "{tag} OK done",
            ],
        ),
        cmd("SELECT INBOX", &select_ok(&[])),
        cmd("UID FETCH 1:2 (UID FLAGS)", &["{tag} OK done"]),
    ];
    let (mut s, server) = session(steps, &cfg).await;
    assert!(!s.qresync_enabled() && !s.capabilities().condstore());
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    s.fetch(&uids(&[(1, 2)]), &FetchSpec::flags())
        .await
        .unwrap();
    let seen = server.await.unwrap();
    assert_eq!(seen.last().unwrap(), "UID FETCH 1:2 (UID FLAGS)");
    assert!(seen.iter().any(|c| c == "SELECT INBOX"), "{seen:?}");
}

#[tokio::test]
async fn objectid_identity_uses_raw_fetch() {
    let steps = with(
        login("ENABLE CONDSTORE OBJECTID"),
        vec![
            cmd("SELECT INBOX (CONDSTORE)", &select_ok(&[])),
            cmd(
                "UID FETCH 1:2 (UID FLAGS MODSEQ)",
                &[
                    "* 1 FETCH (UID 1 FLAGS () MODSEQ (5))",
                    "* 2 FETCH (UID 2 FLAGS (\\Seen) MODSEQ (6))",
                    "{tag} OK done",
                ],
            ),
            cmd(
                "UID FETCH 1:2 (UID EMAILID THREADID)",
                &[
                    "* 1 FETCH (UID 1 EMAILID (Ma1) THREADID (Tz))",
                    "* 2 FETCH (UID 2 EMAILID (Mb2) THREADID NIL)",
                    "{tag} OK done",
                ],
            ),
            cmd("UID SEARCH EMAILID Mb2", &["* SEARCH 2", "{tag} OK done"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let mut spec = FetchSpec::flags();
    spec.identity = true;
    let got = s.fetch(&uids(&[(1, 2)]), &spec).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].email_id.as_deref(), Some("Ma1"));
    assert_eq!(got[0].thread_id.as_deref(), Some("Tz"));
    assert_eq!(got[1].email_id.as_deref(), Some("Mb2"));
    assert_eq!(got[1].thread_id, None);
    assert!(got.iter().all(|m| !m.degraded));
    let found = s
        .uid_search(SearchCriteria::EmailId("Mb2".into()))
        .await
        .unwrap();
    assert_eq!(found.to_string(), "2");
    assert!(
        s.uid_search(SearchCriteria::EmailId("x y".into()))
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn gmail_msgid_identity() {
    let steps = with(
        login("X-GM-EXT-1"),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            cmd(
                "UID FETCH 4 (UID FLAGS)",
                &["* 1 FETCH (UID 4 FLAGS (\\Seen))", "{tag} OK"],
            ),
            cmd(
                "UID FETCH 4 (UID X-GM-MSGID X-GM-THRID)",
                &[
                    "* 1 FETCH (X-GM-THRID 1278455344230334865 X-GM-MSGID 1278455344230334866 UID 4)",
                    "{tag} OK",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let mut spec = FetchSpec::flags();
    spec.identity = true;
    let got = s.fetch(&uids(&[(4, 4)]), &spec).await.unwrap();
    server.await.unwrap();
    assert_eq!(got[0].gmail_msgid, Some(1_278_455_344_230_334_866));
    assert_eq!(got[0].gmail_thrid, Some(1_278_455_344_230_334_865));
}

/// Quirk lines from the M0 parser corpus: the session survives each and
/// recovers UID and FLAGS where the codec gives up.
#[tokio::test]
async fn quirk_corpus_degrades_instead_of_dropping() {
    let steps = with(
        login(""),
        vec![
            cmd(
                "SELECT INBOX",
                &select_ok(&["* OK [X-WEIRD-CODE foo bar] hello"]),
            ),
            cmd(
                "UID FETCH 1:6 (UID FLAGS RFC822.SIZE BODYSTRUCTURE)",
                &[
                    // Exchange: NIL Content-Transfer-Encoding.
                    "* 1 FETCH (UID 1 FLAGS (\\Seen) RFC822.SIZE 100 BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"us-ascii\") NIL NIL NIL 100 5 NIL NIL NIL NIL))",
                    // Yahoo: message/rfc822 without envelope.
                    "* 2 FETCH (UID 2 FLAGS () BODYSTRUCTURE (\"message\" \"rfc822\" NIL NIL NIL \"7bit\" 300 NIL NIL NIL NIL))",
                    // Negative size.
                    "* 3 FETCH (RFC822.SIZE -1 UID 3 FLAGS (\\Flagged))",
                    "* ESEARCH (TAG \"X\") UID ALL 1:3",
                    "* 4 FETCH (UID 4 FLAGS (\\Seen NonJunk $Forwarded) RFC822.SIZE 10 BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 10 1 NIL NIL NIL NIL))",
                    "* 5 FETCH (UID 5 FLAGS (\\Seen [Gmail]Foo) PREVIEW \"hi\")",
                    "* 6 FETCH (UID 6 FLAGS (\\Seen)\n",
                    "{tag} OK done",
                ],
            ),
            cmd("NOOP", &["{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let spec = FetchSpec {
        flags: true,
        size: true,
        body_structure: true,
        ..FetchSpec::default()
    };
    // UID 6 lacks its closing parenthesis and is dropped.
    let mut got = s.fetch(&uids(&[(1, 6)]), &spec).await.unwrap();
    got.sort_by_key(|m| m.uid);
    let by_uid = |u| got.iter().find(|m| m.uid == Some(Uid(u))).unwrap();
    assert_eq!(got.len(), 5, "{got:?}");
    // imap-codec's quirk features cover the Exchange and Yahoo structures.
    assert!(!by_uid(1).degraded && by_uid(1).flags == Some(vec![Flag::Seen]));
    assert_eq!(by_uid(1).body.as_ref().unwrap().parts[0].encoding, "");
    assert!(!by_uid(2).degraded && by_uid(2).body.is_some());
    assert!(by_uid(3).degraded && by_uid(3).flags == Some(vec![Flag::Flagged]));
    assert!(!by_uid(4).degraded && by_uid(4).body.is_some());
    assert!(by_uid(5).degraded);
    s.noop().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn changed_since_with_vanished() {
    let steps = with(
        login(QRESYNC_CAPS),
        vec![
            cmd("SELECT INBOX (CONDSTORE)", &select_ok(&[])),
            cmd(
                "UID FETCH 1:11 (UID FLAGS MODSEQ) (CHANGEDSINCE 30 VANISHED)",
                &[
                    "* VANISHED (EARLIER) 4:5",
                    "* 2 FETCH (UID 6 FLAGS (\\Seen) MODSEQ (35))",
                    "{tag} OK done",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let changes = s
        .fetch_changed_since(&uids(&[(1, 11)]), ModSeq(30), &FetchSpec::flags())
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(changes.vanished.to_string(), "4:5");
    assert_eq!(changes.messages[0].uid, Some(Uid(6)));
}

#[tokio::test]
async fn uid_search_all_is_chunked_and_ignores_esearch() {
    let steps = with(
        login(""),
        vec![
            cmd(
                "SELECT INBOX",
                &[
                    "* 3 EXISTS",
                    "* OK [UIDVALIDITY 1] ok",
                    "* OK [UIDNEXT 150001] ok",
                    "{tag} OK selected",
                ],
            ),
            cmd("UID SEARCH UID 1:100000", &["* SEARCH 5 99999", "{tag} OK"]),
            cmd(
                "UID SEARCH UID 100001:*",
                &[
                    "* ESEARCH (TAG \"x\") UID ALL 1",
                    "* SEARCH 150000",
                    "{tag} OK",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let all = s.uid_search(SearchCriteria::All).await.unwrap();
    server.await.unwrap();
    assert_eq!(all.to_string(), "5,99999,150000");
}

#[tokio::test]
async fn chunked_body_fetch_streams_and_caps() {
    let mut cfg = config();
    cfg.body_chunk = 4;
    let chunk = |off: u32, data: &str| -> Step {
        let expect: &'static str =
            Box::leak(format!("UID FETCH 9 (UID BODY.PEEK[1.2]<{off}.4>)").into_boxed_str());
        let line = format!(
            "* 1 FETCH (UID 9 BODY[1.2]<{off}> {{{}}}\r\n{data})",
            data.len()
        );
        Step::Cmd(expect, vec![line, "{tag} OK".into()])
    };
    let steps = with(
        login(""),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            chunk(0, "abcd"),
            chunk(4, "efgh"),
            chunk(8, "ij"),
            chunk(0, "abcd"),
            chunk(4, "efgh"),
            cmd(
                "UID FETCH 10 (UID BODY.PEEK[]<0.4>)",
                &["* 1 FETCH (UID 10 FLAGS (\\Seen))", "{tag} OK"],
            ),
        ],
    );
    let (mut s, server) = session(steps, &cfg).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let mut sink = Vec::new();
    let got = s
        .fetch_body(Uid(9), "1.2", &mut sink, 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        (got.bytes, got.truncated, sink.as_slice()),
        (10, false, &b"abcdefghij"[..])
    );
    let mut sink = Vec::new();
    let got = s.fetch_body(Uid(9), "1.2", &mut sink, 6).await.unwrap();
    assert_eq!(
        (got.bytes, got.truncated, sink.as_slice()),
        (6, true, &b"abcdef"[..])
    );
    let err = s
        .fetch_body(Uid(10), "", &mut Vec::new(), 100)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound), "{err}");
    server.await.unwrap();
    let events = s.take_events();
    assert!(matches!(events.as_slice(), [MailboxEvent::Fetch(m)] if m.uid == Some(Uid(10))));
}

#[tokio::test]
async fn oversized_response_fails_but_session_survives() {
    let mut cfg = config();
    cfg.max_response_size = 64;
    let big = "x".repeat(200);
    let line = format!("* 1 FETCH (UID 9 BODY[] {{200}}\r\n{big})");
    let steps = with(
        login(""),
        vec![
            cmd("SELECT INBOX", &["* OK [UIDVALIDITY 1] ok", "{tag} OK"]),
            cmd(
                "UID FETCH 9 (UID BODY.PEEK[]<0.1048576>)",
                &[&line, "{tag} OK"],
            ),
            cmd("NOOP", &["{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &cfg).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let err = s
        .fetch_body(Uid(9), "", &mut Vec::new(), u64::MAX)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::ResponseTooLong { limit: 64 }), "{err}");
    s.noop().await.unwrap();
    assert!(s.is_usable());
    server.await.unwrap();
}

#[tokio::test]
async fn idle_returns_on_change_wake_and_renewal() {
    let mut cfg = config();
    cfg.idle_renew = Duration::from_millis(200);
    let steps = with(
        login(QRESYNC_CAPS),
        vec![
            cmd("SELECT INBOX (CONDSTORE)", &select_ok(&[])),
            cmd("IDLE", &["+ idling"]),
            Step::Sleep(Duration::from_millis(20)),
            send(&["* OK still here", "* 4 EXISTS"]),
            cmd(
                "DONE",
                &[
                    "* 1 FETCH (UID 1 FLAGS (\\Seen) MODSEQ (41))",
                    "{tag} OK idle done",
                ],
            ),
            cmd("IDLE", &["+ idling"]),
            cmd("DONE", &["{tag} OK idle done"]),
            cmd("IDLE", &["+ idling"]),
            cmd("DONE", &["{tag} OK idle done"]),
        ],
    );
    let (mut s, server) = session(steps, &cfg).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let wake = Notify::new();
    let r = s.idle(&wake).await.unwrap();
    assert_eq!(r.stop, IdleStop::Changed);
    assert_eq!(r.events.len(), 2, "{:?}", r.events);
    assert_eq!(r.events[0], MailboxEvent::Exists(4));

    let r = s.idle(&wake).await.unwrap();
    assert_eq!((r.stop, r.events.len()), (IdleStop::Renew, 0));

    let notify = Arc::new(Notify::new());
    let n2 = notify.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        n2.notify_one();
    });
    let r = s.idle(&notify).await.unwrap();
    assert_eq!(r.stop, IdleStop::Woken);
    server.await.unwrap();
}

#[tokio::test]
async fn idle_rejected_and_unsupported() {
    let steps = with(
        login("IDLE"),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            cmd("IDLE", &["{tag} NO not now"]),
            cmd("NOOP", &["{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    let wake = Notify::new();
    assert!(matches!(s.idle(&wake).await, Err(Error::NoMailboxSelected)));
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    assert!(matches!(s.idle(&wake).await, Err(Error::No { .. })));
    s.noop().await.unwrap();
    server.await.unwrap();

    let (mut s, _server) = session(
        with(login(""), vec![cmd("SELECT INBOX", &select_ok(&[]))]),
        &config(),
    )
    .await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    assert!(matches!(
        s.idle(&wake).await,
        Err(Error::Unsupported("IDLE"))
    ));
}

#[tokio::test]
async fn store_keywords_with_unchangedsince() {
    let steps = with(
        login(QRESYNC_CAPS),
        vec![
            cmd("SELECT INBOX (CONDSTORE)", &select_ok(&[])),
            cmd(
                "UID STORE 1:3 (UNCHANGEDSINCE 40) +FLAGS.SILENT ($label_Work \\Seen)",
                &[
                    "* 1 FETCH (UID 1 MODSEQ (41))",
                    "* 3 FETCH (UID 3 MODSEQ (42))",
                    "{tag} OK [MODIFIED 2] conditional store failed",
                ],
            ),
            cmd("UID STORE 5 -FLAGS.SILENT ($label_Work)", &["{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let r = s
        .store_flags(
            &uids(&[(1, 3)]),
            StoreOp::Add,
            &[Flag::Keyword("$label_Work".into()), Flag::Seen],
            Some(ModSeq(40)),
        )
        .await
        .unwrap();
    assert_eq!(r.modified.to_string(), "2");
    assert_eq!(r.updates.len(), 2);
    s.store_flags(
        &uids(&[(5, 5)]),
        StoreOp::Remove,
        &[Flag::Keyword("$label_Work".into())],
        None,
    )
    .await
    .unwrap();
    assert!(
        s.store_flags(
            &uids(&[(5, 5)]),
            StoreOp::Add,
            &[Flag::Keyword("bad word".into())],
            None
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn move_uses_move_or_copy_expunge() {
    let steps = with(
        login("MOVE UIDPLUS"),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            cmd(
                "UID MOVE 2:3 Archive",
                &[
                    "* OK [COPYUID 77 2:3 10:11] moved",
                    "* 2 EXPUNGE",
                    "* 2 EXPUNGE",
                    "{tag} OK done",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let r = s.move_messages(&uids(&[(2, 3)]), "Archive").await.unwrap();
    server.await.unwrap();
    let cu = r.copy_uid.unwrap();
    assert_eq!(cu.uid_validity, UidValidity(77));
    assert_eq!(cu.source, vec![Uid(2), Uid(3)]);
    assert_eq!(cu.destination, vec![Uid(10), Uid(11)]);
    assert_eq!(
        s.take_events(),
        vec![MailboxEvent::Expunge(2), MailboxEvent::Expunge(2)]
    );

    let steps = with(
        login("UIDPLUS"),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            cmd("UID COPY 2 Trash", &["{tag} OK [COPYUID 9 2 5] copied"]),
            cmd("UID STORE 2 +FLAGS.SILENT (\\Deleted)", &["{tag} OK"]),
            cmd("UID EXPUNGE 2", &["* 2 EXPUNGE", "{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let r = s.move_messages(&uids(&[(2, 2)]), "Trash").await.unwrap();
    server.await.unwrap();
    assert_eq!(r.copy_uid.unwrap().destination, vec![Uid(5)]);

    let steps = with(
        login(""),
        vec![
            cmd("SELECT INBOX", &select_ok(&[])),
            cmd("UID SEARCH DELETED", &["* SEARCH 2 7", "{tag} OK"]),
            cmd("UID SEARCH DELETED", &["* SEARCH 2", "{tag} OK"]),
            cmd("UID COPY 2 Trash", &["{tag} OK"]),
            cmd("UID STORE 2 +FLAGS.SILENT (\\Deleted)", &["{tag} OK"]),
            cmd("EXPUNGE", &["* 2 EXPUNGE", "{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    let err = s
        .move_messages(&uids(&[(2, 2)]), "Trash")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    let r = s.move_messages(&uids(&[(2, 2)]), "Trash").await.unwrap();
    assert_eq!(r.copy_uid, None);
    server.await.unwrap();
}

#[tokio::test]
async fn expunge_uids_only_touches_the_given_messages() {
    let steps = with(
        login("UIDPLUS"),
        vec![
            cmd("SELECT Drafts", &select_ok(&[])),
            cmd("UID STORE 4 +FLAGS.SILENT (\\Deleted)", &["{tag} OK"]),
            cmd("UID EXPUNGE 4", &["* 3 EXPUNGE", "{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("Drafts", SelectOptions::default()).await.unwrap();
    s.expunge_uids(&uids(&[(4, 4)])).await.unwrap();
    s.expunge_uids(&UidSet::new()).await.unwrap();
    server.await.unwrap();
    assert_eq!(s.take_events(), vec![MailboxEvent::Expunge(3)]);

    let steps = with(
        login(""),
        vec![
            cmd("SELECT Drafts", &select_ok(&[])),
            cmd("UID SEARCH DELETED", &["* SEARCH 4 9", "{tag} OK"]),
            cmd("UID SEARCH DELETED", &["* SEARCH", "{tag} OK"]),
            cmd("UID STORE 4 +FLAGS.SILENT (\\Deleted)", &["{tag} OK"]),
            cmd("EXPUNGE", &["* 3 EXPUNGE", "{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("Drafts", SelectOptions::default()).await.unwrap();
    let err = s.expunge_uids(&uids(&[(4, 4)])).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    s.expunge_uids(&uids(&[(4, 4)])).await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn append_stream_sends_the_literal_in_chunks() {
    let body: Vec<u8> = (0..300_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let steps = with(
        login("UIDPLUS"),
        vec![
            cmd(
                "APPEND \"Sent \\\"x\\\"\" (\\Seen) {300000}",
                &["{tag} OK [APPENDUID 9 12] appended"],
            ),
            cmd(
                "STATUS \"Sent \\\"x\\\"\" (MESSAGES UIDNEXT UIDVALIDITY UNSEEN)",
                &[
                    "* STATUS \"Sent \\\"x\\\"\" (MESSAGES 1 UIDNEXT 13 UIDVALIDITY 9 UNSEEN 0)",
                    "{tag} OK",
                ],
            ),
            cmd("APPEND \"Drafts\" () {3}", &["{tag} NO [OVERQUOTA] full"]),
            cmd("NOOP", &["{tag} OK"]),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    let r = s
        .append_stream(
            "Sent \"x\"",
            &[Flag::Seen],
            body.len() as u64,
            &mut body.as_slice(),
        )
        .await
        .unwrap();
    assert_eq!(
        (r.uid, r.uid_validity),
        (Some(Uid(12)), Some(UidValidity(9)))
    );
    let err = s
        .append_stream("Drafts", &[], 3, &mut &b"abc"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::No { .. }), "{err:?}");
    assert!(s.is_usable());
    s.noop().await.unwrap();
    let seen = server.await.unwrap();
    assert!(seen[seen.len() - 4].ends_with(std::str::from_utf8(&body).unwrap()));

    let err = s
        .append_stream("Drafts\u{e9}", &[], 1, &mut &b"x"[..])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err:?}");
}

#[tokio::test]
async fn append_trusts_status_uidvalidity() {
    let steps = with(
        login("UIDPLUS"),
        vec![
            cmd(
                "APPEND Drafts (\\Draft \\Seen) {10}",
                &["{tag} OK [APPENDUID 1700000001 4] appended"],
            ),
            cmd(
                "STATUS Drafts (MESSAGES UIDNEXT UIDVALIDITY UNSEEN)",
                &[
                    "* STATUS Drafts (MESSAGES 4 UIDNEXT 5 UIDVALIDITY 1700000000 UNSEEN 0)",
                    "{tag} OK",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    let r = s
        .append("Drafts", &[Flag::Draft, Flag::Seen], b"Subject: x")
        .await
        .unwrap();
    let seen = server.await.unwrap();
    assert!(seen[seen.len() - 2].ends_with("Subject: x"), "{seen:?}");
    assert_eq!(r.uid, Some(Uid(4)));
    assert_eq!(r.uid_validity, Some(UidValidity(1_700_000_000)));
}

#[tokio::test]
async fn unsolicited_changes_are_queued() {
    let steps = with(
        login(QRESYNC_CAPS),
        vec![
            cmd("SELECT INBOX (CONDSTORE)", &select_ok(&[])),
            cmd(
                "NOOP",
                &[
                    "* 5 EXISTS",
                    "* VANISHED 3",
                    "* 2 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ (50))",
                    "{tag} OK",
                ],
            ),
            cmd(
                "STATUS Sent (MESSAGES UIDNEXT UIDVALIDITY UNSEEN HIGHESTMODSEQ)",
                &[
                    "* STATUS Sent (MESSAGES 2 UIDNEXT 3 UIDVALIDITY 9 UNSEEN 1 HIGHESTMODSEQ 77)",
                    "{tag} OK",
                ],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("INBOX", SelectOptions::default()).await.unwrap();
    s.noop().await.unwrap();
    let st = s.status("Sent").await.unwrap();
    server.await.unwrap();
    assert_eq!(st.highest_modseq, Some(ModSeq(77)));
    assert_eq!(st.uid_validity, Some(UidValidity(9)));
    let events = s.take_events();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], MailboxEvent::Exists(5));
    assert!(
        matches!(&events[1], MailboxEvent::Vanished(v) if !v.earlier && v.uids.to_string() == "3")
    );
    assert!(matches!(&events[2], MailboxEvent::Fetch(m) if m.modseq == Some(ModSeq(50))));
}

#[tokio::test]
async fn closed_connection_marks_session_unusable() {
    let steps = with(login(""), vec![send(&["* BYE [UNAVAILABLE] going down"])]);
    let (mut s, server) = session(steps, &config()).await;
    server.await.unwrap();
    let err = s.noop().await.unwrap_err();
    assert!(
        matches!(&err, Error::Closed { bye: Some(b) } if b.contains("going down")),
        "{err}"
    );
    assert!(!s.is_usable());
    assert!(s.noop().await.is_err());
}

struct ScriptConnector {
    scripts: std::sync::Mutex<Vec<Vec<Step>>>,
    attempts: AtomicUsize,
}

impl Connector for ScriptConnector {
    type Session = TokioSession;

    async fn connect(&self) -> Result<TokioSession> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let steps = self.scripts.lock().unwrap().pop().expect("script left");
        let (io, _server) = serve(steps);
        TokioSession::start(Box::new(io), false, &config()).await
    }
}

fn connector(scripts: Vec<Vec<Step>>) -> ScriptConnector {
    ScriptConnector {
        scripts: std::sync::Mutex::new(scripts),
        attempts: AtomicUsize::new(0),
    }
}

#[tokio::test]
async fn pool_honors_budget_and_reuses_sessions() {
    let keep_open = || with(login(""), vec![Step::Sleep(Duration::from_secs(60))]);
    let pool = Pool::new(
        connector(vec![keep_open(), keep_open()]),
        PoolConfig {
            max_connections: 1,
            ..PoolConfig::default()
        },
    );
    let first = pool.acquire().await.unwrap();
    let p2 = pool.clone();
    let waiter = tokio::spawn(async move { p2.acquire().await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiter.is_finished(),
        "budget of 1 must block the second acquire"
    );
    drop(first);
    waiter.await.unwrap().unwrap();
    assert_eq!(pool.inner_attempts(), 1, "the session was reused");
}

#[tokio::test]
async fn pool_backs_off_after_auth_failure() {
    let fail = || {
        vec![
            send(&["* OK ready"]),
            cmd("CAPABILITY", &["* CAPABILITY IMAP4rev1", "{tag} OK"]),
            cmd("LOGIN", &["{tag} NO [AUTHENTICATIONFAILED] nope"]),
        ]
    };
    let pool = Pool::new(
        connector(vec![login(""), fail(), fail()]),
        PoolConfig {
            auth_backoff_base: Duration::from_millis(100),
            auth_backoff_max: Duration::from_millis(150),
            ..PoolConfig::default()
        },
    );
    assert!(pool.acquire().await.err().unwrap().is_auth());
    let err = pool.acquire().await.err().unwrap();
    assert!(matches!(err, Error::AuthBackoff { .. }), "{err}");
    assert_eq!(pool.inner_attempts(), 1);
    let (_, text) = pool.auth_backoff().unwrap();
    assert!(text.unwrap().contains("nope"));

    tokio::time::sleep(Duration::from_millis(110)).await;
    assert!(pool.acquire().await.err().unwrap().is_auth());
    let (wait, _) = pool.auth_backoff().unwrap();
    assert!(wait > Duration::from_millis(100) && wait <= Duration::from_millis(150));

    pool.reset_auth_backoff();
    pool.acquire().await.unwrap();
    assert_eq!(pool.inner_attempts(), 3);
}

impl Pool<ScriptConnector> {
    fn inner_attempts(&self) -> usize {
        self.connector().attempts.load(Ordering::SeqCst)
    }
}

mod tls {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::testutil::serve_on;

    const CA: &[u8] = include_bytes!("../tests/fixtures/ca.pem");
    const CERT: &[u8] = include_bytes!("../tests/fixtures/server.pem");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/server.key");

    fn acceptor() -> TlsAcceptor {
        let certs = CertificateDer::pem_slice_iter(CERT)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        TlsAcceptor::from(Arc::new(config))
    }

    fn tls_config(port: u16, security: Security, ca: Option<&[u8]>) -> ServerConfig {
        let mut cfg = ServerConfig::new("localhost", port, security, "user", Password::new("pass"));
        cfg.tls = Some(client_config(ca).unwrap());
        cfg
    }

    fn after_greeting() -> Vec<Step> {
        login("").into_iter().skip(1).collect()
    }

    #[tokio::test]
    async fn implicit_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor().accept(tcp).await.unwrap();
            serve_on(tls, login("")).await.unwrap()
        });
        let s = TokioSession::connect(&tls_config(port, Security::Tls, Some(CA)))
            .await
            .unwrap();
        assert!(s.is_usable());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn untrusted_certificate_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = acceptor().accept(tcp).await;
        });
        let err = TokioSession::connect(&tls_config(port, Security::Tls, None))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Io(_) | Error::Tls(_)), "{err}");
    }

    async fn starttls_server(listener: TcpListener, inject: bool) -> Option<Vec<String>> {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(tcp);
        r.get_mut()
            .write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] ready\r\n")
            .await
            .unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "S0 STARTTLS\r\n");
        let reply: &[u8] = if inject {
            b"S0 OK begin TLS\r\n* OK injected\r\n"
        } else {
            b"S0 OK begin TLS\r\n"
        };
        r.get_mut().write_all(reply).await.unwrap();
        if inject {
            return None;
        }
        let tls = acceptor().accept(r.into_inner()).await.unwrap();
        Some(serve_on(tls, after_greeting()).await.unwrap())
    }

    #[tokio::test]
    async fn starttls_upgrades_before_login() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(starttls_server(listener, false));
        let s = TokioSession::connect(&tls_config(port, Security::StartTls, Some(CA)))
            .await
            .unwrap();
        assert!(s.is_usable());
        let seen = server.await.unwrap().unwrap();
        assert!(seen.iter().any(|c| c.starts_with("LOGIN")), "{seen:?}");
    }

    #[tokio::test]
    async fn starttls_rejects_injected_plaintext() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(starttls_server(listener, true));
        let err = TokioSession::connect(&tls_config(port, Security::StartTls, Some(CA)))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err}");
    }
}

#[tokio::test]
async fn copy_keeps_the_source_and_reports_copyuid() {
    let steps = with(
        login("UIDPLUS"),
        vec![
            cmd("SELECT Archive", &select_ok(&[])),
            cmd(
                "UID COPY 7 INBOX",
                &["{tag} OK [COPYUID 1700000000 7 31] copied"],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    s.select("Archive", SelectOptions::default()).await.unwrap();
    let r = s.copy_messages(&uids(&[(7, 7)]), "INBOX").await.unwrap();
    s.copy_messages(&UidSet::new(), "INBOX").await.unwrap();
    server.await.unwrap();
    let copy = r.copy_uid.unwrap();
    assert_eq!(copy.destination, vec![Uid(31)]);
}

#[tokio::test]
async fn server_metadata_round_trip() {
    let steps = with(
        login("METADATA"),
        vec![
            cmd(
                "GETMETADATA \"\" /private/vendor/tagmail/labels",
                &[
                    "* METADATA \"\" (/private/vendor/tagmail/labels {7}",
                    "{\"a\":1})",
                    "{tag} OK done",
                ],
            ),
            cmd(
                "GETMETADATA \"\" /private/vendor/tagmail/labels",
                &[
                    "* METADATA \"\" (/private/vendor/tagmail/labels NIL)",
                    "{tag} OK done",
                ],
            ),
            cmd(
                "SETMETADATA \"\" (/private/vendor/tagmail/labels \"x\")",
                &["{tag} OK set"],
            ),
            cmd(
                "SETMETADATA \"\" (/private/vendor/tagmail/labels NIL)",
                &["{tag} OK set"],
            ),
        ],
    );
    let (mut s, server) = session(steps, &config()).await;
    let entry = "/private/vendor/tagmail/labels";
    assert_eq!(
        s.get_metadata(entry).await.unwrap().as_deref(),
        Some(&b"{\"a\":1}"[..])
    );
    assert_eq!(s.get_metadata(entry).await.unwrap(), None);
    s.set_metadata(entry, Some(b"x")).await.unwrap();
    s.set_metadata(entry, None).await.unwrap();
    server.await.unwrap();

    let (mut s, server) = session(login(""), &config()).await;
    assert!(matches!(
        s.get_metadata(entry).await,
        Err(Error::Unsupported("METADATA"))
    ));
    server.await.unwrap();
}
