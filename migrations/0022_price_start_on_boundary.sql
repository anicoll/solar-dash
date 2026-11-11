-- Amber gives every interval a start one second after the boundary
-- (12:00:01) and an end on it (12:05:00). Rows were stored as given; they are
-- now stored with the start on the whole minute. This moves the old rows.
--
-- Where a row on the boundary already exists, an actual price must survive
-- over a current/forecast one; otherwise the row already on the boundary
-- stays and the shifted one is dropped.
DELETE FROM price_interval
WHERE kind <> 'actual'
  AND EXISTS (
    SELECT 1 FROM price_interval AS shifted
    WHERE shifted.source = price_interval.source
      AND shifted.channel = price_interval.channel
      AND shifted.start_ts % 60 <> 0
      AND shifted.start_ts - shifted.start_ts % 60 = price_interval.start_ts
      AND shifted.kind = 'actual'
  );

UPDATE OR IGNORE price_interval
SET start_ts = start_ts - start_ts % 60
WHERE start_ts % 60 <> 0;

-- Rows that could not move because the boundary was taken.
DELETE FROM price_interval WHERE start_ts % 60 <> 0;
