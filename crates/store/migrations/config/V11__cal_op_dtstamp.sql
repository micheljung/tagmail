-- The DTSTAMP of the REPLY an apply op takes an attendee's answer from
-- (plan §4 M-F), so an older REPLY that arrives later is not applied.
ALTER TABLE cal_op ADD COLUMN dtstamp INTEGER;
