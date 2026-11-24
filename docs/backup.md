# Backup and restore

IMAP is the source of truth for your mail, including labels (they are
IMAP keywords). The server's own state is small; most of the data
directory is a cache that rebuilds itself from IMAP.

## What to back up

From the data directory (`--data-dir`):

| Path | Contents |
|------|----------|
| `config.db` | Owner password hash, passkeys, sessions, paired devices, accounts with their encrypted passwords, identities and signatures, label names and colors, queued changes not yet applied to IMAP, the outbox queue, image allowlist, push subscriptions and the encrypted Web Push key |
| `uploads/` | Attachments of drafts not yet sent |
| `outbox/` | Messages queued but not yet sent |

And, **separately**, the master key file (see
[install.md](install.md#master-key)). `config.db` is useless without it,
and anyone holding both can read your account passwords. Store the key
somewhere other than the data backups, e.g. in a password manager.

## What not to back up

| Path | Why |
|------|-----|
| `cache.db`, `cache.db-wal`, `cache.db-shm` | Messages, threads, search index: rebuilt from IMAP |
| `blobs/` | Message bodies and text: fetched again from IMAP |

## Consistent copies

The databases run in SQLite's WAL mode: a plain copy of `config.db`
taken while the server runs can miss recent changes, which live in
`config.db-wal` until a checkpoint. Either:

- stop the server, copy `config.db` (and `config.db-wal` if present),
  `uploads/` and `outbox/`, start it again; or
- take an online copy with the `sqlite3` shell, then copy `uploads/` and
  `outbox/`:

  ```sh
  sqlite3 /var/lib/tagmail/config.db ".backup '/backup/tagmail/config.db'"
  ```

The Docker image has no shell or `sqlite3`; stop the container and copy
the volume, as in [deploy/docker/README.md](../deploy/docker/README.md#backup).

## Restore

1. Stop the server.
2. Put `config.db`, `uploads/` and `outbox/` into an empty data directory.
3. Put the master key where the server looks for it.
4. Start the server.

With a key that does not belong to the `config.db`, the server refuses to
start: "the master key does not match config.db". Without the key the
backup cannot be used; set up a new server and add the accounts again.

## After a restore: rebuilding the cache

Without `cache.db` the server creates a new one and syncs every account
again, newest INBOX mail first. Labels come back from the IMAP keywords,
their names and colors from `config.db`. Changes that were queued when
the backup was taken are applied, or shown as failed if their messages
are gone.

Each new cache has a new identity, so clients notice:

- Open web app tabs reload their lists.
- Desktop apps download their offline copy again (minutes for very large
  mailboxes). Changes made offline and mail queued on the desktop are kept
  and sent afterwards.

A conversation that was joined only through a message since deleted from
the server may come back as two conversations.

To rebuild the cache on purpose, e.g. after a corrupted `cache.db`, stop
the server, delete `cache.db`, `cache.db-wal`, `cache.db-shm` and
`blobs/`, and start it.
