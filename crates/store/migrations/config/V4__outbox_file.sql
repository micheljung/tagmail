-- Queued messages move out of config.db into files under <data>/outbox
-- (mime_file, relative to it), so a large message is never held in memory
-- or in a row. Items queued before this version lose their message and
-- fail with an error the player sees in the outbox.
-- flag_msgid/flag_kind: the replied-to or forwarded message, by Message-ID,
-- flagged \Answered or $Forwarded once sent.
ALTER TABLE outbox DROP COLUMN mime_blob;
ALTER TABLE outbox ADD COLUMN mime_file TEXT;
ALTER TABLE outbox ADD COLUMN mime_size INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN flag_msgid TEXT;
ALTER TABLE outbox ADD COLUMN flag_kind TEXT CHECK (flag_kind IN ('answered', 'forwarded'));
