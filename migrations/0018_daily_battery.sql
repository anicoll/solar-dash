-- Finished days' battery figures, so /api/battery does not re-read every
-- 15-second sample of the last month on each page load. `facts` is the
-- day's `DayFacts` (src/web/battery.rs) as JSON. A row is only used while
-- `tariff_key` matches the day's plan, time zone and cache versions;
-- otherwise the day is worked out again and the row replaced.
CREATE TABLE daily_battery (
  date        TEXT PRIMARY KEY,  -- local day, 'YYYY-MM-DD'
  tariff_key  TEXT NOT NULL,
  facts       TEXT NOT NULL,     -- JSON
  computed_at INTEGER NOT NULL   -- unix seconds, UTC
);
