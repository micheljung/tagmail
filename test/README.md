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
| `mailpit` | SMTP 10025, UI 10825 | `axllent/mailpit:v1.27` | SMTP sink |

Every server has the users `user1` / `pass1` and `user2` / `pass2`, with
cleartext login over plain IMAP. These are test-only credentials.

## Seeding

```bash
cargo xtask seed --count 1000 --avg-kb 8 --seed 1 --out target/seed
cargo xtask dovecot load --from target/seed [--user user1] [--gmail]
python3 test/smoke/check_seed.py --manifest target/seed/tagmail-seed.json
```

`seed` writes a deterministic Maildir++ tree (`INBOX`, `.Sent`, `.Archive`)
plus `tagmail-seed.json`, which holds the expected per-mailbox counts.
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
