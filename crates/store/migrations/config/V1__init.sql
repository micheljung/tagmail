-- Durable: holds only what IMAP cannot. Per the durable-reference rule,
-- rows reference accounts, mailboxes by (name, special_use, uidvalidity) and
-- messages by email_id, never cache.db ids or change_log seqs.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE owner (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE passkey (
    id         INTEGER PRIMARY KEY,
    cred_id    BLOB    NOT NULL UNIQUE,
    public_key BLOB    NOT NULL,
    sign_count INTEGER NOT NULL DEFAULT 0,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE session (
    id_hash      BLOB    PRIMARY KEY,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at   INTEGER NOT NULL,
    user_agent   TEXT
) WITHOUT ROWID;

CREATE INDEX session_expires ON session (expires_at);

CREATE TABLE device (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    token_hash   BLOB    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at INTEGER,
    revoked_at   INTEGER
);

CREATE TABLE pairing_code (
    code_hash  BLOB    PRIMARY KEY,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE account (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    imap_host          TEXT    NOT NULL,
    imap_port          INTEGER NOT NULL,
    imap_tls           TEXT    NOT NULL CHECK (imap_tls IN ('tls', 'starttls')),
    smtp_host          TEXT    NOT NULL,
    smtp_port          INTEGER NOT NULL,
    smtp_tls           TEXT    NOT NULL CHECK (smtp_tls IN ('tls', 'starttls')),
    username           TEXT    NOT NULL,
    secret_ct          BLOB    NOT NULL,
    secret_nonce       BLOB    NOT NULL,
    max_connections    INTEGER NOT NULL DEFAULT 2 CHECK (max_connections BETWEEN 1 AND 4),
    server_saves_sent  INTEGER CHECK (server_saves_sent IN (0, 1)),
    archive_mode       TEXT    CHECK (archive_mode IN ('move_archive', 'move_all')),
    status             TEXT    NOT NULL DEFAULT 'active',
    auth_backoff_until INTEGER,
    created_at         INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE identity (
    id             INTEGER PRIMARY KEY,
    account_id     INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    address        TEXT    NOT NULL,
    display_name   TEXT,
    signature_html TEXT,
    is_default     INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1))
);

CREATE INDEX identity_account ON identity (account_id);
CREATE UNIQUE INDEX identity_one_default ON identity (account_id) WHERE is_default = 1;

CREATE TABLE push_subscription (
    id         INTEGER PRIMARY KEY,
    endpoint   TEXT    NOT NULL UNIQUE,
    p256dh     BLOB    NOT NULL,
    auth       BLOB    NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- email_ids: JSON array of the email ids the client saw.
CREATE TABLE pending_op (
    op_id               TEXT    PRIMARY KEY,
    account_id          INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    kind                TEXT    NOT NULL,
    email_ids           TEXT    NOT NULL DEFAULT '[]',
    mailbox_name        TEXT,
    mailbox_special_use TEXT,
    mailbox_uidvalidity INTEGER,
    payload             TEXT,
    state               TEXT    NOT NULL DEFAULT 'pending'
                                CHECK (state IN ('pending', 'running', 'done', 'failed')),
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_at             INTEGER,
    error               TEXT,
    created_at          INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((mailbox_name IS NULL) = (mailbox_uidvalidity IS NULL)),
    CHECK (mailbox_special_use IS NULL OR mailbox_name IS NOT NULL)
) WITHOUT ROWID;

CREATE INDEX pending_op_due ON pending_op (state, next_at);

CREATE TABLE outbox (
    op_id             TEXT    PRIMARY KEY,
    account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    identity_id       INTEGER REFERENCES identity (id) ON DELETE SET NULL,
    mime_blob         BLOB    NOT NULL,
    reply_to_email_id TEXT,
    state             TEXT    NOT NULL DEFAULT 'queued'
                              CHECK (state IN ('queued', 'sending', 'sent', 'failed')),
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_at           INTEGER,
    error             TEXT,
    created_at        INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;

CREATE INDEX outbox_due ON outbox (state, next_at);

-- progress: JSON array of the mailbox names already processed.
CREATE TABLE label_job (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind IN ('rename', 'delete')),
    from_kw    TEXT    NOT NULL,
    to_kw      TEXT,
    progress   TEXT    NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((kind = 'rename') = (to_kw IS NOT NULL))
);

CREATE TABLE op_seen (
    op_id      TEXT    PRIMARY KEY,
    account_id INTEGER NOT NULL,
    seen_at    INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;

CREATE INDEX op_seen_age ON op_seen (seen_at);

CREATE TABLE label_registry (
    account_id   INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    label_key    TEXT    NOT NULL,
    display_name TEXT    NOT NULL,
    color        TEXT,
    sort         INTEGER,
    PRIMARY KEY (account_id, label_key)
) WITHOUT ROWID;
