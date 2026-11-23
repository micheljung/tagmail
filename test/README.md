# Test servers

`compose.yml` runs the IMAP and SMTP servers used by integration and
end-to-end tests. Start and stop them with `cargo xtask`:

```bash
cargo xtask dovecot up [--gmail]   # Dovecot (+ Gmail-mode variant) and Mailpit
cargo xtask stalwart up            # Stalwart and Mailpit
cargo xtask dovecot down           # removes the containers and their mail
cargo xtask stalwart down
```

| Service | Port | Image | Notes |
|---------|------|-------|-------|
| `dovecot` | IMAP 10143 | `dovecot/dovecot:2.4.5` | CONDSTORE, QRESYNC, METADATA, SPECIAL-USE (`Sent`, `Drafts`, `Trash`, `Junk`, `Archive`). Does **not** advertise OBJECTID. |
| `dovecot-gmail` | IMAP 10144 | same | Adds a virtual `[Gmail]/All` mailbox with `\All` for Gmail-mode tests |
| `stalwart` | IMAP 10145, HTTP 10080 | `stalwartlabs/stalwart:v0.15.3` | Advertises OBJECTID. Admin `admin` / `admin` |
| `mailpit` | SMTP 10025, UI and API 10825 | `axllent/mailpit:v1.27` | SMTP sink; accepts any login, also without TLS (test stack only) |

Every server has the users `user1` / `pass1` and `user2` / `pass2`, with
cleartext login over plain IMAP. Dovecot also has `engine1` … `engine9` and
`ops1` … `ops4` / `engine`, one per engine integration test; Stalwart has
`ops4` / `engine` for the engine smoke test. These are test-only credentials.
`tagmail-server` reaches them with `--insecure-plain-imap`, which turns TLS
off for **all** of its accounts: never use it on a server that also syncs
real mail.

## Seeding

```bash
cargo xtask seed --count 1000 --avg-kb 8 --seed 1 --out target/seed
cargo xtask dovecot load --from target/seed [--user user1] [--gmail]
python3 test/smoke/check_seed.py --manifest target/seed/tagmail-seed.json
```

`seed` writes a deterministic Maildir++ tree (`INBOX`, `.Sent`, `.Archive`)
plus `tagmail-seed.json`, which holds the expected per-mailbox counts.
`--huge-mb 25` also appends one unread 25 MB message (HTML body plus one
attachment, subject `tagmail-bench-huge`) as the newest in INBOX, for the
RSS gate.
`check_seed.py` compares those counts with what an independent IMAP client
sees.

`load` streams the Maildir into the container with `docker cp` instead of a
bind mount, so it also works when the Docker daemon runs on another host. If
the published ports are not reachable either, run the check inside the
container's network namespace:

```bash
id=$(docker compose -f test/compose.yml ps -q dovecot)
docker run --rm -i --network container:$id python:3.13-alpine \
  python3 -c "$(cat test/smoke/check_seed.py)" --port 31143 --manifest - \
  < target/seed/tagmail-seed.json
```

## IMAP integration tests

`crates/imap/tests/dovecot.rs` runs against the `dovecot` service as
`user2`, each test in its own fresh mailbox:

```bash
cargo xtask dovecot up
cargo test -p tagmail-imap --features dovecot-tests --test dovecot
```

`TAGMAIL_IMAP_ADDR` (default `127.0.0.1:10143`) points them elsewhere. With
a remote Docker daemon, build a static test binary and run it in the
container's network namespace:

```bash
CC_x86_64_unknown_linux_musl=gcc cargo test -p tagmail-imap \
  --features dovecot-tests --test dovecot --target x86_64-unknown-linux-musl --no-run
id=$(docker compose -f test/compose.yml ps -q dovecot)
c=$(docker create --network container:$id -e TAGMAIL_IMAP_ADDR=127.0.0.1:31143 busybox /it)
docker cp <test binary> $c:/it && docker start $c && docker wait $c
docker logs $c; docker rm $c
```

The same file runs against Stalwart, which advertises OBJECTID, with
`TAGMAIL_IMAP_SERVER=stalwart TAGMAIL_IMAP_ADDR=127.0.0.1:10145` after
`cargo xtask stalwart up` (the `imap-stalwart` job in `e2e.yml`). The tests
derive what they expect from the server's capabilities.
`cargo xtask probe-server` reports how a server handles keywords, object ids
and COPY; see [`docs/servers.md`](../docs/servers.md). It consumes
Stalwart's per-account keyword budget, so run it as `user1` there (the tests
use `user2`).

## Engine integration tests

