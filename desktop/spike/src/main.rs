use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spike::protocol::{self, Event, State};
use tauri::{Emitter, WebviewUrl, WebviewWindowBuilder};

const MESSAGES: usize = 1000;
const TICK: Duration = Duration::from_millis(250);
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
const ORIGIN: &str = "http://app.localhost/";
#[cfg(not(windows))]
const ORIGIN: &str = "app://localhost/";

fn main() {
    let started = Instant::now();
    let state = Arc::new(State::bootstrap(MESSAGES).expect("bootstrap replica"));
    println!(
        "SPIKE_BOOTSTRAP messages={MESSAGES} ms={}",
        started.elapsed().as_millis()
    );
    let selftest = std::env::var_os("SPIKE_SELFTEST").is_some();

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("app", move |ctx, req, responder| {
            let app = ctx.app_handle().clone();
            let state = state.clone();
            std::thread::spawn(move || {
                let (response, events) = protocol::handle(&state, &req);
                responder.respond(response);
                for event in events {
                    match event {
                        Event::Flushed(report) => {
                            app.emit("replica:flushed", report).expect("emit")
                        }
                        Event::Report(report) => {
                            println!("SPIKE_REPORT {report}");
                            if selftest {
                                let ok = report["ok"].as_bool().unwrap_or(false);
                                app.exit(if ok { 0 } else { 1 });
                            }
                        }
                    }
                }
            });
        })
        .setup(move |app| {
            let url = ORIGIN.parse().expect("valid origin");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::CustomProtocol(url))
                .title("Tagmail spike")
                .build()?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                for seq in 0u64.. {
                    let sent_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("clock")
                        .as_millis() as u64;
                    if handle
                        .emit(
                            "tick",
                            serde_json::json!({ "seq": seq, "sent_ms": sent_ms }),
                        )
                        .is_err()
                    {
                        break;
                    }
                    std::thread::sleep(TICK);
                }
            });
            if selftest {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(SELFTEST_TIMEOUT);
                    eprintln!("SPIKE_TIMEOUT no report after {SELFTEST_TIMEOUT:?}");
                    handle.exit(2);
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run tauri app");
}
