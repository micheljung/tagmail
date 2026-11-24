-- Rows written while paired with another server: kept for the user to
-- read or discard, never sent to the current one.
ALTER TABLE intent ADD COLUMN other_server INTEGER NOT NULL DEFAULT 0;
ALTER TABLE draft ADD COLUMN other_server INTEGER NOT NULL DEFAULT 0;
