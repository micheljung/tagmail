use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tagmail_api_core::dto::{
    ChangeDto, ChangeOpDto, EntityDto, LabelDto, MailboxDto, SpecialUseDto, ThreadRowDto,
};
use tagmail_api_core::id_index;
use tagmail_api_core::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelPatchDto, NewLabelDto, OpDto, OpKindDto, OpStatusDto,
};
use tagmail_api_core::signing::UrlKeys;
use tagmail_api_core::{
    AccountRegistry, AccountSummary, Api, BoxFuture, ChangeFeed, ChangePage, Event, EventSession,
    FetchRawError, MessageRecord, Method, NewAccount, OpenError, Request, Response, RuntimeStatus,
    Store, StoreResult, ThreadDetail, ThreadRows, View, ViewIndex,
};
use tagmail_api_core::{Submitted, WriteError, WriteResult, WriteService};
use tagmail_core::query::parse;
use tagmail_core::{
    AccountId, CacheEpoch, ChangeCursor, LabelKey, MailboxRowId, MessageRowId, ThreadId,
};

const EPOCH: CacheEpoch = CacheEpoch(0xabc);

/// In-memory store: a fixed set of threads per view and an append-only log.
#[derive(Default)]
struct MemStore {
    inbox: Vec<i64>,
    mailbox: Vec<(i64, Vec<i64>)>,
    rows: Vec<ThreadRowDto>,
    log: Mutex<Vec<ChangeDto>>,
    min_seq: i64,
    accounts: Mutex<Vec<(AccountSummary, NewAccount)>>,
    /// Label and search views answer with `inbox` and are recorded here.
    queried: Mutex<Vec<View>>,
}

impl MemStore {
    fn cursor(&self) -> ChangeCursor {
        let log = self.log.lock().unwrap();
        ChangeCursor {
            epoch: EPOCH,
            seq: log.last().map_or(self.min_seq, |c| c.seq),
        }
    }

    fn append(&self, id: i64) -> ChangeCursor {
        let mut log = self.log.lock().unwrap();
        let seq = log.last().map_or(self.min_seq, |c| c.seq) + 1;
        log.push(ChangeDto {
            seq,
            entity: EntityDto::Thread,
            id,
            op: ChangeOpDto::Upsert,
            payload: Some(json!({ "lastDate": 1 })),
        });
        ChangeCursor { epoch: EPOCH, seq }
    }
}

impl Store for MemStore {
    async fn head(&self) -> StoreResult<ChangeCursor> {
        Ok(self.cursor())
    }

    async fn view_index(&self, view: View) -> StoreResult<ViewIndex> {
        let ids = match view {
            View::Inbox => self.inbox.clone(),
            View::Label(_) | View::Search(_) | View::Account(..) => {
                self.queried.lock().unwrap().push(view);
                self.inbox.clone()
            }
            View::Mailbox(MailboxRowId(m)) => self
                .mailbox
                .iter()
                .find(|(id, _)| *id == m)
                .map(|(_, ids)| ids.clone())
                .unwrap_or_default(),
        };
        Ok(ViewIndex {
            cursor: self.cursor(),
            ids: ids.into_iter().map(ThreadId).collect(),
        })
    }

    async fn mailboxes(&self) -> StoreResult<Vec<MailboxDto>> {
        Ok(vec![MailboxDto {
            id: 1,
            account_id: 1,
            name: "Entwürfe".to_owned(),
            delimiter: Some("/".to_owned()),
            special_use: Some(SpecialUseDto::Drafts),
        }])
    }

    async fn labels(&self) -> StoreResult<Vec<LabelDto>> {
        Ok(vec![LabelDto {
            key: "work".to_owned(),
            name: "Work".to_owned(),
            color: None,
            account_ids: vec![1],
        }])
    }

    async fn threads(&self, ids: Vec<ThreadId>) -> StoreResult<ThreadRows> {
        let rows = ids
            .iter()
            .filter_map(|id| self.rows.iter().find(|r| r.id == id.0).cloned())
            .collect();
        Ok(ThreadRows {
            cursor: self.cursor(),
            rows,
        })
    }

