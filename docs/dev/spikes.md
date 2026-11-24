# Spikes

Results of the three M0 spikes and the M1 replica spike (2026-11, Rust
1.100). Numbers are from single developer-machine runs; treat them as orders
of magnitude.

| # | Spike | Decision |
|---|-------|----------|
| 1 | IMAP library | **Go:** `imap-next` 0.3.4 on `imap-codec` `=2.0.0-alpha.10`, behind the `ImapSession` trait; `async-imap` stays the fallback |
| 2 | 100k-row conversation list | **Go:** Svelte 5 + in-house virtualizer; `@tanstack/svelte-virtual` rejected |
| 3 | OpenSSL | **Go:** OpenSSL-free, musl static; enforced in `deny.toml` |
| 4 | Replica + transport (M1) | **Go on WebKitGTK:** one `app://localhost` origin for SPA and `/api/*`, events via `emit`; WKWebView and WebView2 not run yet |

## 1. IMAP library

Tested against Dovecot 2.4.5 (config reused in `test/dovecot/`).

**Works with imap-next:** `ENABLE QRESYNC`, `SELECT (CONDSTORE)` and
`SELECT (QRESYNC (uidvalidity modseq))` yielding a typed
`VANISHED (EARLIER)` plus the changed FETCHes, `UID FETCH ... (CHANGEDSINCE n VANISHED)`,
IDLE with EXISTS from another connection's APPEND (~0.5 s Dovecot notify delay),
custom keyword `$label_Work` visible to other sessions with `PERMANENTFLAGS (... \*)`,
`UID MOVE` (COPYUID and VANISHED), METADATA and SPECIAL-USE.

**Literal buffering (25 MiB message, peak RSS increase):**

| Mode | Peak RSS | Time |
|------|----------|------|
| imap-next, full `BODY.PEEK[]` | +50.7 MB (2x message) | ~130 ms |
| imap-next, chunked `BODY.PEEK[]<off.1048576>` | +2.0 MB | ~140 ms |
| async-imap, full `BODY.PEEK[]` | +25.4 MB (1x message) | ~30 ms |

**Parser corpus** (46 crafted quirk lines, not recorded transcripts):
imap-codec 32/46, imap-proto (async-imap) 37/46. An unparseable response
makes imap-next return `MalformedMessage` for that response only and the
session continues, with the raw bytes available. A real Dovecot `* ESEARCH`
reply killed the async-imap session.

**Why not async-imap:** it has no QRESYNC API (raw commands, untyped
unsolicited channel), cannot be replayed sans-I/O, and is fatal on ESEARCH.
It parses X-GM-\* and EMAILID, so it is the fallback for the trait.

**Follow-ups**

- imap-codec cannot encode or parse **ESEARCH**, **X-GM-\*** or **OBJECTID**
  (EMAILID/THREADID). Upstream PRs preferred; otherwise a hand parser over the
  discarded bytes. Until then: plain `UID SEARCH`, and the hash `email_id` path.
- Never request FETCH items the codec doesn't know (PREVIEW, SAVEDATE,
  X-GM-\*): one unknown attribute drops the whole FETCH line, flags included.
- Chunked 1 MiB body fetches are **mandatory**. Set `max_response_size` low
  (~2 MiB plus headroom) so an accidental full fetch fails.
- Dovecot does **not** advertise OBJECTID. Test the EMAILID path against
  Stalwart or a capability override.
- Quirk: the first APPEND to a fresh user's INBOX reported an APPENDUID
  UIDVALIDITY one higher than SELECT (3 of 3 runs). Trust SELECT, not APPENDUID.
- Pin `imap-codec` exactly: imap-next accepts any newer `2.0.0-alpha`.
- Still to do: recorded Gmail/Fastmail/Yahoo/Exchange transcripts in
  `test/transcripts/`, TLS against Dovecot, long IDLE renewal.

## 2. 100k-row conversation list

Svelte 5 (runes) with a fixed-row-height in-house virtualizer. The id index
is delta + zigzag + LEB128 varint, and rows are paged 100 at a time from an
LRU cache. Measured in headless Chromium.

