-- Queued calendar writes (appointments plan §3.2), flushed to the server
-- under their client op id. The message is kept by its IMAP-stable email id;
-- payload is the request without its messageId.
CREATE TABLE cal_intent (
    op_id             TEXT    PRIMARY KEY,
    seq               INTEGER NOT NULL UNIQUE,
    kind              TEXT    NOT NULL CHECK (kind IN ('rsvp', 'import', 'apply')),
    account_id        INTEGER NOT NULL,
    email_id          TEXT    NOT NULL,
    target_account_id INTEGER,
    calendar_href     TEXT,
    uid               TEXT,
    recurrence_id     TEXT,
    payload           TEXT    NOT NULL,
    created_at        INTEGER NOT NULL,
    state             TEXT    NOT NULL CHECK (state IN ('pending', 'sent', 'done', 'failed')),
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_at           INTEGER,
    sent_at           INTEGER,
    error             TEXT
) WITHOUT ROWID;

CREATE INDEX cal_intent_state ON cal_intent (state, next_at);
