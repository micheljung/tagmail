//! Tagmail desktop shell. The lib holds everything testable without
//! WebKit; the Tauri binary (feature `app`) wires it to the webview.

pub mod backend;
pub mod desktop;
pub mod events;
pub mod mailto;
pub mod notify;
pub mod protocol;
#[cfg(feature = "test-hooks")]
pub mod selftest;
pub mod settings;
pub mod state;
pub mod token;
pub mod tray;
