-- received: the message's INTERNALDATE, which bounds automatic applies of
-- invitations; NULL for messages cached before this version.
ALTER TABLE message ADD COLUMN received INTEGER;

-- An invitation that could not be read yet (IMAP or calendar not ready)
-- waits until next_at, backing off per wait.
ALTER TABLE cal_inbox ADD COLUMN waits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cal_inbox ADD COLUMN next_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX cal_inbox_due ON cal_inbox (next_at);
