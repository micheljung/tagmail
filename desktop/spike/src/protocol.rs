use std::collections::BTreeMap;
use std::sync::Mutex;

use http::{Method, Request, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::replica::{FlushReport, Replica};
use crate::server::MockServer;

pub const SPA_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; frame-src 'self'; connect-src 'self' ipc: http://ipc.localhost";
pub const RENDER_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:";
const THREAD_PAGE: usize = 50;
const PIXEL_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

/// Side effects the Tauri shell turns into `emit` calls or process exit.
#[derive(Debug)]
pub enum Event {
    Flushed(FlushReport),
    Report(Value),
}

pub struct State {
    pub replica: Mutex<Replica>,
    pub server: Mutex<MockServer>,
    beacons: Mutex<BTreeMap<String, u64>>,
}

#[derive(Deserialize)]
struct LabelRequest {
    email_ids: Vec<String>,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Deserialize)]
struct NetworkRequest {
    online: bool,
    #[serde(default)]
    lose_acks: bool,
}

impl State {
    pub fn bootstrap(messages: usize) -> rusqlite::Result<Self> {
        let server = MockServer::synthetic(messages);
        let mut replica = Replica::open_in_memory()?;
        replica.bootstrap(&server.snapshot_ndjson())?;
        Ok(Self {
            replica: Mutex::new(replica),
            server: Mutex::new(server),
            beacons: Mutex::default(),
        })
    }
}

pub fn handle(state: &State, req: &Request<Vec<u8>>) -> (Response<Vec<u8>>, Vec<Event>) {
    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let mut events = Vec::new();
    let response = match (req.method(), segments.as_slice()) {
        (&Method::GET, [""] | ["index.html"]) => {
            asset("text/html", include_str!("../ui/index.html"))
        }
        (&Method::GET, ["app.js"]) => asset("text/javascript", include_str!("../ui/app.js")),
        (&Method::GET, ["api", "threads"]) => {
            match state.replica.lock().unwrap().threads(THREAD_PAGE) {
                Ok(threads) => json_response(StatusCode::OK, &json!(threads)),
                Err(e) => error(&e),
            }
        }
        (&Method::GET, ["api", "messages", id, "render"]) => render(id, !query.contains("csp=0")),
        (&Method::GET, ["api", "beacon", kind]) => {
            *state
                .beacons
                .lock()
                .unwrap()
                .entry(format!("{kind}:{query}"))
                .or_default() += 1;
            with_type(Response::builder(), "image/gif")
                .body(PIXEL_GIF.to_vec())
                .expect("valid response")
        }
        (&Method::GET, ["api", "beacons"]) => {
            json_response(StatusCode::OK, &json!(*state.beacons.lock().unwrap()))
        }
        (&Method::GET, ["api", "server", "labels", id]) => json_response(
            StatusCode::OK,
            &json!(state.server.lock().unwrap().labels(id)),
        ),
        (&Method::POST, ["api", "labels"]) => {
            match serde_json::from_slice::<LabelRequest>(req.body()) {
                Ok(r) => match state
                    .replica
                    .lock()
                    .unwrap()
                    .queue(r.email_ids, r.add, r.remove)
                {
                    Ok(op_id) => json_response(StatusCode::ACCEPTED, &json!({ "op_id": op_id })),
                    Err(e) => error(&e),
                },
                Err(e) => error(&e),
            }
        }
        (&Method::POST, ["api", "network"]) => {
            match serde_json::from_slice::<NetworkRequest>(req.body()) {
                Ok(r) => {
                    let mut server = state.server.lock().unwrap();
                    server.online = r.online;
                    match state
                        .replica
                        .lock()
                        .unwrap()
                        .flush(&mut server, r.lose_acks)
                    {
                        Ok(report) => {
                            let body = json!({ "flush": report, "server_applied": server.applied });
                            events.push(Event::Flushed(report));
                            json_response(StatusCode::OK, &body)
                        }
                        Err(e) => error(&e),
                    }
                }
                Err(e) => error(&e),
            }
        }
        (&Method::POST, ["api", "report"]) => match serde_json::from_slice::<Value>(req.body()) {
            Ok(v) => {
                events.push(Event::Report(v));
                json_response(StatusCode::OK, &json!({}))
            }
            Err(e) => error(&e),
        },
        _ => json_response(StatusCode::NOT_FOUND, &json!({ "error": "not found" })),
    };
    (response, events)
}

