-- The car's odometer, one row per local day (see `teslamate::Odometer`).
-- The row holds the latest reading of that day, and is rewritten at most
-- every 30 minutes, so this table gets a few writes an hour at most. The
-- km driven on a day is its reading minus the previous row's. A day with no
-- reading has no row.
CREATE TABLE ev_odometer_daily (
  date         TEXT PRIMARY KEY,  -- local day, YYYY-MM-DD
  odometer_km  REAL NOT NULL,
  updated_at   INTEGER NOT NULL   -- unix seconds, UTC
);