    async fn changes_since(&self, since: ChangeCursor, limit: usize) -> StoreResult<ChangePage> {
        let head = self.cursor();
        if since.epoch != EPOCH || since.seq < self.min_seq || since.seq > head.seq {
            return Ok(ChangePage::Gone);
        }
        let log = self.log.lock().unwrap();
        let after: Vec<_> = log.iter().filter(|c| c.seq > since.seq).cloned().collect();
        let more = after.len() > limit;
        let changes: Vec<_> = after.into_iter().take(limit).collect();
        let next = ChangeCursor {
            epoch: EPOCH,
            seq: changes.last().map_or(since.seq, |c| c.seq),
        };
        Ok(ChangePage::Page {
            changes,
            next,
            more,
        })
    }

    async fn accounts(&self) -> StoreResult<Vec<AccountSummary>> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect())
    }

    async fn create_account(&self, account: NewAccount) -> StoreResult<AccountId> {
        let mut accounts = self.accounts.lock().unwrap();
        let id = AccountId(accounts.len() as i64 + 1);
        accounts.push((
            AccountSummary {
                id,
                name: account.name.clone(),
                email: Some(account.email.clone()),
                status: "active".to_owned(),
                mailboxes_initial: 0,
                mailboxes_catchup: 0,
                mailboxes_live: 0,
                text_pending: 0,
            },
            account,
        ));
        Ok(id)
    }

    async fn message(&self, _: MessageRowId) -> StoreResult<Option<MessageRecord>> {
        Ok(None)
    }

    async fn thread_detail(&self, _: ThreadId) -> StoreResult<Option<ThreadDetail>> {
        Ok(None)
    }

    async fn raw_message(&self, _: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        Ok(None)
    }

    async fn url_keys(&self) -> StoreResult<UrlKeys> {
        Err("no keys".into())
    }

    async fn image_senders(&self) -> StoreResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn set_image_sender(&self, _: String, _: bool) -> StoreResult<()> {
        Err("read-only".into())
    }
}

#[derive(Default)]
struct Registry {
    added: Mutex<Vec<AccountId>>,
}

impl AccountRegistry for Registry {
    fn account_added(&self, id: AccountId) {
        self.added.lock().unwrap().push(id);
    }

    fn runtime_status(&self, id: AccountId) -> Option<RuntimeStatus> {
        (id == AccountId(1)).then(|| RuntimeStatus {
            last_error: Some("auth failed".to_owned()),
            capabilities: vec!["QRESYNC".to_owned()],
            degraded: vec![],
        })
    }

    fn fetch_raw(&self, _: AccountId, _: MessageRowId) -> BoxFuture<Result<(), FetchRawError>> {
        Box::pin(async { Err(FetchRawError::Unavailable("test".to_owned())) })
    }
}

fn row(id: i64) -> ThreadRowDto {
    ThreadRowDto {
        id,
        account_id: 1,
        subject: format!("subject {id}"),
        snippet: String::new(),
        last_date: id,
        message_count: 1,
        unread_count: 0,
        starred: false,
        has_attachment: false,
        senders: vec![],
        labels: vec![],
        mailbox_ids: vec![],
    }
}

fn api(store: MemStore) -> (Api<MemStore>, Arc<Registry>) {
    let registry = Arc::new(Registry::default());
    let api = Api::new(
        Arc::new(store),
        Some(registry.clone() as Arc<dyn AccountRegistry>),
    );
    (api, registry)
}

async fn get(api: &Api<MemStore>, path: &str) -> Response {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    api.handle(Request {
        method: Method::Get,
        path,
        query,
        body: b"",
    })
    .await
}

async fn post(api: &Api<MemStore>, path: &str, body: &Value) -> Response {
    api.handle(Request {
        method: Method::Post,
        path,
        query: "",
        body: &serde_json::to_vec(body).unwrap(),
    })
    .await
}

fn json_body(resp: &Response) -> Value {
    serde_json::from_slice(&resp.body).unwrap()
}

#[tokio::test]
async fn version_reports_api_and_min_client() {
    let (api, _) = api(MemStore::default());
    let resp = get(&api, "/api/version").await;
    assert_eq!(resp.status, 200);
    let body = json_body(&resp);
    assert_eq!(body["api"], json!({ "major": 1, "minor": 0 }));
    assert_eq!(body["minClient"], json!({ "major": 1, "minor": 0 }));
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
    assert!(resp.header("content-security-policy").is_some());
}

