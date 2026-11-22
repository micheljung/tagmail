#!/usr/bin/env python3
"""Check a seeded mailbox with an independent IMAP client (python imaplib).

Compares per-mailbox message, flag and keyword counts with the manifest that
`cargo xtask seed` writes, and checks the capabilities and SPECIAL-USE
mailboxes the Tagmail test harness relies on.
"""

import argparse
import imaplib
import json
import re
import sys

REQUIRED_CAPABILITIES = ["IMAP4REV1", "CONDSTORE", "QRESYNC", "METADATA", "SPECIAL-USE"]
SPECIAL_USE = {
    "Sent": "\\Sent",
    "Drafts": "\\Drafts",
    "Trash": "\\Trash",
    "Junk": "\\Junk",
    "Archive": "\\Archive",
}
SEARCHES = {"seen": "SEEN", "answered": "ANSWERED", "flagged": "FLAGGED"}


def ok(response):
    typ, data = response
    if typ != "OK":
        raise RuntimeError(f"IMAP command failed: {typ} {data}")
    return data


def quote(mailbox):
    return '"' + mailbox.replace("\\", "\\\\").replace('"', '\\"') + '"'


def search_count(imap, *criteria):
    data = ok(imap.uid("SEARCH", *criteria))
    return len(data[0].split()) if data and data[0] else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port", type=int, default=10143)
    parser.add_argument("--user", default="user1")
    parser.add_argument("--password", default="pass1")
    parser.add_argument("--manifest", required=True, help="tagmail-seed.json, or - for stdin")
    parser.add_argument("--all-mailbox", help="mailbox expected to carry \\All (Gmail mode)")
    args = parser.parse_args()

    with open(sys.stdin.fileno() if args.manifest == "-" else args.manifest) as f:
        manifest = json.load(f)

    errors = []

    def expect(what, actual, expected):
        status = "ok" if actual == expected else "MISMATCH"
        print(f"{status:8} {what}: {actual} (expected {expected})")
        if actual != expected:
            errors.append(what)

    imap = imaplib.IMAP4(args.host, args.port)
    ok(imap.login(args.user, args.password))
    capabilities = set(ok(imap.capability())[0].decode().upper().split())
    for cap in REQUIRED_CAPABILITIES:
        expect(f"capability {cap}", cap in capabilities, True)
    print(f"info     OBJECTID advertised: {'OBJECTID' in capabilities}")

    attributes = {}
    for line in ok(imap.list('""', "*")):
        match = re.match(rb'\((?P<attrs>[^)]*)\) (?:"[^"]*"|NIL) (?P<name>.+)$', line)
        name = match["name"].decode().strip('"')
        attributes[name] = match["attrs"].decode().split()
    special_use = dict(SPECIAL_USE)
    if args.all_mailbox:
        special_use[args.all_mailbox] = "\\All"
    for mailbox, attr in special_use.items():
        expect(f"{mailbox} has {attr}", attr in attributes.get(mailbox, []), True)

    total = {"messages": 0, "keywords": {}}
    for mailbox, expected in sorted(manifest["mailboxes"].items()):
        status = ok(imap.status(quote(mailbox), "(MESSAGES UNSEEN)"))[0].decode()
        counts = dict(re.findall(r"(MESSAGES|UNSEEN) (\d+)", status))
        expect(f"{mailbox} messages", int(counts["MESSAGES"]), expected["messages"])
        expect(f"{mailbox} unseen", int(counts["UNSEEN"]), expected["messages"] - expected["seen"])
        total["messages"] += expected["messages"]

        ok(imap.select(quote(mailbox), readonly=True))
        for key, criterion in SEARCHES.items():
            expect(f"{mailbox} {criterion}", search_count(imap, criterion), expected[key])
        for keyword, count in expected["keywords"].items():
            expect(f"{mailbox} KEYWORD {keyword}", search_count(imap, "KEYWORD", keyword), count)
            total["keywords"][keyword] = total["keywords"].get(keyword, 0) + count
        with_id = expected["messages"] - expected["without_message_id"]
        expect(
            f"{mailbox} with Message-ID",
            search_count(imap, "HEADER", "Message-ID", "@"),
            with_id,
        )
        ok(imap.close())

    if args.all_mailbox:
        ok(imap.select(quote(args.all_mailbox), readonly=True))
        expect(f"{args.all_mailbox} messages", search_count(imap, "ALL"), total["messages"])
        for keyword, count in total["keywords"].items():
            expect(f"{args.all_mailbox} KEYWORD {keyword}", search_count(imap, "KEYWORD", keyword), count)
        ok(imap.close())

    imap.logout()
    if errors:
        print(f"FAILED: {len(errors)} mismatches", file=sys.stderr)
        return 1
    print(f"PASSED: {manifest['count']} messages (seed {manifest['seed']}) readable over IMAP")
    return 0


if __name__ == "__main__":
    sys.exit(main())
