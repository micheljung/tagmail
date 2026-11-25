-- CalDAV calendars (appointments plan §3.2). Every table is account-scoped
-- and cascades with the account; op_seen has no foreign key and is purged
-- with the account explicitly.
ALTER TABLE op_seen ADD COLUMN kind TEXT NOT NULL DEFAULT 'mail' CHECK (kind IN ('mail', 'cal'));

-- username/secret NULL: the account's IMAP login is reused. scheduling is
-- the owner's override, NULL = scheduling_detected.
CREATE TABLE caldav (
    account_id            INTEGER PRIMARY KEY REFERENCES account (id) ON DELETE CASCADE,
    url                   TEXT,
    principal_url         TEXT,
    home_url              TEXT,
    confirmed_host        TEXT,
    -- The host discovery asked the owner to confirm.
    pending_host          TEXT,
    allow_insecure        INTEGER NOT NULL DEFAULT 0 CHECK (allow_insecure IN (0, 1)),
    scheduling            TEXT    CHECK (scheduling IN ('server', 'client')),
    scheduling_detected   TEXT    CHECK (scheduling_detected IN ('server', 'client')),
    username              TEXT,
    secret_ct             BLOB,
    secret_nonce          BLOB,
    fallback_account_id   INTEGER REFERENCES account (id) ON DELETE SET NULL,
    default_calendar_href TEXT,
    discovery             TEXT    CHECK (discovery IN ('well_known', 'manual', 'srv')),
    status                TEXT    NOT NULL DEFAULT 'unconfigured'
                                  CHECK (status IN ('unconfigured', 'needs_confirmation', 'ok', 'error')),
    last_error            TEXT,
    updated_at            INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((username IS NULL) = (secret_ct IS NULL)),
    CHECK ((secret_ct IS NULL) = (secret_nonce IS NULL)),
    CHECK (fallback_account_id IS NULL OR fallback_account_id != account_id)
);

CREATE TABLE calendar_pref (
    account_id INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    href       TEXT    NOT NULL,
    color      TEXT,
    visible    INTEGER NOT NULL DEFAULT 1 CHECK (visible IN (0, 1)),
    sort       INTEGER,
    PRIMARY KEY (account_id, href)
) WITHOUT ROWID;

-- authserv_id: the owner-confirmed Authentication-Results id; NULL = no
-- automatic apply.
CREATE TABLE caldav_trust (
    account_id  INTEGER PRIMARY KEY REFERENCES account (id) ON DELETE CASCADE,
    authserv_id TEXT
);

-- account_id is the source account (whose mail held the invite and whose
-- identity replies); target_account_id owns the calendar.
CREATE TABLE cal_op (
    op_id             TEXT    PRIMARY KEY,
    account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    target_account_id INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    calendar_href     TEXT,
    uid               TEXT    NOT NULL,
    recurrence_id     TEXT,
    kind              TEXT    NOT NULL
                              CHECK (kind IN ('put', 'delete', 'move', 'rsvp', 'import', 'apply')),
    partstat          TEXT,
    ics_file          TEXT,
    if_match          TEXT,
    scope             TEXT    CHECK (scope IN ('this', 'following', 'all')),
    reply_op_id       TEXT,
    state             TEXT    NOT NULL DEFAULT 'pending'
                              CHECK (state IN ('pending', 'running', 'done', 'failed')),
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_at           INTEGER,
    error             TEXT,
    conflict_kind     TEXT,
    conflict_ics_file TEXT,
    created_at        INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;

CREATE INDEX cal_op_due ON cal_op (state, next_at);

-- fire_at is kept because alarm_key is a hash: rows are pruned by age.
CREATE TABLE alarm_ack (
    alarm_key  TEXT    PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    fire_at    INTEGER NOT NULL,
    acked_at   INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;

-- target: 'session:<hash hex>' or 'device:<id>'.
CREATE TABLE alarm_sent (
    alarm_key  TEXT    NOT NULL,
    target     TEXT    NOT NULL,
    account_id INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    fire_at    INTEGER NOT NULL,
    sent_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (alarm_key, target)
) WITHOUT ROWID;

CREATE INDEX alarm_sent_fire ON alarm_sent (fire_at);

ALTER TABLE push_subscription ADD COLUMN reminders INTEGER NOT NULL DEFAULT 0 CHECK (reminders IN (0, 1));
ALTER TABLE device ADD COLUMN reminders INTEGER NOT NULL DEFAULT 1 CHECK (reminders IN (0, 1));
