-- Compose and send (plan §3.2, §3.5). A draft's IMAP copy is replaced on
-- every save; msgid_hdr (its Message-ID) stays the same across versions and
-- becomes the sent message's. content: JSON of the fields the client edits
-- plus the reply references, all IMAP-stable (Message-ID headers, email
-- ids).
CREATE TABLE draft (
    id                INTEGER PRIMARY KEY,
    account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    identity_id       INTEGER REFERENCES identity (id) ON DELETE SET NULL,
    msgid_hdr         TEXT    NOT NULL,
    content           TEXT    NOT NULL,
    version           INTEGER NOT NULL DEFAULT 1,
    saved_version     INTEGER NOT NULL DEFAULT 0,
    saved_mailbox     TEXT,
    saved_uidvalidity INTEGER,
    saved_uid         INTEGER,
    save_error        TEXT,
    save_attempts     INTEGER NOT NULL DEFAULT 0,
    next_save_at      INTEGER,
    -- Queued in the outbox: frozen until sent or cancelled.
    queued            INTEGER NOT NULL DEFAULT 0 CHECK (queued IN (0, 1)),
    created_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((saved_mailbox IS NULL) = (saved_uidvalidity IS NULL)),
    CHECK ((saved_mailbox IS NULL) = (saved_uid IS NULL))
);

CREATE INDEX draft_unsaved ON draft (saved_version, version);

-- Saved draft copies still to expunge from IMAP: replaced across accounts,
-- deleted, or sent.
CREATE TABLE draft_purge (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    mailbox     TEXT    NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid         INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     INTEGER
);

-- Attachment files under <data>/uploads/<id>; draft_id NULL = orphan,
-- removed after a grace period.
CREATE TABLE upload (
    id           INTEGER PRIMARY KEY,
    draft_id     INTEGER REFERENCES draft (id) ON DELETE SET NULL,
    name         TEXT    NOT NULL,
    content_type TEXT    NOT NULL,
    size         INTEGER NOT NULL,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX upload_draft ON upload (draft_id);

-- Replaces V1's unused outbox: a numeric id for the API and change events.
-- recipients: JSON array of {name, email} (To, Cc and Bcc; the envelope).
-- state 'sent': accepted by SMTP; copied_at is set once Sent has it, and
-- the row stays visible for a while after that.
DROP TABLE outbox;

CREATE TABLE outbox (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id             TEXT    NOT NULL UNIQUE,
    account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    identity_id       INTEGER REFERENCES identity (id) ON DELETE SET NULL,
    draft_id          INTEGER REFERENCES draft (id) ON DELETE SET NULL,
    mime_blob         BLOB    NOT NULL,
    msgid_hdr         TEXT    NOT NULL,
    sender            TEXT    NOT NULL,
    recipients        TEXT    NOT NULL,
    subject           TEXT    NOT NULL,
    reply_to_email_id TEXT,
    state             TEXT    NOT NULL DEFAULT 'queued'
                              CHECK (state IN ('queued', 'sending', 'sent', 'failed')),
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_at           INTEGER,
    error             TEXT,
    created_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    sent_at           INTEGER,
    copied_at         INTEGER
);

CREATE INDEX outbox_due ON outbox (state, next_at);
