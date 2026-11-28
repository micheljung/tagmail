-- Calendar ops (appointments plan §4 M-C). op_seq orders cal ops with the
-- mail ops in the op list, like pending_op's payload seq; it is no cache
-- id. attendee and identity_id are the owner's ATTENDEE and the identity a
-- REPLY is sent from; href is the resource the op writes; dest_href the
-- calendar a move goes to.
ALTER TABLE cal_op ADD COLUMN op_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cal_op ADD COLUMN attendee TEXT;
ALTER TABLE cal_op ADD COLUMN identity_id INTEGER REFERENCES identity (id) ON DELETE SET NULL;
ALTER TABLE cal_op ADD COLUMN href TEXT;
ALTER TABLE cal_op ADD COLUMN dest_href TEXT;
ALTER TABLE cal_op ADD COLUMN reply_cancelled INTEGER NOT NULL DEFAULT 0 CHECK (reply_cancelled IN (0, 1));
ALTER TABLE cal_op ADD COLUMN prior_partstat TEXT;

-- The authserv-id offered to the owner (plan §3.4), refreshed by the
-- calendar runner.
ALTER TABLE caldav_trust ADD COLUMN suggestion TEXT;
