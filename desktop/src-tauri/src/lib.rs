//! Tagmail desktop shell. The lib holds everything testable without
//! WebKit; the Tauri binary (feature `app`) wires it to the webview.

pub mod events;
pub mod protocol;
#[cfg(feature = "test-hooks")]
pub mod selftest;
pub mod state;
