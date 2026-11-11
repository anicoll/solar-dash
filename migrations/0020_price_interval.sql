-- Electricity prices per interval, from any price source. `source` says where
-- a row came from, e.g. 'amber:<site id>', like `metric_reading.source`.
--
-- `cents` is c/kWh including GST, exactly as the source gives it. The sign of
-- a feed-in price is NOT flipped: whether a positive feed-in price means "you
-- are paid" is not confirmed yet, so readers must not assume either way.
CREATE TABLE price_interval (
  source      TEXT    NOT NULL,
  channel     TEXT    NOT NULL,  -- 'general' | 'feed_in' | 'controlled_load'
  start_ts    INTEGER NOT NULL,  -- unix seconds, UTC
  end_ts      INTEGER NOT NULL,
  cents       REAL    NOT NULL,
  spot_cents  REAL,              -- wholesale price, when the source has one
  kind        TEXT    NOT NULL,  -- 'actual' | 'current' | 'forecast'
  fetched_at  INTEGER NOT NULL,  -- unix seconds, when we stored this row
  PRIMARY KEY (source, channel, start_ts)
) WITHOUT ROWID;
