-- Never wiped: survives every re-bootstrap of the disposable replica, so it
-- holds only IMAP-stable keys (server account ids, email ids, mailbox refs).

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE intent (
    op_id       TEXT    PRIMARY KEY,
    seq         INTEGER NOT NULL UNIQUE,
    kind        TEXT    NOT NULL,
    account_id  INTEGER NOT NULL,
    email_ids   TEXT    NOT NULL,
    label       TEXT,
    mailbox_ref TEXT,
    created_at  INTEGER NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN ('pending', 'sent', 'done', 'failed')),
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     INTEGER,
    error       TEXT
) WITHOUT ROWID;

CREATE INDEX intent_state ON intent (state, next_at);

CREATE TABLE draft (
    client_id       TEXT    PRIMARY KEY,
    server_draft_id INTEGER,
    account_id      INTEGER NOT NULL,
    identity_id     INTEGER,
    content         TEXT    NOT NULL,
    updated_at      INTEGER NOT NULL,
    synced_at       INTEGER
) WITHOUT ROWID;

CREATE TABLE draft_attachment (
    id               INTEGER PRIMARY KEY,
    client_id        TEXT    NOT NULL REFERENCES draft (client_id) ON DELETE CASCADE,
    name             TEXT    NOT NULL,
    content_type     TEXT    NOT NULL,
    size             INTEGER NOT NULL,
    path             TEXT    NOT NULL,
    server_upload_id INTEGER
);

CREATE INDEX draft_attachment_draft ON draft_attachment (client_id);

CREATE TABLE outbox (
    op_id      TEXT    PRIMARY KEY,
    client_id  TEXT    NOT NULL REFERENCES draft (client_id),
    created_at INTEGER NOT NULL,
    state      TEXT    NOT NULL
                       CHECK (state IN ('queued', 'uploading', 'submitted', 'sent', 'failed')),
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_at    INTEGER,
    error      TEXT
) WITHOUT ROWID;
