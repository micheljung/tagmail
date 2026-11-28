-- Event edits (appointments plan §4 M-F) are `edit` intents to
-- POST /api/calendar/ops; they name no message, so email_id is ''.
CREATE TABLE cal_intent_v6 (
    op_id             TEXT    PRIMARY KEY,
    seq               INTEGER NOT NULL UNIQUE,
    kind              TEXT    NOT NULL CHECK (kind IN ('rsvp', 'import', 'apply', 'edit')),
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
    error             TEXT,
    calendar          TEXT,
    on_server         INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
INSERT INTO cal_intent_v6 (op_id, seq, kind, account_id, email_id, target_account_id,
    calendar_href, uid, recurrence_id, payload, created_at, state, attempts, next_at, sent_at,
    error, calendar, on_server)
    SELECT op_id, seq, kind, account_id, email_id, target_account_id, calendar_href, uid,
        recurrence_id, payload, created_at, state, attempts, next_at, sent_at, error, calendar,
        on_server FROM cal_intent;
DROP TABLE cal_intent;
ALTER TABLE cal_intent_v6 RENAME TO cal_intent;
CREATE INDEX cal_intent_state ON cal_intent (state, next_at);
