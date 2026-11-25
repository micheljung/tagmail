//! The desktop router online: search through the server's index,
//! out-of-window opens fetched from the server, server-only routes passed
//! through and uploads kept locally.

mod fixture;

use fixture::{Stack, call, get, ids, json, search_path};
use tagmail_api_core::{Method, PARTIAL_HEADER};
use tagmail_replica::Router;

#[tokio::test]
async fn online_search_uses_the_server_index() {
    let stack = Stack::start().await;
    let router = Router::new(stack.replica().await);

    let resp = get(&router, &search_path("news")).await;
    assert_eq!(
        ids(&resp),
        vec![2],
        "out-of-window body text is a server hit"
    );
    assert!(resp.header(PARTIAL_HEADER).is_none());
    assert!(resp.header("x-tagmail-cursor").is_some());
    assert_eq!(ids(&get(&router, &search_path("fox")).await), vec![2]);
}

#[tokio::test]
async fn an_out_of_window_message_is_fetched_on_open() {
    let stack = Stack::start().await;
    let replica = stack.replica().await;
    let router = Router::new(replica.clone());
    let resp = get(&router, "/api/messages/13/render").await;
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert!(String::from_utf8_lossy(&resp.body).contains("ancient scroll"));
    let raw = replica
        .store()
        .unwrap()
        .cache()
        .read(|c| {
            Ok(
                c.query_row("SELECT raw_blob FROM message WHERE id = 13", [], |r| {
                    r.get::<_, Option<String>>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(raw.is_some(), "kept for later offline opens");
}

#[tokio::test]
async fn server_only_routes_pass_through() {
    let stack = Stack::start().await;
    let router = Router::new(stack.replica().await);
    let body = serde_json::json!({ "accountId": stack.account, "address": "alt@x.example" });
    let resp = call(
        &router,
        Method::Post,
        "/api/identities",
        body.to_string().as_bytes(),
    )
    .await;
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
    let server = stack.owner_get("/api/identities").await;
    assert!(
        server["identities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["address"] == "alt@x.example")
    );
    let resp = call(&router, Method::Post, "/api/labels", br#"{"name":""}"#).await;
    assert_eq!(resp.status, 422, "the server's validation answer is kept");
}

#[tokio::test]
async fn calendar_routes_pass_through() {
    let stack = Stack::start().await;
    let router = Router::new(stack.replica().await);
    let settings = get(&router, &format!("/api/accounts/{}/caldav", stack.account)).await;
    assert_eq!(
        settings.status,
        200,
        "{}",
        String::from_utf8_lossy(&settings.body)
    );
    assert_eq!(json(&settings)["status"], "unconfigured");
    let calendars = get(&router, "/api/calendars").await;
    assert_eq!(calendars.status, 200);
    assert_eq!(json(&calendars)["calendars"], serde_json::json!([]));
}

#[tokio::test]
async fn an_upload_is_kept_locally() {
    let stack = Stack::start().await;
    let replica = stack.replica().await;
    let router = Router::new(replica.clone());
    let body = "--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\
                Content-Type: text/plain\r\n\r\nhello\r\n--XyZ--\r\n";
    let resp = router
        .handle(
            tagmail_api_core::Request {
                method: Method::Post,
                path: "/api/attachments",
                query: "",
                body: body.as_bytes(),
            },
            Some("multipart/form-data; boundary=XyZ"),
        )
        .await;
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
    let dto = json(&resp);
    assert_eq!(dto["name"], "a.txt");
    assert_eq!(dto["size"], 5);
    let count: i64 = replica
        .durable()
        .conn()
        .query_row("SELECT count(*) FROM draft_attachment", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
