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

### Login

On first start the server logs a one-time setup token. Open the web app
and enter it with the owner password. The password is hashed with
argon2id (19 MiB, t=2); logins run one hash at a time and are rate
limited per client IP and globally. Passkeys can be added afterwards in
Settings. They need a domain name origin (`localhost` works; IP addresses
do not).

Every `/api` route except `/api/version`, setup, login and the desktop
pairing exchange requires a session or a device token. Browser sessions
are random tokens stored as SHA-256 hashes in `config.db` and sent as an
`HttpOnly; SameSite=Strict` cookie (`Secure` with an `https` origin).
They expire after 30 days without use or 90 days after login, and can be
listed and revoked in Settings.

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
Device tokens are hashed at rest and revocable.

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
warning at every start if it is inside. Back up `config.db` and the key
together. `cache.db` needs no backup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). To report a vulnerability, see
[SECURITY.md](SECURITY.md).

## License

Tagmail is licensed under the
[GNU Affero General Public License v3.0](LICENSE).
