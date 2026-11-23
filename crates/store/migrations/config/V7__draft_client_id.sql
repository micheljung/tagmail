-- Client-generated id of a draft: re-posting it updates the draft instead
-- of creating a second one (replica sync after a lost ack).
ALTER TABLE draft ADD COLUMN client_id TEXT;
CREATE UNIQUE INDEX draft_client_id ON draft (client_id) WHERE client_id IS NOT NULL;