/// Untrusted mail HTML stand-in: a tall body, an image and a script that
/// each phone home, so the SPA can tell what the CSP and sandbox blocked.
fn render(id: &str, csp: bool) -> Response<Vec<u8>> {
    let variant = if csp { "csp" } else { "nocsp" };
    let html = format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <div id=\"body\" style=\"height:1500px\">Message {id}</div>\
         <img src=\"/api/beacon/img?{variant}\" alt=\"\">\
         <script>fetch('/api/beacon/script?{variant}')</script>\
         </body></html>"
    );
    let mut builder = with_type(Response::builder(), "text/html");
    if csp {
        builder = builder.header(header::CONTENT_SECURITY_POLICY, RENDER_CSP);
    }
    builder.body(html.into_bytes()).expect("valid response")
}

fn asset(content_type: &str, body: &str) -> Response<Vec<u8>> {
    with_type(Response::builder(), content_type)
        .header(header::CONTENT_SECURITY_POLICY, SPA_CSP)
        .body(body.as_bytes().to_vec())
        .expect("valid response")
}

fn json_response(status: StatusCode, body: &Value) -> Response<Vec<u8>> {
    with_type(Response::builder().status(status), "application/json")
        .body(body.to_string().into_bytes())
        .expect("valid response")
}

fn error(e: &dyn std::fmt::Display) -> Response<Vec<u8>> {
    json_response(StatusCode::BAD_REQUEST, &json!({ "error": e.to_string() }))
}

fn with_type(builder: http::response::Builder, content_type: &str) -> http::response::Builder {
    builder
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        state: &State,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (Response<Vec<u8>>, Vec<Event>) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(body.as_bytes().to_vec())
            .unwrap();
        handle(state, &req)
    }

    fn body_json(resp: &Response<Vec<u8>>) -> Value {
        serde_json::from_slice(resp.body()).unwrap()
    }

    #[test]
    fn serves_spa_and_api_on_one_origin() {
        let state = State::bootstrap(1000).unwrap();
        let (index, _) = call(&state, Method::GET, "app://localhost/", "");
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(index.headers()[header::CONTENT_SECURITY_POLICY], SPA_CSP);
        let (threads, _) = call(&state, Method::GET, "app://localhost/api/threads", "");
        assert_eq!(body_json(&threads).as_array().unwrap().len(), THREAD_PAGE);
    }

    #[test]
    fn render_carries_its_own_csp() {
        let state = State::bootstrap(3).unwrap();
        let (with, _) = call(
            &state,
            Method::GET,
            "app://localhost/api/messages/E000001/render",
            "",
        );
        assert_eq!(with.headers()[header::CONTENT_SECURITY_POLICY], RENDER_CSP);
        let (without, _) = call(
            &state,
            Method::GET,
            "app://localhost/api/messages/E000001/render?csp=0",
            "",
        );
        assert!(
            !without
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
    }

    #[test]
    fn reconnect_flushes_and_emits() {
        let state = State::bootstrap(30).unwrap();
        call(
            &state,
            Method::POST,
            "app://localhost/api/network",
            r#"{"online":false}"#,
        );
        let (queued, _) = call(
            &state,
            Method::POST,
            "app://localhost/api/labels",
            r#"{"email_ids":["E000029"],"add":["Spike"]}"#,
        );
        assert_eq!(queued.status(), StatusCode::ACCEPTED);
        let (resp, events) = call(
            &state,
            Method::POST,
            "app://localhost/api/network",
            r#"{"online":true}"#,
        );
        assert_eq!(body_json(&resp)["server_applied"], 1);
        assert!(matches!(
            events.as_slice(),
            [Event::Flushed(FlushReport {
                sent: 1,
                duplicates: 0,
                remaining: 0
            })]
        ));
        let (labels, _) = call(
            &state,
            Method::GET,
            "app://localhost/api/server/labels/E000029",
            "",
        );
        assert_eq!(body_json(&labels), json!(["Inbox", "Spike", "Work"]));
    }
}
