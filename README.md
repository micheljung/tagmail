# Tagmail

Tagmail is an open-source, self-hosted, single-user email client with a
Gmail-like interface for any IMAP server. A small Rust sync server keeps a
local cache of your mailboxes and serves one TypeScript UI as a web app, a
Tauri desktop app and an installable PWA.

**Status: pre-alpha.** Nothing works yet; the repository currently contains
the project skeleton only. Do not use it for real mail.

## Planned features

- Threaded conversations across folders, including Sent
- Labels stored as IMAP keywords, so they survive a cache wipe and are visible
  to other IMAP clients
- Archive, star, read/unread and Gmail keyboard shortcuts
- Operator search (`from:`, `to:`, `subject:`, `label:`, `in:`, `is:unread`,
  `is:starred`, `has:attachment`, `before:`, `after:`, quotes, negation) over
  a local full-text index
- Multiple accounts with a unified inbox and per-account identities and
  signatures
- Compose, reply, forward and attachments, with drafts stored in IMAP Drafts
- Sanitized HTML rendering with remote images blocked by default
- Desktop app with offline reading, search and compose, native notifications
  and a `mailto:` handler
- PWA with responsive layout and Web Push
- Passkey or password login; account credentials encrypted at rest
- Docker image and NixOS module

Out of scope for v1: native mobile apps, filters, snooze, undo send,
multi-user, JMAP, Gmail API labels and OAuth2 providers. Outlook.com and
Microsoft 365 are unsupported because they disabled basic authentication.

## Architecture

```
 IMAP / SMTP servers
        │
 ┌──────┴─────────────────────────────┐
 │ tagmail-server (Rust)              │
 │  engine: sync, threading, labels   │
 │  store:  SQLite + FTS5, blob cache │
 │  api:    HTTP + server-sent events │
 └──────┬─────────────────────────────┘
        │
 ┌──────┴──────┬───────────────┬─────────────┐
 │ Web app     │ Desktop       │ PWA         │
 │ (SvelteKit) │ (Tauri 2,     │ (SvelteKit) │
 │             │ local replica)│             │
 └─────────────┴───────────────┴─────────────┘
```

IMAP is the source of truth. The SQLite cache and search index are
disposable and can be rebuilt from the server at any time.

| Path | Contents |
|------|----------|
| `crates/core` | Domain types, label codec, search query parser |
| `crates/store` | SQLite schemas, migrations, full-text search, blobs, credential encryption |
| `crates/imap` | IMAP session abstraction and implementation |
| `crates/engine` | Account sync, threading, operations, SMTP, MIME |
| `crates/api-core` | API handlers shared by server and desktop |
| `crates/api-server` | HTTP server, binary `tagmail-server` |
| `crates/replica` | Desktop offline replica |
| `xtask` | Development tasks (`cargo xtask`) |
| `web` | SvelteKit single-page app |
| `packages/api-types` | Generated TypeScript API types |

## Running the server

The server serves plain HTTP on `127.0.0.1:7878` by default and does not
terminate TLS. To expose it, put a TLS-terminating reverse proxy (Caddy,
nginx, Traefik) in front and tell the server its public origin:

```sh
tagmail-server --data-dir /var/lib/tagmail --public-origin https://mail.example.com \
  --trusted-proxy 127.0.0.1
```

- A non-loopback `--listen` address needs an `https` `--public-origin`.
  `--insecure-http` allows plain HTTP for development only.
- `--trusted-proxy` (address or CIDR, repeatable) lists proxies whose
  `X-Forwarded-For` is used for login rate limits. Other peers' headers
  are ignored.
- `--web-dir` serves the built web app (`web/build`) at `/`. By default a
  `web` directory next to the binary is used if present.
- `--insecure-plain-imap` is for local test servers only: it connects to
  IMAP and SMTP without TLS for **every** account on the server, not just
  one, and sends their passwords in cleartext.

### Login

On first start the server logs a one-time setup token. Open the web app
and enter it with the owner password. The password is hashed with
argon2id (19 MiB, t=2); logins run one hash at a time and are rate
limited per client IP (10 attempts, then one per 10 s). A global limit
(600, then ten per second) is only a backstop: an attacker with hundreds
of addresses can still delay password logins until it refills. Passkeys can be added afterwards in
Settings. They need a domain name origin (`localhost` works; IP addresses
do not).

Every `/api` route except `/api/version`, setup, login and the desktop
pairing exchange requires a session or a device token. Browser sessions
are random tokens stored as SHA-256 hashes in `config.db` and sent as an
`HttpOnly; SameSite=Strict` cookie (`Secure` with an `https` origin).
They expire after 30 days without use or 90 days after login, and can be
listed and revoked in Settings. Open event streams end when their session
or device does.

CSRF protection: besides `SameSite=Strict`, every state-changing request
must send `X-Requested-With: tagmail`. Browsers cannot add that header
cross-origin without a CORS preflight, and the server allows no CORS. A
present `Origin` must match the public origin, and `Sec-Fetch-Site` must
be `same-origin` or `none`. Behind a web dev server that proxies `/api`,
pass the dev server's origin as `--public-origin`.

The desktop app pairs from Settings → Devices: the web app shows an
8-character code (and QR code) valid for 5 minutes. The desktop app
exchanges it once for a device token, which only its Rust side sends as
`Authorization: Bearer`. Requests from browsers cannot use device tokens.
Device tokens are hashed at rest and revocable, and cannot pair devices
or manage sessions, devices or passkeys.

### Sending

Drafts live in the account's IMAP Drafts mailbox: each save APPENDs the
new version with `\Draft` and expunges the previous one, two seconds
after the last edit. Attachments are uploaded to the server first (at
most 25 MB each and 35 MB per message) and kept in `uploads/` in the
data directory; uploads no draft uses are removed after a day.

Sending queues the message in a durable outbox in `config.db`. Each send
request carries a client-generated id, so a repeated request never sends
twice. Delivery is **at least once**: the outbox marks a message as
sending before the SMTP transaction; if the server stops after the SMTP
server accepted the message but before the outbox recorded it, the
message is looked up by its Message-ID in Sent on restart and sent again
only if it is not there. A provider that does not file sent mail in Sent
itself can therefore receive such a message twice. Failed messages stay
in the outbox with the SMTP error until retried or cancelled.

After sending, the copy in Sent comes from the provider when it files
sent mail itself (preset for Gmail, otherwise detected on the first send
by watching Sent for ten seconds) and is APPENDed by Tagmail otherwise,
never twice. Replies carry `In-Reply-To` and `References`, so the Sent
copy joins the conversation.

### Master key

```sh
tagmail-server --data-dir /var/lib/tagmail --master-key /etc/tagmail/master.key --init-key
```

IMAP and SMTP passwords are encrypted in `config.db` with a master key.
The server looks for the key in this order:

1. `--master-key`
2. `$CREDENTIALS_DIRECTORY/master.key` (systemd `LoadCredential=`)
3. `$TAGMAIL_MASTER_KEY_FILE` (Docker secret)
4. `/etc/tagmail/master.key`

Without a key file it refuses to start. `--init-key` generates one with
mode 0600. Keep the key outside the data directory: the server logs a
warning at every start if it is inside. Back up `config.db`, `uploads/`
(attachments of drafts not yet sent) and the key together. `cache.db` and
`blobs/` need no backup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). To report a vulnerability, see
[SECURITY.md](SECURITY.md).

## License

Tagmail is licensed under the
[GNU Affero General Public License v3.0](LICENSE).