#[tokio::test]
async fn unknown_routes_and_methods() {
    let (api, _) = api(MemStore::default());
    assert_eq!(get(&api, "/api/nope").await.status, 404);
    assert_eq!(get(&api, "/other").await.status, 404);
    assert_eq!(get(&api, "/api/views/spam/index").await.status, 404);
    assert_eq!(get(&api, "/api/status/x").await.status, 404);
    assert_eq!(post(&api, "/api/accounts/1", &json!({})).await.status, 405);
    assert_eq!(post(&api, "/api/version", &json!({})).await.status, 405);
}

#[tokio::test]
async fn view_index_is_binary_with_cursor() {
    let ids = vec![9_007_199_254_740_991, 5, 7, 1];
    let store = MemStore {
        inbox: ids.clone(),
        mailbox: vec![(3, vec![2, 1])],
        ..MemStore::default()
    };
    store.append(5);
    let (api, _) = api(store);

    let resp = get(&api, "/api/views/inbox/index").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(resp.header("x-tagmail-cursor"), Some("0000000000000abc:1"));
    assert_eq!(id_index::decode(&resp.body).unwrap(), ids);

    let resp = get(&api, "/api/views/mailbox:3/index").await;
    assert_eq!(id_index::decode(&resp.body).unwrap(), [2, 1]);
}

#[tokio::test]
async fn label_and_search_views_decode_segments_after_splitting() {
    let store = MemStore {
        inbox: vec![7],
        ..MemStore::default()
    };
    let (api, _) = api(store);
    let resp = get(&api, "/api/views/label%3AWork/index").await;
    assert_eq!(resp.status, 200);
    let resp = get(
        &api,
        "/api/views/search%3Aafter%3A2026%2F01%2F01%20%22100%25%20off%22/index",
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(id_index::decode(&resp.body).unwrap(), [7]);
    assert_eq!(
        *api.store().queried.lock().unwrap(),
        [
            View::Label(LabelKey::of("work")),
            View::Search(parse("after:2026/01/01 \"100% off\"").unwrap()),
        ]
    );

    let resp = get(&api, "/api/views/search%3A%28a/index").await;
    assert_eq!(resp.status, 400);
    assert_eq!(json_body(&resp)["error"], "invalid_query");
    assert_eq!(get(&api, "/api/views/search:a/b/index").await.status, 404);
    assert_eq!(get(&api, "/api/views/%FF/index").await.status, 400);
}

#[tokio::test]
async fn mailboxes_and_labels() {
    let (api, _) = api(MemStore::default());
    let resp = get(&api, "/api/mailboxes").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        json_body(&resp),
        json!([{ "id": 1, "accountId": 1, "name": "Entwürfe", "delimiter": "/",
                 "specialUse": "drafts" }])
    );
    let resp = get(&api, "/api/labels").await;
    assert_eq!(
        json_body(&resp),
        json!([{ "key": "work", "name": "Work", "color": null, "accountIds": [1] }])
    );
    assert_eq!(post(&api, "/api/mailboxes", &json!({})).await.status, 405);
}