| Metric | Result |
|--------|--------|
| Jump to row 99,000, row has data (incl. 5–20 ms mock API) | ~31 ms median (AC 10 gate: < 300 ms) |
| Jump to 99,000, first rows in DOM | 4–6 ms |
| Scroll 25 px/frame, p95 frame / main-thread per frame | 16.7 ms / 1.8 ms |
| Id index, 100,000 ids | 199 KB (1.99 B/id) |
| App JS bundle | 59 kB / 23 kB gz |

**tanstack rejected:** it is a Svelte 4 store adapter, renders zero rows with
`bind:this` without a workaround, adds ~35 kB, and provides nothing we need.

**Follow-ups (M2)**

- WebKit/Safari is unmeasured. Run the Playwright jump test (hard gate) and the
  frame test (soft gate) in Chromium and WebKit CI.
- Measure real-device (GPU) frame times.
- Keep rows a fixed height. Cap the spacer near 16M px for Firefox/WebKit and
  switch to scaled scrolling above that.

## 3. OpenSSL

`cargo tree -i openssl` over all candidate crates:

- **`web-push` 0.11** is the only crate that pulls in OpenSSL, through `ece`'s
  default `backend-openssl` and the default isahc/curl client. Disabling
  default features doesn't remove it.
- **`webauthn-rs` 0.6.2 is OpenSSL-free.** It now uses RustCrypto.
- Two defaults pull in a C crypto stack and need switching off: `reqwest` 0.13's
  `rustls` feature pulls in `aws-lc-sys`, and `lettre`'s default features pull in
  `native-tls`, which links OpenSSL.

**Binary size** (probe with axum, reqwest, lettre, ammonia, mail-parser,
rusqlite bundled, webauthn-rs and web-push-native; stripped, LTO):

| Target | OpenSSL-free | Plus web-push with vendored OpenSSL | Delta |
|--------|--------------|-------------------------------------|-------|
| x86_64 musl (static-pie, no NEEDED) | 9.02 MB | 13.61 MB | +4.37 MiB (+51 %) |
| x86_64 glibc | 8.85 MB | 13.52 MB | +4.45 MiB (+53 %) |

**Decision:** use these crate settings:

- `webauthn-rs` 0.6
- `web-push-native` 0.5 (not `web-push`)
- `reqwest` with `default-features = false, features = ["rustls-no-provider", ...]`
  and the `rustls` ring provider installed at startup
- `lettre` with `default-features = false`, `tokio1-rustls`, `ring` and
  `webpki-roots` or `rustls-native-certs`
- `imap-next`, which uses `tokio-rustls` without a provider

`deny.toml` bans `openssl`, `openssl-sys`, `native-tls` and `aws-lc-sys`.

## 4. Replica + transport

Prototype in `desktop/spike/` (Tauri 2.12.7, wry 0.57.3). Its own Cargo
workspace, because the main CI job has no WebKitGTK libraries. The replica
logic and the protocol handler are a plain library (`cargo test`, no webview).
The `app` feature adds the Tauri shell. On start, the SPA runs eight checks
inside the webview, posts them to `/api/report`, and the process exits 0 only
if all pass (`SPIKE_SELFTEST=1`). Retired in M5: `desktop/src-tauri` runs the
same checks against the shipped SPA (`TAGMAIL_DESKTOP_SELFTEST=1`, CI in
`desktop.yml`); the code is in git history before that removal.

**Setup:** a synthetic mock server provides 1,000 messages in 334 threads as
an NDJSON snapshot. They load into an in-memory SQLite replica (`message`,
`message_label`, `intent`). One custom protocol, `app`, serves `/`, `/app.js`
and the `/api/*` routes from the same handler.

**Verified on WebKitGTK 2.54.3 (Linux, Xvfb), 3 release runs and 2 debug runs, all green:**

