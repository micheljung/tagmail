-- CalDAV cache (appointments plan §3.2), rebuilt from the servers like the
-- rest of cache.db. Keys that leave cache.db are (account, calendar href,
-- UID), never these row ids.

-- Invitations still to process, written in the message's sync transaction.
CREATE TABLE cal_inbox (
    account_id    INTEGER NOT NULL,
    email_id      TEXT    NOT NULL,
    internal_date INTEGER NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, email_id)
) WITHOUT ROWID;

CREATE INDEX cal_inbox_date ON cal_inbox (internal_date);

CREATE TABLE calendar (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    href         TEXT    NOT NULL,
    display_name TEXT,
    color_srv    TEXT,
    ctag         TEXT,
    sync_token   TEXT,
    writable     INTEGER NOT NULL DEFAULT 1 CHECK (writable IN (0, 1)),
    -- Servers without CTag or sync-token still mark a completed first sync.
    synced_at    INTEGER,
    UNIQUE (account_id, href)
);

-- ics: zstd. window_edge: end of the window a recurring event was expanded
-- to; NULL once all its instances are in cal_instance.
CREATE TABLE cal_event (
    id           INTEGER PRIMARY KEY,
    calendar_id  INTEGER NOT NULL REFERENCES calendar (id) ON DELETE CASCADE,
    href         TEXT    NOT NULL,
    etag         TEXT,
    uid          TEXT    NOT NULL,
    ics          BLOB    NOT NULL,
    sequence     INTEGER NOT NULL DEFAULT 0,
    dtstamp      INTEGER,
    status       TEXT,
    my_partstat  TEXT,
    organizer    TEXT,
    is_recurring INTEGER NOT NULL DEFAULT 0 CHECK (is_recurring IN (0, 1)),
    first_start  INTEGER,
    last_end     INTEGER,
    window_edge  INTEGER,
    UNIQUE (calendar_id, href)
);

CREATE INDEX cal_event_uid ON cal_event (uid);
CREATE INDEX cal_event_edge ON cal_event (window_edge) WHERE window_edge IS NOT NULL;

-- Resources without a usable VEVENT (a task, garbage) at the etag they had,
-- so an unchanged one is not fetched again.
CREATE TABLE cal_skip (
    calendar_id INTEGER NOT NULL REFERENCES calendar (id) ON DELETE CASCADE,
    href        TEXT    NOT NULL,
    etag        TEXT,
    PRIMARY KEY (calendar_id, href)
) WITHOUT ROWID;

CREATE TABLE cal_instance (
    event_id      INTEGER NOT NULL REFERENCES cal_event (id) ON DELETE CASCADE,
    start_utc     INTEGER NOT NULL,
    end_utc       INTEGER NOT NULL,
    recurrence_id TEXT,
    all_day       INTEGER NOT NULL DEFAULT 0 CHECK (all_day IN (0, 1))
);

CREATE INDEX cal_instance_start ON cal_instance (start_utc);
CREATE INDEX cal_instance_event ON cal_instance (event_id);

CREATE TABLE cal_alarm (
    event_id      INTEGER NOT NULL REFERENCES cal_event (id) ON DELETE CASCADE,
    alarm_key     TEXT    NOT NULL,
    recurrence_id TEXT,
    fire_at       INTEGER NOT NULL
);

CREATE INDEX cal_alarm_fire ON cal_alarm (fire_at);
CREATE INDEX cal_alarm_event ON cal_alarm (event_id);
