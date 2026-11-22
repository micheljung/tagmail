-- Senders whose remote images always load (plan §3.9), by lowercased
-- address, so the list survives cache rebuilds.
CREATE TABLE image_sender (
    address    TEXT    PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;
