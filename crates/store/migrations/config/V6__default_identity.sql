-- Accounts that gained an email in V5 but had no identity cannot send;
-- give each one a default identity for that address.
INSERT INTO identity (account_id, address, is_default)
SELECT id, email, 1 FROM account
WHERE email IS NOT NULL AND email != ''
  AND NOT EXISTS (SELECT 1 FROM identity WHERE identity.account_id = account.id);