#[tokio::test]
async fn threads_returns_known_rows_in_request_order() {
    let store = MemStore {
        rows: vec![row(1), row(2), row(3)],
        ..MemStore::default()
    };
    let (api, _) = api(store);
    let resp = get(&api, "/api/threads?ids=3,99,1").await;
    assert_eq!(resp.status, 200);
    let body = json_body(&resp);
    assert_eq!(body["cursor"], "0000000000000abc:0");
    let ids: Vec<i64> = body["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [3, 1]);
    assert_eq!(body["threads"][0]["subject"], "subject 3");
    assert_eq!(body["threads"][0]["messageCount"], 1);
}

#[tokio::test]
async fn threads_rejects_bad_id_lists() {
    let (api, _) = api(MemStore::default());
    for q in ["", "?ids=", "?ids=1,x", "?ids=-1", "?ids=1,,2"] {
        assert_eq!(
            get(&api, &format!("/api/threads{q}")).await.status,
            400,
            "{q}"
        );
    }
    let many: Vec<String> = (0..101).map(|i| i.to_string()).collect();
    let resp = get(&api, &format!("/api/threads?ids={}", many.join(","))).await;
    assert_eq!(resp.status, 400);
}

#[tokio::test]
async fn changes_pages_and_reports_gone() {
    let store = MemStore {
        min_seq: 10,
        ..MemStore::default()
    };
    for id in 0..3 {
        store.append(id);
    }
    let (api, _) = api(store);

    let resp = get(&api, "/api/changes?since=0000000000000abc:10&limit=2").await;
    assert_eq!(resp.status, 200);
    let body = json_body(&resp);
    assert_eq!(body["next"], "0000000000000abc:12");
    assert_eq!(body["more"], true);
    assert_eq!(body["changes"][0]["op"], json!({ "kind": "upsert" }));
    assert_eq!(body["changes"][0]["entity"], "thread");

    let resp = get(&api, "/api/changes?since=0000000000000abc%3A12").await;
    let body = json_body(&resp);
    assert_eq!(body["next"], "0000000000000abc:13");
    assert_eq!(body["more"], false);

    for gone in [
        "0000000000000abd:12",
        "0000000000000abc:9",
        "0000000000000abc:14",
    ] {
        let resp = get(&api, &format!("/api/changes?since={gone}")).await;
        assert_eq!(resp.status, 410, "{gone}");
        assert_eq!(json_body(&resp)["error"], "gone");
    }
    for bad in ["", "?since=abc", "?since=0000000000000abc:1&limit=0"] {
        assert_eq!(
            get(&api, &format!("/api/changes{bad}")).await.status,
            400,
            "{bad}"
        );
    }
}

fn account_json() -> Value {
    json!({
        "name": " Work ",
        "imapHost": "IMAP.example.com",
        "imapPort": 993,
        "imapTls": "tls",
        "smtpHost": "smtp.example.com",
        "smtpPort": 587,
        "smtpTls": "starttls",
        "username": "me@example.com",
        "password": "hunter2",
    })
}

#[tokio::test]
async fn create_account_persists_and_notifies_the_engine() {
    let (api, registry) = api(MemStore::default());
    let resp = post(&api, "/api/accounts", &account_json()).await;
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
    let body = json_body(&resp);
    assert_eq!(body["id"], 1);
    assert_eq!(body["name"], "Work");
    assert_eq!(body["imapHost"], "imap.example.com");
    assert_eq!(body["maxConnections"], 2);
    assert!(body.get("password").is_none());
    assert!(!String::from_utf8_lossy(&resp.body).contains("hunter2"));
    assert_eq!(*registry.added.lock().unwrap(), [AccountId(1)]);

    let stored = &api.store().accounts.lock().unwrap()[0].1;
    assert_eq!(stored.password.expose(), "hunter2");
    assert_eq!(stored.smtp_port, 587);
}

#[tokio::test]
async fn create_account_validates_fields() {
    let (api, registry) = api(MemStore::default());
    let cases = [
        ("name", json!("  ")),
        ("imapHost", json!("bad host")),
        ("smtpHost", json!("")),
        ("imapPort", json!(0)),
        ("username", json!("")),
        ("password", json!("")),
        ("maxConnections", json!(5)),
    ];
    for (field, value) in cases {
        let mut body = account_json();
        body[field] = value;
        let resp = post(&api, "/api/accounts", &body).await;
        assert_eq!(resp.status, 422, "{field}");
        assert_eq!(json_body(&resp)["field"], field);
    }
    let mut body = account_json();
    body["imapTls"] = json!("plain");
    assert_eq!(post(&api, "/api/accounts", &body).await.status, 400);
    body = account_json();
    body["extra"] = json!(1);
    assert_eq!(post(&api, "/api/accounts", &body).await.status, 400);
    assert!(registry.added.lock().unwrap().is_empty());
    assert!(api.store().accounts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_account_needs_an_engine() {
    let api = Api::new(Arc::new(MemStore::default()), None);
    let resp = post(&api, "/api/accounts", &account_json()).await;
    assert_eq!(resp.status, 503);
    assert!(api.store().accounts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn status_merges_persisted_and_runtime_state() {
    let (api, _) = api(MemStore::default());
    post(&api, "/api/accounts", &account_json()).await;
    post(&api, "/api/accounts", &account_json()).await;
    {
        let mut accounts = api.store().accounts.lock().unwrap();
        accounts[0].0.mailboxes_initial = 1;
        accounts[0].0.mailboxes_live = 2;
        accounts[0].0.text_pending = 7;
        accounts[1].0.status = "disabled".to_owned();
    }
    let body = json_body(&get(&api, "/api/status").await);
    let a = &body["accounts"][0];
    assert_eq!(a["phase"], "initial");
    assert_eq!(a["mailboxesTotal"], 3);
    assert_eq!(a["mailboxesLive"], 2);
    assert_eq!(a["textPending"], 7);
    assert_eq!(a["lastError"], "auth failed");
    assert_eq!(a["capabilities"], json!(["QRESYNC"]));
    let b = &body["accounts"][1];
    assert_eq!(b["phase"], "stopped");
    assert_eq!(b["lastError"], Value::Null);
}

#[tokio::test(start_paused = true)]
async fn event_session_resumes_coalesces_and_resets() {
    let store = Arc::new(MemStore::default());
    let feed = ChangeFeed::new(store.cursor());
    store.append(1);
    store.append(2);

    let resume = ChangeCursor {
        epoch: EPOCH,
        seq: 1,
    };
    let mut session = EventSession::open(store.clone(), &feed, Some(resume))
        .await
        .unwrap()
        .unwrap();
    match session.next().await.unwrap().unwrap() {
        Event::Changes(c) => {
            assert_eq!(c.next, "0000000000000abc:2");
            assert_eq!(c.changes.len(), 1);
        }
        e => panic!("{e:?}"),
    }

    let waiting = tokio::spawn(async move {
        let event = session.next().await.unwrap().unwrap();
        (event, session)
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    store.append(3);
    feed.publish(store.cursor());
    tokio::time::sleep(Duration::from_millis(10)).await;
    store.append(4);
    feed.publish(store.cursor());
    let (event, mut session) = waiting.await.unwrap();
    assert_eq!(event.name(), "changes");
    assert_eq!(event.id(), Some("0000000000000abc:4"));
    match &event {
        Event::Changes(c) => assert_eq!(c.changes.len(), 2, "coalesced"),
        e => panic!("{e:?}"),
    }

    feed.publish(ChangeCursor {
        epoch: CacheEpoch(1),
        seq: 0,
    });
    let event = session.next().await.unwrap().unwrap();
    assert_eq!(event.name(), "reset");
    assert_eq!(event.id(), None);
    assert!(event.data().contains("\"gone\""));
    assert_eq!(session.next().await.unwrap(), None);
}

#[tokio::test]
async fn event_session_rejects_stale_resume() {
    let store = Arc::new(MemStore::default());
    let feed = ChangeFeed::new(store.cursor());
    let stale = ChangeCursor {
        epoch: CacheEpoch(7),
        seq: 0,
    };
    let opened = EventSession::open(store, &feed, Some(stale)).await.unwrap();
    assert_eq!(opened.err(), Some(OpenError::Gone));
}

/// Records the write requests it gets and answers from fixed data.
#[derive(Default)]
struct Writes {
    ops: Mutex<Vec<tagmail_api_core::OpRequest>>,
}

fn op_dto(id: &str, status: OpStatusDto) -> OpDto {
    OpDto {
        op_id: id.to_owned(),
        seq: 1,
        account_id: 1,
        kind: OpKindDto::Archive,
        status,
        error: None,
        attempts: 0,
        thread_ids: vec![4, 5],
        label: None,
        mailbox_id: None,
        created_at: 1,
    }
}

impl WriteService for Writes {
    fn submit_op(&self, req: tagmail_api_core::OpRequest) -> BoxFuture<WriteResult<Submitted>> {
        let mut ops = self.ops.lock().unwrap();
        let seen = ops.iter().any(|o| o.op_id == req.op_id);
        let id = req.op_id.clone();
        ops.push(req);
        Box::pin(async move {
            Ok(if seen {
                Submitted::Existing(op_dto(&id, OpStatusDto::Done))
            } else {
                Submitted::Created(op_dto(&id, OpStatusDto::Pending))
            })
        })
    }
    fn ops(&self, status: Option<OpStatusDto>) -> BoxFuture<WriteResult<Vec<OpDto>>> {
        Box::pin(async move { Ok(vec![op_dto("f", status.unwrap_or(OpStatusDto::Pending))]) })
    }
    fn op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        Box::pin(async move {
            if op_id == "missing" {
                Err(WriteError::NotFound("no such op".into()))
            } else {
                Ok(op_dto(&op_id, OpStatusDto::Pending))
            }
        })
    }
    fn retry_op(&self, op_id: String) -> BoxFuture<WriteResult<OpDto>> {
        Box::pin(async move { Err(WriteError::Conflict(format!("{op_id} is not failed"))) })
    }
    fn discard_op(&self, _: String) -> BoxFuture<WriteResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn create_label(&self, label: NewLabelDto) -> BoxFuture<WriteResult<LabelDto>> {
        Box::pin(async move {
            Ok(LabelDto {
                key: LabelKey::of(&label.name).to_string(),
                name: label.name,
                color: label.color,
                account_ids: vec![1],
            })
        })
    }
    fn update_label(
        &self,
        key: String,
        _: LabelPatchDto,
    ) -> BoxFuture<WriteResult<LabelChangeDto>> {
        Box::pin(async move {
            Ok(LabelChangeDto {
                label: Some(LabelDto {
                    key,
                    name: "x".into(),
                    color: None,
                    account_ids: vec![1],
                }),
                job_id: Some(7),
                job_ids: vec![7],
            })
        })
    }
    fn delete_label(&self, _: String) -> BoxFuture<WriteResult<LabelChangeDto>> {
        Box::pin(async {
            Ok(LabelChangeDto {
                label: None,
                job_id: Some(8),
                job_ids: vec![8],
            })
        })
    }
    fn label_jobs(&self) -> BoxFuture<WriteResult<Vec<LabelJobDto>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

async fn call(api: &Api<MemStore>, method: Method, path: &str, body: &[u8]) -> Response {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    api.handle(Request {
        method,
        path,
        query,
        body,
    })
    .await
}

#[tokio::test]
async fn write_routes_need_an_engine() {
    let (api, _) = api(MemStore::default());
    for (method, path) in [
        (Method::Post, "/api/ops"),
        (Method::Get, "/api/ops"),
        (Method::Delete, "/api/ops/x"),
        (Method::Post, "/api/labels"),
        (Method::Patch, "/api/labels/work"),
        (Method::Get, "/api/label-jobs"),
    ] {
        let resp = call(&api, method, path, b"{}").await;
        assert_eq!(resp.status, 503, "{method:?} {path}");
    }
}

#[tokio::test]
async fn write_routes_reach_the_write_service() {
    let (api, _) = api(MemStore::default());
    let writes = Arc::new(Writes::default());
    let api = api.with_writes(writes.clone() as Arc<dyn WriteService>);

    let body = json!({ "opId": "u-1", "kind": "archive", "threadIds": [4, 5] });
    let resp = post(&api, "/api/ops", &body).await;
    assert_eq!(resp.status, 202);
    assert_eq!(json_body(&resp)["status"], "pending");
    let again = post(&api, "/api/ops", &body).await;
    assert_eq!(
        (again.status, json_body(&again)["status"].clone()),
        (200, json!("done"))
    );
    {
        let ops = writes.ops.lock().unwrap();
        assert_eq!(ops[0].thread_ids, [ThreadId(4), ThreadId(5)]);
        assert_eq!(ops[0].kind, tagmail_core::ops::OpKind::Archive);
    }

    let bad = post(
        &api,
        "/api/ops",
        &json!({ "opId": "u-2", "kind": "move", "threadIds": [1] }),
    )
    .await;
    assert_eq!(
        (bad.status, json_body(&bad)["field"].clone()),
        (422, json!("mailboxId"))
    );
    let unknown = post(
        &api,
        "/api/ops",
        &json!({ "opId": "u-3", "kind": "nuke", "threadIds": [1] }),
    )
    .await;
    assert_eq!(unknown.status, 400);

    let list = get(&api, "/api/ops?status=failed").await;
    assert_eq!(json_body(&list)["ops"][0]["status"], "failed");
    assert_eq!(get(&api, "/api/ops?status=done").await.status, 400);
    assert_eq!(get(&api, "/api/ops/missing").await.status, 404);
    assert_eq!(
        call(&api, Method::Post, "/api/ops/u-1/retry", b"")
            .await
            .status,
        409
    );
    assert_eq!(
        call(&api, Method::Delete, "/api/ops/u-1", b"").await.status,
        204
    );

    let created = post(
        &api,
        "/api/labels",
        &json!({ "name": "Work", "color": "#aa0000" }),
    )
    .await;
    assert_eq!(
        (created.status, json_body(&created)["key"].clone()),
        (201, json!("work"))
    );
    let renamed = call(
        &api,
        Method::Patch,
        "/api/labels/work",
        br#"{"name":"Job"}"#,
    )
    .await;
    assert_eq!(json_body(&renamed)["jobId"], json!(7));
    let deleted = call(&api, Method::Delete, "/api/labels/work", b"").await;
    assert_eq!(
        (deleted.status, json_body(&deleted)["label"].clone()),
        (202, Value::Null)
    );
    assert_eq!(
        json_body(&get(&api, "/api/label-jobs").await)["jobs"],
        json!([])
    );
}
