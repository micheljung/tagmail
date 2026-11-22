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

The server has no login yet (planned for milestone M2). Until then:

- It serves plain HTTP and listens on `127.0.0.1:7878` by default. It
  refuses any non-loopback `--listen` address unless you pass
  `--insecure-no-auth`. Use that flag only for development.
- It does not terminate TLS. To expose it, put a reverse proxy that
  handles TLS (Caddy, nginx, Traefik) in front of the loopback port.

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
