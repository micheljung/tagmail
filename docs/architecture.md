# Architecture

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
 │ (SvelteKit) │ (Tauri 2,     │ (SvelteKit, │
 │             │ local replica)│ Web Push)   │
 └─────────────┴───────────────┴─────────────┘
```

IMAP is the source of truth. The server keeps a cache of every account
and serves one SvelteKit single-page app, which also runs inside the
desktop app and as an installed PWA. The cache and search index are
disposable and rebuild from IMAP.

## Code layout

| Path | Contents |
|------|----------|
| `crates/core` | Domain types, email identity, label codec, search query parser, operations and their optimistic overlay |
| `crates/store` | SQLite databases and migrations, single writer and reader pool, full-text search, blob store, change log, credential encryption |
| `crates/imap` | IMAP session (on `imap-next`): capabilities, IDLE, CONDSTORE/QRESYNC, chunked fetches |
| `crates/engine` | Library: per-account supervisor, sync, threading, operation executor, labels, drafts, outbox and SMTP (lettre), MIME |
| `crates/api-core` | API handlers and DTOs shared by server and desktop, HTML sanitizer, signed URLs, event sessions |
| `crates/api-server` | Binary `tagmail-server` (axum): auth, passkeys, sessions, pairing, uploads, image proxy, Web Push, SSE, replica routes, static web app |
| `crates/replica` | The desktop's local copy: bootstrap, pull, offline writes, version check |
| `desktop/src-tauri` | Tauri 2 shell: `app://` protocol, tray, notifications, `mailto:`, keychain |
| `web` | SvelteKit app and service worker |
| `packages/api-types` | TypeScript API types generated from the Rust DTOs (`cargo xtask gen-ts-types`) |
| `xtask` | Development tasks: test servers, seeding, benchmarks, server probe |

## Data

The data directory holds two SQLite databases (WAL mode) and files:

| Path | Kind | Contents |
|------|------|----------|
| `config.db` | durable | Owner, passkeys, sessions, devices, accounts (encrypted passwords), identities, label registry, queued operations, outbox queue, push subscriptions |
| `uploads/`, `outbox/` | durable | Draft attachments, queued outgoing messages |
| `cache.db` | disposable | Mailboxes, messages, their locations (mailbox, UID, flags, keywords), threads, labels, full-text index, change log |
| `blobs/` | disposable | zstd-compressed raw messages (evicted by size, fetched again on demand) and plain text (at most 64 KB per message, feeds search) |

Deleting `cache.db` and `blobs/` makes the server sync everything again.
Durable data refers to mail only by keys IMAP keeps stable (email id,
mailbox name, special use, UIDVALIDITY), never by cache row ids, so
queued operations survive a rebuild. See [backup.md](backup.md).

Every write goes through one writer task per database; reads use a small
connection pool.

## Message identity

A message's `email_id` is the server's `EMAILID` when it advertises
`OBJECTID`, Gmail's `X-GM-MSGID` with `X-GM-EXT-1`, and otherwise a hash
of Message-ID, date, size, sender and subject. A message in several
mailboxes (or moved by another client) is one message with several
locations.

## Sync

Each account has a supervisor task with a connection budget (2 by
default): one connection IDLEs on INBOX, the other syncs, polls the other
mailboxes (every 2 minutes, `--poll-interval`) and runs queued work. With
a budget of one, the worker IDLEs between polls. Failures back off;
authentication failures back off longer, so a provider is never hammered
into a lockout.

- **Initial sync** lists mailboxes (special use first: INBOX, Sent,
  Archive/All, Drafts), then fetches headers newest first in windows of
  500 UIDs, committing each window. It resumes where it stopped. A modseq
  snapshot taken before the first window and a catch-up afterwards make
  sure changes during the sync are not lost.
- **Incremental sync** uses QRESYNC, else CONDSTORE plus a UID diff for
  expunges, else a full flag diff.
- **Bodies:** a background pass fetches text parts newest first, in
  chunks, and feeds the full-text index. Raw messages are fetched when
  opened.
- A UIDVALIDITY change resyncs that mailbox; messages are matched back by
  `email_id`. A message whose last location disappears is hidden and
  deleted only after every mailbox has been checked again, so a move seen
  as delete-then-add keeps its row.

## Labels as keywords

A label is an IMAP keyword on the message, so it is visible to other
clients and survives a cache rebuild. The name is encoded reversibly: the
UTF-8 bytes outside `[A-Za-z0-9_.-]` become `=HH`, behind the prefix
`$label_`. `Work Stuff/2026` is `$label_Work=20Stuff=2F2026`. Label
identity is the Unicode casefold of the name. Common keywords of other
clients (`$Forwarded`, `$Junk`, …) are not shown as labels.

- A message has a label if any of its locations has the keyword; a
  conversation has it if any message does. Adding or removing a label
  changes every location of every message of the conversation.
- New mail in a labelled conversation does not inherit the label.
- Display names, colors and order are kept in `config.db` and, where the
  server supports METADATA, in the server annotation
  `/private/vendor/tagmail/labels`.
- Renaming or deleting a label re-flags every message in a resumable job,
  one mailbox at a time.
