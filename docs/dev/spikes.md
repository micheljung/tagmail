# M0 spikes

Results of the three M0 spikes (2026-11, Rust 1.100). Numbers are from single
developer-machine runs; treat them as orders of magnitude.

| # | Spike | Decision |
|---|-------|----------|
| 1 | IMAP library | **Go:** `imap-next` 0.3.4 on `imap-codec` `=2.0.0-alpha.10`, behind the `ImapSession` trait; `async-imap` stays the fallback |
| 2 | 100k-row conversation list | **Go:** Svelte 5 + in-house virtualizer; `@tanstack/svelte-virtual` rejected |
| 3 | OpenSSL | **Go:** OpenSSL-free, musl static; enforced in `deny.toml` |

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
