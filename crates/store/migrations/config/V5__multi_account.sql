-- Multi-account (M4). account.email is the primary sender address (NULL
-- until the owner enters one); keyword_probe records whether the server
-- keeps a custom keyword. Identity addresses become unique per account;
-- exact duplicates from before are dropped first so startup cannot fail,
-- and an account that lost its default gets its lowest identity promoted.
ALTER TABLE account ADD COLUMN email TEXT;
ALTER TABLE account ADD COLUMN keyword_probe TEXT
    CHECK (keyword_probe IN ('ok', 'dropped', 'unsupported', 'skipped'));
ALTER TABLE account ADD COLUMN keyword_probe_at INTEGER;
UPDATE account SET email = lower(username) WHERE instr(username, '@') > 1;

DELETE FROM identity
WHERE id NOT IN (SELECT min(id) FROM identity GROUP BY account_id, lower(address));
UPDATE identity SET is_default = 1
WHERE id IN (
    SELECT min(id) FROM identity GROUP BY account_id HAVING max(is_default) = 0
);
CREATE UNIQUE INDEX identity_address ON identity (account_id, address COLLATE NOCASE);
