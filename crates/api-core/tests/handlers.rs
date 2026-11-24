use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tagmail_api_core::dto::{
    AccountDto, AccountStateDto, AccountTestDto, ChangeDto, ChangeOpDto, EntityDto, LabelDto,
    MailboxDto, SpecialUseDto, TestStepDto, ThreadRowDto,
};
use tagmail_api_core::id_index;
use tagmail_api_core::ops_dto::{
    LabelChangeDto, LabelJobDto, LabelPatchDto, NewLabelDto, OpDto, OpKindDto, OpStatusDto,
};
use tagmail_api_core::signing::UrlKeys;
use tagmail_api_core::{
    AccountPatch, AccountRegistry, AccountSummary, Api, BoxFuture, ChangeFeed, ChangePage, Event,
    EventSession, FetchRawError, MarkDeleting, MessageRecord, Method, NewAccount, OpenError,
    Request, Response, RuntimeStatus, Store, StoreResult, ThreadDetail, ThreadRows, View,
    ViewIndex,
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
    /// Labels a [`LabelWrites`] keeps; empty: one fixed label.
    labels: Arc<Mutex<Vec<LabelDto>>>,
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
        let kept = self.labels.lock().unwrap().clone();
        if !kept.is_empty() {
            return Ok(kept);
        }
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

    async fn accounts_full(&self) -> StoreResult<Vec<AccountDto>> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .iter()
            .map(|(s, a)| account_dto(s, a))
            .collect())
    }

    async fn account_settings(&self, id: AccountId) -> StoreResult<Option<NewAccount>> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s.id == id)
            .map(|(_, a)| a.clone()))
    }

    async fn update_account(
        &self,
        id: AccountId,
        patch: AccountPatch,
    ) -> StoreResult<Option<AccountDto>> {
        let mut accounts = self.accounts.lock().unwrap();
        let Some((s, a)) = accounts
            .iter_mut()
            .find(|(s, _)| s.id == id && s.status != "deleting")
        else {
            return Ok(None);
        };
        if let Some(name) = patch.name {
            s.name = name.clone();
            a.name = name;
        }
        if let Some(password) = patch.password {
            a.password = password;
        }
        if let Some(host) = patch.imap_host {
            a.imap_host = host;
        }
        if let Some(email) = patch.email {
            s.email = Some(email.clone());
            a.email = email;
        }
        if let Some(status) = patch.status {
            s.status = status.as_str().to_owned();
        }
        Ok(Some(account_dto(s, a)))
    }

    async fn mark_deleting(&self, id: AccountId) -> StoreResult<MarkDeleting> {
        let mut accounts = self.accounts.lock().unwrap();
        Ok(match accounts.iter_mut().find(|(s, _)| s.id == id) {
            Some((s, _)) if s.status == "deleting" => MarkDeleting::AlreadyDeleting,
            Some((s, _)) => {
                s.status = "deleting".to_owned();
                MarkDeleting::Marked
            }
            None => MarkDeleting::Missing,
        })
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

fn account_dto(s: &AccountSummary, a: &NewAccount) -> AccountDto {
    AccountDto {
        id: s.id.0,
        name: a.name.clone(),
        imap_host: a.imap_host.clone(),
        imap_port: a.imap_port,
        imap_tls: a.imap_tls,
        smtp_host: a.smtp_host.clone(),
        smtp_port: a.smtp_port,
        smtp_tls: a.smtp_tls,
        username: a.username.clone(),
        max_connections: a.max_connections,
        email: s.email.clone(),
        status: AccountStateDto::parse(&s.status).unwrap(),
    }
}

#[derive(Default)]
struct Registry {
    added: Mutex<Vec<AccountId>>,
    updated: Mutex<Vec<(AccountId, bool)>>,
    removed: Mutex<Vec<AccountId>>,
    tested: Mutex<Vec<NewAccount>>,
}

impl AccountRegistry for Registry {
    fn account_added(&self, id: AccountId) {
        self.added.lock().unwrap().push(id);
    }

    fn account_updated(&self, id: AccountId, restart: bool) {
        self.updated.lock().unwrap().push((id, restart));
    }

    fn account_removed(&self, id: AccountId) -> BoxFuture<()> {
        self.removed.lock().unwrap().push(id);
        Box::pin(async {})
    }

    fn test_account(&self, settings: NewAccount) -> BoxFuture<AccountTestDto> {
        let ok = settings.password.expose() == "hunter2";
        self.tested.lock().unwrap().push(settings);
        let step = TestStepDto {
            ok,
            error: (!ok).then(|| "authentication failed".to_owned()),
            ms: 1,
        };
        Box::pin(async move {
            AccountTestDto {
                imap: step.clone(),
                smtp: step,
                capabilities: vec!["IMAP4rev1".to_owned()],
                mailboxes: ok.then_some(3),
            }
        })
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

async fn send(api: &Api<MemStore>, method: Method, path: &str, body: &Value) -> Response {
    api.handle(Request {
        method,
        path,
        query: "",
        body: &serde_json::to_vec(body).unwrap(),
    })
    .await
}

async fn patch(api: &Api<MemStore>, path: &str, body: &Value) -> Response {
    send(api, Method::Patch, path, body).await
}

async fn delete(api: &Api<MemStore>, path: &str) -> Response {
    send(api, Method::Delete, path, &Value::Null).await
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
    assert_eq!(body["api"], json!({ "major": 1, "minor": 3 }));
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
async fn sender_address_comes_from_email_or_the_username() {
    let (api, _) = api(MemStore::default());
    let mut body = account_json();
    body["email"] = json!(" Work@Example.com ");
    body["displayName"] = json!("Ann");
    let resp = post(&api, "/api/accounts", &body).await;
    assert_eq!(json_body(&resp)["email"], "Work@Example.com");
    let stored = api.store().accounts.lock().unwrap()[0].1.clone();
    assert_eq!(
        (stored.email.as_str(), stored.display_name.as_deref()),
        ("Work@Example.com", Some("Ann"))
    );

    let resp = post(&api, "/api/accounts", &account_json()).await;
    assert_eq!(json_body(&resp)["email"], "me@example.com");

    let mut bare = account_json();
    bare["username"] = json!("me");
    let resp = post(&api, "/api/accounts", &bare).await;
    assert_eq!(resp.status, 422);
    assert_eq!(json_body(&resp)["field"], "email");
    bare["email"] = json!("not-an-address");
    assert_eq!(
        json_body(&post(&api, "/api/accounts", &bare).await)["field"],
        "email"
    );
    assert_eq!(api.store().accounts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn accounts_are_listed_patched_tested_and_deleted() {
    let (api, registry) = api(MemStore::default());
    post(&api, "/api/accounts", &account_json()).await;
    let list = json_body(&get(&api, "/api/accounts").await);
    assert_eq!(list["accounts"][0]["status"], "active");
    assert!(!list.to_string().contains("hunter2"));

    let resp = patch(&api, "/api/accounts/1", &json!({ "name": "Home" })).await;
    assert_eq!(resp.status, 200);
    assert_eq!(json_body(&resp)["name"], "Home");
    let resp = patch(
        &api,
        "/api/accounts/1",
        &json!({ "password": "new", "status": "disabled" }),
    )
    .await;
    assert_eq!(json_body(&resp)["status"], "disabled");
    assert_eq!(
        *registry.updated.lock().unwrap(),
        [(AccountId(1), false), (AccountId(1), true)]
    );
    for (body, field) in [
        (json!({ "status": "deleting" }), "status"),
        (json!({ "email": "x" }), "email"),
        (json!({ "imapPort": 0 }), "imapPort"),
    ] {
        let resp = patch(&api, "/api/accounts/1", &body).await;
        assert_eq!(
            (resp.status, json_body(&resp)["field"].clone()),
            (422, json!(field))
        );
    }
    assert_eq!(patch(&api, "/api/accounts/9", &json!({})).await.status, 404);
    assert_eq!(
        patch(&api, "/api/accounts/1", &json!({ "x": 1 }))
            .await
            .status,
        400
    );

    let resp = post(
        &api,
        "/api/accounts/1/test",
        &json!({ "password": "hunter2" }),
    )
    .await;
    let body = json_body(&resp);
    assert_eq!(
        (body["imap"]["ok"].clone(), body["mailboxes"].clone()),
        (json!(true), json!(3))
    );
    let resp = post(&api, "/api/accounts/1/test", &json!({})).await;
    assert_eq!(
        json_body(&resp)["smtp"]["ok"],
        false,
        "stored password is now 'new'"
    );
    let mut bare = account_json();
    bare["username"] = json!("me");
    let resp = post(&api, "/api/accounts/test", &bare).await;
    assert_eq!(resp.status, 200, "a test needs no sender address");
    assert_eq!(
        registry.tested.lock().unwrap()[0].imap_host,
        "imap.example.com"
    );
    assert_eq!(
        api.store().accounts.lock().unwrap().len(),
        1,
        "tests save nothing"
    );
    assert_eq!(
        post(&api, "/api/accounts/9/test", &json!({})).await.status,
        404
    );

    for _ in 0..2 {
        let resp = delete(&api, "/api/accounts/1").await;
        assert_eq!((resp.status, json_body(&resp)), (202, json!({ "id": 1 })));
    }
    assert_eq!(
        *registry.removed.lock().unwrap(),
        [AccountId(1)],
        "a repeated delete starts no second removal"
    );
    assert_eq!(
        patch(&api, "/api/accounts/1", &json!({ "name": "x" }))
            .await
            .status,
        404
    );
    assert_eq!(delete(&api, "/api/accounts/7").await.status, 404);
}

#[tokio::test]
async fn moving_the_connection_needs_the_password_again() {
    let (api, registry) = api(MemStore::default());
    post(&api, "/api/accounts", &account_json()).await;
    for body in [
        json!({ "imapHost": "evil.example.net" }),
        json!({ "imapPort": 143 }),
        json!({ "imapTls": "starttls" }),
        json!({ "smtpHost": "evil.example.net" }),
        json!({ "smtpPort": 465 }),
        json!({ "smtpTls": "tls" }),
        json!({ "username": "other@example.com" }),
    ] {
        for path in ["/api/accounts/1", "/api/accounts/1/test"] {
            let method = if path.ends_with("test") {
                Method::Post
            } else {
                Method::Patch
            };
            let resp = send(&api, method, path, &body).await;
            assert_eq!(
                (resp.status, json_body(&resp)["field"].clone()),
                (422, json!("password")),
                "{path} {body}"
            );
        }
    }
    assert!(registry.tested.lock().unwrap().is_empty());
    assert!(registry.updated.lock().unwrap().is_empty());
    assert_eq!(
        api.store().accounts.lock().unwrap()[0].1.imap_host,
        "imap.example.com"
    );

    let unchanged = json!({ "imapHost": "imap.example.com", "smtpPort": 587, "name": "W" });
    assert_eq!(patch(&api, "/api/accounts/1", &unchanged).await.status, 200);
    assert_eq!(
        post(&api, "/api/accounts/1/test", &unchanged).await.status,
        200
    );
    let moved = json!({ "imapHost": "imap2.example.com", "password": "hunter2" });
    assert_eq!(post(&api, "/api/accounts/1/test", &moved).await.status, 200);
    assert_eq!(patch(&api, "/api/accounts/1", &moved).await.status, 200);
    assert_eq!(
        api.store().accounts.lock().unwrap()[0].1.imap_host,
        "imap2.example.com"
    );
    assert_eq!(
        patch(&api, "/api/accounts/9", &json!({ "imapPort": 1 }))
            .await
            .status,
        404
    );
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
        (422, json!("mailbox"))
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

/// Creates and recolours labels in the store's shared list.
struct LabelWrites {
    labels: Arc<Mutex<Vec<LabelDto>>>,
    calls: Mutex<Vec<String>>,
}

impl WriteService for LabelWrites {
    fn submit_op(&self, _: tagmail_api_core::OpRequest) -> BoxFuture<WriteResult<Submitted>> {
        unreachable!()
    }
    fn ops(&self, _: Option<OpStatusDto>) -> BoxFuture<WriteResult<Vec<OpDto>>> {
        unreachable!()
    }
    fn op(&self, _: String) -> BoxFuture<WriteResult<OpDto>> {
        unreachable!()
    }
    fn retry_op(&self, _: String) -> BoxFuture<WriteResult<OpDto>> {
        unreachable!()
    }
    fn discard_op(&self, _: String) -> BoxFuture<WriteResult<()>> {
        unreachable!()
    }
    fn create_label(&self, label: NewLabelDto) -> BoxFuture<WriteResult<LabelDto>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("create {}", label.name));
        let dto = LabelDto {
            key: LabelKey::of(&label.name).to_string(),
            name: label.name,
            color: label.color,
            account_ids: vec![1],
        };
        self.labels.lock().unwrap().push(dto.clone());
        Box::pin(async move { Ok(dto) })
    }
    fn update_label(
        &self,
        key: String,
        patch: LabelPatchDto,
    ) -> BoxFuture<WriteResult<LabelChangeDto>> {
        self.calls.lock().unwrap().push(format!("update {key}"));
        let mut labels = self.labels.lock().unwrap();
        let label = labels.iter_mut().find(|l| l.key == key).unwrap();
        label.color = patch.color.filter(|c| !c.is_empty());
        let label = label.clone();
        Box::pin(async move {
            Ok(LabelChangeDto {
                label: Some(label),
                job_id: None,
                job_ids: vec![],
            })
        })
    }
    fn delete_label(&self, _: String) -> BoxFuture<WriteResult<LabelChangeDto>> {
        unreachable!()
    }
    fn label_jobs(&self) -> BoxFuture<WriteResult<Vec<LabelJobDto>>> {
        unreachable!()
    }
}

#[tokio::test]
async fn label_import_is_idempotent_and_export_round_trips() {
    let labels = Arc::new(Mutex::new(vec![LabelDto {
        key: "work".to_owned(),
        name: "Work".to_owned(),
        color: None,
        account_ids: vec![1],
    }]));
    let writes = Arc::new(LabelWrites {
        labels: labels.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let (api, _) = api(MemStore {
        labels,
        ..MemStore::default()
    });
    let api = api.with_writes(writes.clone() as Arc<dyn WriteService>);
    let file = json!({
        "format": "tagmail-labels",
        "version": 1,
        "labels": [
            { "name": "Travel", "color": "#00AA00", "sort": 2 },
            { "name": "work", "color": "#112233", "sort": 1 },
            { "name": "Bad", "color": "red", "sort": 3 },
        ],
    });
    let first = json_body(&post(&api, "/api/labels/import", &file).await);
    assert_eq!(
        (&first["created"], &first["updated"], &first["unchanged"]),
        (&json!(1), &json!(1), &json!(0))
    );
    assert_eq!(first["errors"][0]["name"], "Bad");
    assert_eq!(
        *writes.calls.lock().unwrap(),
        ["update work", "create Travel"],
        "applied in sort order, never renamed"
    );
    let second = json_body(&post(&api, "/api/labels/import", &file).await);
    assert_eq!(
        (&second["created"], &second["updated"], &second["unchanged"]),
        (&json!(0), &json!(0), &json!(2))
    );
    assert_eq!(writes.calls.lock().unwrap().len(), 2);

    let resp = get(&api, "/api/labels/export").await;
    assert_eq!(
        resp.header("content-disposition"),
        Some("attachment; filename=\"tagmail-labels.json\"")
    );
    let export = json_body(&resp);
    assert_eq!(export["format"], "tagmail-labels");
    assert_eq!(
        export["labels"],
        json!([
            { "name": "Work", "color": "#112233", "sort": 0 },
            { "name": "Travel", "color": "#00aa00", "sort": 1 },
        ])
    );
    let third = json_body(&post(&api, "/api/labels/import", &export).await);
    assert_eq!(third["unchanged"], 2);

    for bad in [
        json!({ "format": "other", "version": 1, "labels": [] }),
        json!({ "format": "tagmail-labels", "version": 2, "labels": [] }),
    ] {
        assert_eq!(post(&api, "/api/labels/import", &bad).await.status, 400);
    }
    let many: Vec<Value> = (0..1001)
        .map(|i| json!({ "name": format!("l{i}"), "color": null, "sort": i }))
        .collect();
    let too_many = json!({ "format": "tagmail-labels", "version": 1, "labels": many });
    assert_eq!(
        post(&api, "/api/labels/import", &too_many).await.status,
        400
    );
}
