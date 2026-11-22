-- The engine's text pass: messages still missing text, newest first.
CREATE INDEX message_pending_text ON message (account_id, date DESC, id DESC)
    WHERE text_blob IS NULL AND tombstoned_at IS NULL;
