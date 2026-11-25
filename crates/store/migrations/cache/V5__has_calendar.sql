-- Set at sync from BODYSTRUCTURE: a text/calendar or application/ics part,
-- or a *.ics file name. Messages cached before stay 0 until resynced.
ALTER TABLE message ADD COLUMN has_calendar INTEGER NOT NULL DEFAULT 0 CHECK (has_calendar IN (0, 1));
