-- Disposable: everything here is rebuilt from IMAP. Deleting cache.db
-- creates a new cache_epoch, so clients holding old ids get 410 Gone.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

INSERT INTO meta (key, value) VALUES
    ('cache_epoch', lower(hex(randomblob(8)))),
    ('min_seq', '0');

CREATE TABLE mailbox (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL,
    name            TEXT    NOT NULL,
    delimiter       TEXT,
    special_use     TEXT,
    uidvalidity     INTEGER NOT NULL,
    uidnext         INTEGER,
    highestmodseq   INTEGER,
    nomodseq        INTEGER NOT NULL DEFAULT 0 CHECK (nomodseq IN (0, 1)),
    keyword_capable INTEGER NOT NULL DEFAULT 1 CHECK (keyword_capable IN (0, 1)),
    sync_state      TEXT    NOT NULL DEFAULT 'initial'
                            CHECK (sync_state IN ('initial', 'catchup', 'live')),
    sync_cursor_uid INTEGER,
    snapshot_modseq INTEGER,
    last_poll_at    INTEGER,
    UNIQUE (account_id, name)
);

CREATE TABLE thread (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    subject_norm TEXT    NOT NULL DEFAULT '',
    last_date    INTEGER NOT NULL DEFAULT 0,
    msg_count    INTEGER NOT NULL DEFAULT 0,
    unread_count INTEGER NOT NULL DEFAULT 0,
    starred      INTEGER NOT NULL DEFAULT 0 CHECK (starred IN (0, 1)),
    merged_into  INTEGER REFERENCES thread (id) ON DELETE SET NULL
);

CREATE INDEX thread_account_date ON thread (account_id, last_date DESC);

-- subject_norm is the threading subject (§3.5); the index finds subject-edge
-- candidates in both date directions.
CREATE TABLE message (
    id             INTEGER PRIMARY KEY,
    account_id     INTEGER NOT NULL,
    email_id       TEXT    NOT NULL,
    msgid_hdr      TEXT,
    in_reply_to    TEXT,
    thread_id      INTEGER REFERENCES thread (id) DEFERRABLE INITIALLY DEFERRED,
    date           INTEGER NOT NULL,
    subject_norm   TEXT    NOT NULL DEFAULT '',
    from_json      TEXT,
    to_json        TEXT,
    cc_json        TEXT,
    subject        TEXT,
    snippet        TEXT,
    size           INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0 CHECK (has_attachment IN (0, 1)),
    raw_blob       TEXT,
    text_blob      TEXT,
    tombstoned_at  INTEGER,
    UNIQUE (account_id, email_id)
);

CREATE INDEX message_thread ON message (thread_id, date);
CREATE INDEX message_subject ON message (account_id, subject_norm, date);
CREATE INDEX message_tombstoned ON message (account_id, tombstoned_at)
    WHERE tombstoned_at IS NOT NULL;

-- flags: core::Flags bits. keywords: canonical core::KeywordSet storage.
CREATE TABLE location (
    mailbox_id INTEGER NOT NULL REFERENCES mailbox (id) ON DELETE CASCADE,
    uid        INTEGER NOT NULL,
    message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
    flags      INTEGER NOT NULL DEFAULT 0,
    keywords   TEXT    NOT NULL DEFAULT '',
    modseq     INTEGER,
    PRIMARY KEY (mailbox_id, uid)
) WITHOUT ROWID;

CREATE INDEX location_message ON location (message_id);

CREATE TABLE label (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    keyword    TEXT,
    label_key  TEXT    NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('keyword', 'folder')),
    mailbox_id INTEGER REFERENCES mailbox (id) ON DELETE CASCADE,
    UNIQUE (account_id, label_key, kind),
    CHECK ((kind = 'keyword') = (keyword IS NOT NULL)),
    CHECK ((kind = 'folder') = (mailbox_id IS NOT NULL))
);

-- Derived: union of the keywords over a message's locations.
CREATE TABLE message_label (
    message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
    label_id   INTEGER NOT NULL REFERENCES label (id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
) WITHOUT ROWID;

CREATE INDEX message_label_label ON message_label (label_id, message_id);

-- Derived: drives inbox and folder lists.
CREATE TABLE thread_mailbox (
    thread_id  INTEGER NOT NULL REFERENCES thread (id) ON DELETE CASCADE,
    mailbox_id INTEGER NOT NULL REFERENCES mailbox (id) ON DELETE CASCADE,
    last_date  INTEGER NOT NULL,
    PRIMARY KEY (thread_id, mailbox_id)
) WITHOUT ROWID;

CREATE INDEX thread_mailbox_list ON thread_mailbox (mailbox_id, last_date DESC, thread_id);

CREATE TABLE thread_ref (
    account_id INTEGER NOT NULL,
    msgid_hdr  TEXT    NOT NULL,
    thread_id  INTEGER NOT NULL REFERENCES thread (id) ON DELETE CASCADE
                       DEFERRABLE INITIALLY DEFERRED,
    PRIMARY KEY (account_id, msgid_hdr)
) WITHOUT ROWID;

CREATE INDEX thread_ref_thread ON thread_ref (thread_id);

-- detail=full keeps token positions: phrase and NEAR queries need them.
CREATE VIRTUAL TABLE message_fts USING fts5 (
    subject, from_txt, to_txt, body,
    content = '',
    contentless_delete = 1,
    detail = full,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- op: upsert | delete | merged:<target thread id> | bulk:<mailbox id>
CREATE TABLE change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    entity     TEXT    NOT NULL,
    entity_id  INTEGER NOT NULL,
    op         TEXT    NOT NULL,
    payload    TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX change_log_age ON change_log (created_at);