| Question | Result |
|----------|--------|
| SPA and `/api/*` on one origin | Window loads `app://localhost/`. `fetch('/api/threads')` works with no CORS. POST bodies reach the handler. |
| Render iframe height | `<iframe sandbox="allow-same-origin">` on `/api/messages/{id}/render`. The parent reads `contentDocument.documentElement.scrollHeight` = 1518 (1500 px body). |
| Iframe header CSP enforced | `img-src data:` blocks the `<img>` beacon (0 hits). The same page without the CSP header hits it (control). |
| Sandbox blocks scripts | With `allow-same-origin` and without `allow-scripts`, the inline-script beacon gets 0 hits. Without `sandbox`, it gets 1 hit (control). |
| SPA header CSP enforced | `script-src 'self'` makes `eval` throw. Tauri's injected IPC scripts still work under it. |
| IPC on a custom origin | `emit`/`listen` work with only `core:event:default`. Tauri treats a user-registered scheme as a local origin (`Webview::is_local_url`), so no remote-URL capability is needed. |
| Event push | Rust `emit` → JS `listen` latency: median ≤ 1 ms, max ≤ 2 ms (8 ticks, `Date.now()` diff). |
| `/api/threads` (50 threads) | First fetch 1–2 ms (release), 4 ms (debug). Median of 20 is 1 ms, the WebKit timer resolution. |
| Bootstrap 1,000 messages | NDJSON generate + parse + insert in one transaction: 2 ms (release), 12–16 ms (debug), in-memory DB. |
| Offline label op | Offline, the intent gets a UUID `op_id` and the overlay shows the label right away (`pending: true`). The server lacks the label. On reconnect, the flush emits `replica:flushed`. |
| Op-id dedupe | Flush with a dropped ack: the server applies once and the intent stays pending. The replay is reported as `Duplicate` and the server's applied count stays at 1. Then the overlay clears. |

**Streaming custom-protocol responses: not possible, by API.** The protocol
responder takes one whole body: `UriSchemeResponder::respond(Response<Cow<[u8]>>)`
in Tauri, the same in wry. So the server's SSE stream must end in Rust. Rust
forwards changes with `emit`, as the plan says. Large bodies, such as a 25 MB
raw message, are buffered whole per request. Serve them in ranges or from a
file path if this matters.

**Other findings**

- Tauri's `app.security.csp` applies only to Tauri's own asset protocol, never
  to a user protocol. The CSP the SPA gets is the header our handler sets, so
  set it in `api-core`, the same as on the web. The spike config has no
  Tauri CSP.
- Windows/Android map custom schemes to `http://app.localhost/`. The spike
  picks the window URL per platform. Same-origin paths are unchanged.
- `cargo deny` against the repo `deny.toml` passes: no OpenSSL, native-tls or
  aws-lc-sys on any target. There are 39 duplicate-version warnings (toml,
  syn, phf, windows-sys…) and ~360 crates. The gtk3-rs "unmaintained"
  advisories (RUSTSEC-2024-0411…0420) were withdrawn in 2026-08.
- Local build without apt: `nix-shell` with `webkitgtk_4_1 gtk3 libsoup_3`
  plus pkg-config, then run the binary with `LD_LIBRARY_PATH` built from
  `$NIX_LDFLAGS` under `xvfb-run`.

**CI:** in the "Desktop spike" workflow at commit 7bd3022, the in-webview
self-test passed on `macos-latest` (WKWebView) and `windows-latest`
(WebView2).

**Not verified**

- WKWebView and WebView2 by hand. The self-test there still runs with
  `continue-on-error`. Check by hand: header CSP on `http://app.localhost`
  (WebView2), iframe sandbox and height, POST bodies.
- Real HTTP to a server (the mock is in-process), an on-disk replica, the 90-day
  window, intent TTL and the 410 re-bootstrap. These belong to `crates/replica`
  in M5.
- Wayland, GPU and RSS of the webview.

**Verdict: go** for the plan's §3.12 transport on Linux: one custom-protocol
origin, all traffic through Rust, events via `emit`, intents deduped by
`op_id`. If WKWebView or WebView2 fail the iframe/CSP checks, the likely
fallback is to serve the SPA from Tauri's built-in `tauri://` asset protocol
and `/api/*` through `invoke`. The render iframe would then need a `<meta>`
CSP instead of a header.
