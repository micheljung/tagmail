-- `$has_cal` became a hidden keyword; its labels were derived at sync and
-- would otherwise stay until a resync. message_label rows cascade.
DELETE FROM label WHERE kind = 'keyword' AND keyword = '$has_cal' COLLATE NOCASE;
