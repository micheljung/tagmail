-- What a reminder shows: the instance's start and the event's SUMMARY.
ALTER TABLE cal_alarm ADD COLUMN start_utc INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cal_alarm ADD COLUMN title TEXT;
-- Dismissing looks an alarm up by its key.
CREATE INDEX cal_alarm_key ON cal_alarm (alarm_key);
