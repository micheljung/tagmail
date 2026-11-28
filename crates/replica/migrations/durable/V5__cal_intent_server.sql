-- calendar: the server's CalOpInfoDto JSON. on_server: the server holds
-- the op, so retry and discard go to it. op_seq: the last seq handed to a
-- mail or calendar intent; one sequence, never reused.
ALTER TABLE cal_intent ADD COLUMN calendar TEXT;
ALTER TABLE cal_intent ADD COLUMN on_server INTEGER NOT NULL DEFAULT 0;
UPDATE cal_intent SET on_server = 1 WHERE state IN ('sent', 'done')
    OR state = 'failed' AND sent_at IS NOT NULL
       AND coalesce(error, '') NOT IN ('No longer on the server', 'discarded on the server');
INSERT INTO meta (key, value)
    SELECT 'op_seq', CAST(max(coalesce((SELECT max(seq) FROM intent), 0),
                             coalesce((SELECT max(seq) FROM cal_intent), 0)) AS TEXT)
    WHERE true ON CONFLICT (key) DO NOTHING;
