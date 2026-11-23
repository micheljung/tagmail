# IMAP server behaviour

What Tagmail's design depends on, measured with `cargo xtask probe-server`
(plan §4 M3). Rows marked "not yet probed" have not been run: nothing here
is guessed.

## Probe results

| | Dovecot 2.4.5 | Stalwart v0.15.3 | Gmail | Fastmail |
|---|---|---|---|---|
| Probed | 2026-11-23, `test/compose.yml` | 2026-11-23, `test/compose.yml` | not yet probed | not yet probed |
| `PERMANENTFLAGS` has `\*` | yes | yes | not yet probed | not yet probed |
| Keyword visible in the same session | yes | yes | not yet probed | not yet probed |
| Keyword persists across sessions | yes | yes | not yet probed | not yet probed |
| Distinct keywords on one message (bound: 128) | all 128 stored | 98 stored, then silently dropped (no `NO`) | not yet probed | not yet probed |
| `OBJECTID` (EMAILID) | not advertised | advertised | not yet probed | not yet probed |
| `EMAILID` equal after `COPY` | n/a | yes | not yet probed | not yet probed |
| `RFC822.SIZE` differs after `COPY` | no (248 / 248) | no (248 / 248) | not yet probed | not yet probed |
| `COPYUID` reported | yes | yes | not yet probed | not yet probed |
| `CONDSTORE` / `QRESYNC` | yes / yes | yes / yes | not yet probed | not yet probed |
| `MOVE`, `UIDPLUS`, `IDLE` | yes | yes | not yet probed | not yet probed |
| Special-use mailboxes | `\Junk`, `\Drafts`, `\Sent`, `\Archive`, `\Trash` | `\Trash` ("Deleted Items"), `\Drafts`, `\Junk` ("Junk Mail"), `\Sent` ("Sent Items"); no `\Archive` | not yet probed | not yet probed |
| Sent saved by the server on SMTP submission | manual, see below | manual, see below | not yet probed | not yet probed |

Consequences for Tagmail:

- `RFC822.SIZE` is stable across copies on both servers, so a size-including
  hash identity is stable there. The probe is how a server where it is not
  gets noticed (the plan drops the size from the hash then).
- Stalwart silently drops custom keywords past about 98 distinct ones. The
  limit holds per account, not per message: after the probe used it up, a
  new keyword on another message in another mailbox was dropped as well, and
  deleting the scratch mailboxes does not free it. A label whose keyword the
  server dropped looks stored (`STORE` answers `OK`) and is gone on the next
  `FETCH FLAGS`. Label writes must be verified by reading the flags back, and
  the probe should be run against an account you can throw away: it uses
  the keyword budget of the account it runs on.
- Stalwart has no `\Archive` mailbox, so archiving there needs a mailbox the
  user designates.
- Stalwart v0.15.3 answers `UID SEARCH HEADER <name> <value>` with an empty
  result when the mailbox holds two or more messages (one message works).
  `SEARCH SUBJECT` is unaffected. Header-search lookups (Message-ID based
  Sent and outbox matching) therefore cannot rely on it there. The IMAP
  integration test `sync_window_fetch_and_uid_search` pins the behaviour so
  that it fails the day Stalwart fixes it. When a header search comes back
  empty from a mailbox that has messages, the engine reads the Message-ID
  header of the newest 500 UIDs instead, once per op batch and mailbox or
  per Sent watch; later lookups there read only UIDs that arrived since.
  That finds appended drafts and sent copies, which land at the tail; an op that has to find older mail
  again after a UIDVALIDITY change can still miss it on Stalwart, which
  advertises OBJECTID, so this only matters for messages without one.
- The engine probes each account on its first connection after a start
  until it has a result: it sets the hidden keyword `$tagmail_probe` on the
  newest INBOX message, reads it back on another connection and clears it
  (`account.keyword_probe`: `ok`, `dropped`, `unsupported`, or `skipped`
  while INBOX is empty). A read-back that fails or finds the message gone
  gives no result, and the keyword is cleared all the same. One keyword
  cannot reveal Stalwart's limit; a label op whose keyword the server did not
  keep marks the account `labels_degraded` until a later label is kept.

