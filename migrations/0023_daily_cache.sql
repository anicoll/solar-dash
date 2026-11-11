-- Finished days' worked-out figures for the pages that would otherwise read
-- every 15-second sample of the range again on each load: the Amber
-- comparison and what-if (`kind` 'amber_whatif'), the bill's tariff split
-- ('grid_split'), the Amber price stats ('price_stats') and the battery's
-- discharge for the payback page ('discharge'). One row per kind and local
-- day; `payload` is the day's figures as JSON. A row is only used while its
-- `cache_key` matches what the day would be worked out from now (plan, time
-- zone, price fingerprint, settings and a version number); otherwise the day
-- is worked out again and the row replaced. Like `daily_cost`, it only ever
-- holds a copy: dropping the rows loses nothing.
CREATE TABLE daily_cache (
  kind        TEXT    NOT NULL,
  date        TEXT    NOT NULL,  -- local day, 'YYYY-MM-DD'
  cache_key   TEXT    NOT NULL,
  payload     TEXT    NOT NULL,  -- JSON
  computed_at INTEGER NOT NULL,  -- unix seconds, UTC
  PRIMARY KEY (kind, date)
) WITHOUT ROWID;

-- The price queries ask for one day's rows from every source with a given
-- name prefix, so they cannot use the (source, channel, start_ts) key and read
-- the whole table for each day. This index lets them read just the day.
CREATE INDEX price_interval_start_ts ON price_interval (start_ts);