`crates/engine/tests/dovecot.rs` runs the sync engine against the
`dovecot` service; each test empties and uses its own `engineN` user.
`labels.rs` and `rebuild.rs` (ops, labels, AC 5 and AC 6) use the `opsN`
users and the Gmail-mode `dovecot-gmail` service (`cargo xtask dovecot up
--gmail`), whose virtual `[Gmail]/All` saves into `Archive` so a MOVE to
`\All` leaves INBOX as on Gmail; `TAGMAIL_IMAP_GMAIL_ADDR` (default
`127.0.0.1:10144`) points them elsewhere.
`labels.rs` also runs against Stalwart with
`TAGMAIL_IMAP_SERVER=stalwart TAGMAIL_IMAP_ADDR=127.0.0.1:10145` after
`cargo xtask stalwart up` (the `imap-stalwart` job): there only
`label_round_trip_and_draft_uid_without_uidplus` runs (as `ops4`), the
others need Dovecot and say so. It hides UIDPLUS, so the draft's UID must
come from the Message-ID lookup, which Stalwart's `UID SEARCH HEADER`
cannot answer once Drafts holds two messages.
`dovecot_seed.rs` checks that the newest INBOX message of the seed loaded
into `user1` is cached within 30 s:

```bash
cargo xtask dovecot up
cargo xtask seed --count 100000 --avg-kb 8 --seed 1 --out target/seed
cargo xtask dovecot load --from target/seed
TAGMAIL_SEED_MANIFEST=target/seed/tagmail-seed.json cargo test -p tagmail-engine \
  --features dovecot-tests --test dovecot --test dovecot_seed -- --nocapture
```

For the static musl build, the bundled SQLite needs
`LIBSQLITE3_FLAGS=-DSQLITE_DISABLE_LFS` (glibc headers otherwise reference
`open64` and friends, which musl does not export). Pass the manifest into the
container with `-e TAGMAIL_SEED_MANIFEST=/seed.json` after copying it there.

## Send integration tests

`crates/engine/tests/send.rs` saves drafts to Dovecot's Drafts as
`engine8` and `engine9`, submits through Mailpit and checks what Mailpit
received (its HTTP API) and what Sent holds:

```bash
cargo xtask dovecot up
cargo test -p tagmail-engine --features mailpit-tests --test send
```

`TAGMAIL_SMTP_ADDR` (default `127.0.0.1:10025`) and `TAGMAIL_MAILPIT_API`
(default `127.0.0.1:10825`) point elsewhere. With a remote Docker daemon,
run the static binary in the Dovecot container's network namespace as
above, with `TAGMAIL_SMTP_ADDR=mailpit:1025` and
`TAGMAIL_MAILPIT_API=mailpit:8025`; add `--log-driver k8s-file` to
`docker create` if `docker logs` shows nothing (Podman's journald driver).

## RSS gate

`cargo xtask bench-rss` checks AC 9 on the full scenario: it needs the seed
with `--seed 1 --huge-mb 25` in `user1` and `--seed 2` in `user2`. It starts
a release `tagmail-server`, adds both accounts (`--account user:password`,
repeated; default `user1:pass1 user2:pass2`), waits until every account is
live with the text pass done, then opens one `/api/events` stream that
stays connected, runs one search (`meeting`: view index plus the first
page of rows) and opens and renders the `tagmail-bench-huge` message. It
then idles 5 minutes and fails if the highest VmRSS of the last 60 s
exceeds 64 MB (decimal, 64 × 10^6 bytes = 61.04 MiB, as in the spec) or the
event stream closed. It writes `target/bench-rss.json` (with `accounts`,
`sseConnected`, `searchMs`, `renderMs`) and, under GitHub Actions, the job
summary (`--idle-secs`, `--window-secs`, `--limit-mb`, `--out` adjust it).

```bash
cargo xtask seed --count 100000 --avg-kb 8 --seed 1 --huge-mb 25 --out target/seed
cargo xtask seed --count 100000 --avg-kb 8 --seed 2 --out target/seed2
cargo xtask dovecot load --from target/seed --user user1
cargo xtask dovecot load --from target/seed2 --user user2
cargo xtask bench-rss
```

With a remote Docker daemon, `--in-dovecot-network` builds static binaries
and runs the benchmark in the Dovecot container's network namespace. These
binaries use musl's allocator, so their RSS is not comparable with the
glibc build that `bench.yml` measures.

## Search gate

`cargo xtask bench-search` checks AC 11: it needs the seed with `--seed 1`
in `user1` and `--seed 2` in `user2`, syncs both accounts to the end of
the text pass, restarts the server, runs the 30 named queries of
`xtask/src/bench_search.rs` once (the cold run: fresh process caches, but
the OS page cache stays warm; reported, not gated) and then
`--warm-runs` more times, and fails if the warm p95 reaches `--limit-ms`
(500). A query's latency is `GET /api/views/search:<q>/index` plus the
first page of `/api/threads`. It writes `target/bench-search.json` and,
under GitHub Actions, the job summary. `--in-dovecot-network` works as for
`bench-rss`.

```bash
cargo xtask seed --count 100000 --avg-kb 8 --seed 2 --out target/seed2
cargo xtask dovecot load --from target/seed2 --user user2
cargo xtask bench-search
```
