-- Pending-op overlay (plan §3.4 Writes): flags/keywords hold what the UI
-- shows. While an op is pending on a location, srv_flags/srv_keywords keep
-- the server's state so the overlay can be dropped or re-projected; hidden
-- marks a location a pending op moves out of its mailbox.
ALTER TABLE location ADD COLUMN srv_flags INTEGER;
ALTER TABLE location ADD COLUMN srv_keywords TEXT;
ALTER TABLE location ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1));

CREATE INDEX location_overlaid ON location (message_id) WHERE srv_flags IS NOT NULL OR hidden = 1;
