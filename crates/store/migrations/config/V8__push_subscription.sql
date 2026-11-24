-- Web Push subscriptions live as long as the browser session that made
-- them: logout, revocation and expiry delete them through the cascade.
DROP TABLE push_subscription;
CREATE TABLE push_subscription (
    id              INTEGER PRIMARY KEY,
    session_hash    BLOB    NOT NULL REFERENCES session (id_hash) ON DELETE CASCADE,
    endpoint        TEXT    NOT NULL UNIQUE,
    p256dh          BLOB    NOT NULL CHECK (length(p256dh) = 65),
    auth            BLOB    NOT NULL CHECK (length(auth) = 16),
    preview         INTEGER NOT NULL DEFAULT 1 CHECK (preview IN (0, 1)),
    -- JSON array of account ids; NULL = all accounts.
    account_ids     TEXT,
    user_agent      TEXT,
    failures        INTEGER NOT NULL DEFAULT 0,
    last_success_at INTEGER,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at      INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX push_subscription_session ON push_subscription (session_hash);