Raw output of the runs above (Dovecot: `user2`; Stalwart: `user1`):

```
Dovecot 2.4.5
  PERMANENTFLAGS: \Answered \Flagged \Deleted \Seen \Draft (\* arbitrary keywords: true)
  Keyword limit: attempted 128, stored 128, error null, limit_found false
  COPY: copyuid_reported true, size_original 248, size_copy 248, size_differs false

Stalwart v0.15.3
  PERMANENTFLAGS: \Deleted \Seen \Answered \Flagged \Draft (\* arbitrary keywords: true)
  Keyword limit: attempted 128, stored 98, error null, limit_found true
  COPY: copyuid_reported true, size_differs false, email_id_equal true
```

## Running the probe

```bash
TAGMAIL_PROBE_PASSWORD=... cargo xtask probe-server \
  --host imap.example.com --port 993 --security tls --user me@example.com
```

`--security` is `tls` (implicit TLS, port 993), `starttls` (port 143) or
`plain` (local test servers only). Without `TAGMAIL_PROBE_PASSWORD` the
password is prompted for; it is never printed or logged. `--json` prints the
report as JSON instead of text.

The probe creates two scratch mailboxes named `tagmail-probe-<unix time>-a`
and `-b`, appends two tiny messages, stores keywords on them, copies one
message and deletes both mailboxes again, also when a step fails. It never
touches existing mail. A failing step is reported as `{"error": ...}` in
its own section and does not stop the others.

Against the test servers (`cargo xtask dovecot up`, `cargo xtask stalwart up`):

```bash
TAGMAIL_PROBE_PASSWORD=pass2 cargo xtask probe-server --host 127.0.0.1 --port 10143 --security plain --user user2
TAGMAIL_PROBE_PASSWORD=pass1 cargo xtask probe-server --host 127.0.0.1 --port 10145 --security plain --user user1
```

With a remote container daemon, build `xtask` for `x86_64-unknown-linux-musl`
(see `test/README.md` for the environment) and run it in the server's
network namespace: `docker create --network container:<server id>
-e TAGMAIL_PROBE_PASSWORD=... busybox /x probe-server --host 127.0.0.1
--port 31143 ...`, `docker cp` the binary to `/x`, `docker start -a`.

### Gmail

Enable IMAP in Gmail's settings. Sign-in needs an app password (account with
2-step verification) or an OAuth token; the probe sends a plain password, so
use an app password:

```bash
TAGMAIL_PROBE_PASSWORD=<app password> cargo xtask probe-server \
  --host imap.gmail.com --port 993 --security tls --user you@gmail.com
```

Expect `X-GM-EXT-1` (`gmail_extensions: true`), and read the `\All` mailbox
in the special-use list. Gmail's label model makes the keyword rows
especially interesting: record whether a custom keyword stored in a scratch
folder appears as a label and survives the reconnect. Paste the human output
into the Gmail column above.

### Fastmail

Create an app password with IMAP access (Settings, Privacy & Security):

```bash
TAGMAIL_PROBE_PASSWORD=<app password> cargo xtask probe-server \
  --host imap.fastmail.com --port 993 --security tls --user you@fastmail.com
```

Fastmail advertises `OBJECTID`; check that `email_id_equal` holds after COPY.

## Not probeable over IMAP

**Sent saving.** Whether the server files a copy of a message sent through
its SMTP submission service in the Sent mailbox decides `server_saves_sent`
for the account. It cannot be probed without SMTP credentials and a real
send. Check it by hand: send one message from the account through its
submission server (port 587 or 465) without Tagmail appending anything, then
look in Sent. A copy means the server saves it (Gmail does; most others do
not).