- The engine checks whether a server keeps custom keywords and marks an
  account whose server dropped one. See [servers.md](servers.md).

Folders stay folders: `v` moves between mailboxes, `l` only sets labels.
Archive moves to `\Archive` (created as `Archive` if missing), or on
servers with `\All` (Gmail) only leaves INBOX.

## Operations

A user action becomes a queued operation in `config.db` that names the
`email_id`s the client saw and the mailbox by name, special use and
UIDVALIDITY. Until it runs, reads show its effect as an overlay over the
cache. The executor resolves mailbox and messages again when it runs, so
renames and cache rebuilds do not misapply it; an operation that cannot
be resolved fails visibly and the view returns to the server's state.
Client-generated operation ids make retries idempotent.

## Threading

Messages are grouped by Message-ID, In-Reply-To and References, plus a
fallback: same normalized subject (without `Re:`, `Fwd:`, `AW:`, list
tags…), dates within 14 days of each other and a shared participant.
Conversations are the connected components of these links, so the result
does not depend on arrival order. Threading is per account across all
mailboxes, so Sent replies join their conversation. Conversations merge
but never split; a conversation linked only through a since-deleted
message may come back as two after a cache rebuild.

## Search

Search runs on a contentless SQLite FTS5 index (subject, addresses,
body; `unicode61` with diacritics removed, phrase queries supported) plus
indexed filters. The query language is described in
[shortcuts.md](shortcuts.md#search-operators). Scripts without word
separators (Chinese, Japanese) are not split into words.

## Sending

Drafts live in the account's IMAP Drafts mailbox: a save APPENDs the new
version with `\Draft` and removes the previous one. The server keeps every
edit but saves to IMAP only after 30 seconds without edits, when the
attachments change, or when the client asks (e.g. on closing the compose
window). Attachments are uploaded first (at most 25 MiB each and 35 MiB
per message) into `uploads/`; uploads no draft uses are removed after a
day. Messages are streamed from files to SMTP and IMAP, never held in
memory whole.

Sending queues the message in a durable outbox (`config.db` and
`outbox/`). Each send carries a client-generated id, so a repeated request
never sends twice. Delivery is at least once: if the server stops after
the SMTP server accepted a message but before the outbox recorded it, the
message is looked up by Message-ID in Sent on restart and sent again only
if it is not there. Failed messages stay in the outbox with the SMTP error
until retried or cancelled.

The copy in Sent comes from the provider when it files sent mail itself
(preset for Gmail, otherwise detected on the first send by watching Sent
for ten seconds) and is APPENDed by Tagmail otherwise, never twice.
Replies carry `In-Reply-To` and `References`; the original is flagged
`\Answered` or `$Forwarded`.

## API, change log and events

The API is JSON over HTTP under `/api`. Long lists are not paged by
offset: `GET /api/views/{view}/index` returns the ordered conversation
ids of a view as a compact binary list, and rows are fetched by id. This
lets the client jump anywhere in 100,000 conversations.

Every cache change appends to a change log. Its cursor is
`<cache epoch>:<sequence>`; the epoch is random and new for every new
`cache.db`. `GET /api/events` is a server-sent event stream that tells
clients about changes (coalesced over 100 ms, heartbeat every 25 s,
resumable with `Last-Event-ID`). A cursor from another epoch or older
than the trimmed log answers `410 Gone`, and the client reloads.

## Desktop

The desktop app embeds the same SPA. It serves the app and `/api/*` from
one custom protocol origin (`app://localhost`), so the message iframe and
its CSP work as on the web. API calls are answered by `api-core` over a
local replica (`replica/cache.db`, `replica/config.db` and blobs in the
app's data directory):

- **Bootstrap** streams all conversation and message metadata plus the
  text of mail within the offline window (90 days by default, plus
  starred) from the server; afterwards the replica follows the change log
  over SSE.
- **Offline** the full list depth is available; mail in the window can be
  read and searched. Older mail needs a connection; while online, search
  and opening older mail go to the server.
- **Writes** made offline are queued in `replica-durable.db` with
  client-generated ids, which survives re-bootstraps, and are sent on
  reconnect. Conflicts show up under unsynced changes.
- A new server cache epoch or replica schema makes the replica bootstrap
  again; a server API it does not support stops syncing and asks for an
  update.

All server traffic goes through Rust with the device token; the web view
never sees it. The shell adds notifications, a tray icon with the unread
count, `mailto:` handling, autostart and close-to-tray.

## PWA and Web Push

The service worker caches the app shell, so the installed app opens
offline; mail is not stored in the browser. Web Push uses VAPID (RFC 8292)
and payload encryption (RFC 8291) implemented in the server. New unread
INBOX mail is batched (2 seconds, at most one push per device every 30
seconds) into one notification. See [pwa.md](pwa.md).

## Memory

The server targets an idle RSS of 64 MB with two accounts of 100,000
messages: few worker threads, small SQLite caches without mmap, and
jemalloc tuned to return memory (glibc builds; the musl builds use their
own allocator). Developer notes: [docs/dev/memory.md](dev/memory.md).
