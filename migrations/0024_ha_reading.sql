-- Numeric states of Home Assistant entities, read over its WebSocket API (the
-- `home-assistant` source). One row per entity per time, at most one every 15
-- seconds. `unit` is the unit Home Assistant reported (or the config's
-- override); `value` is stored as reported, not converted. Entities that the
-- config maps to a dashboard metric are ALSO stored in `metric_reading` (source
-- 'home-assistant'), converted to the metric's unit. Text states ('on',
-- 'unavailable', ...) are not stored.
CREATE TABLE ha_reading (
  entity_id TEXT    NOT NULL,  -- e.g. 'sensor.solar_power'
  ts        INTEGER NOT NULL,  -- unix seconds, UTC
  value     REAL    NOT NULL,
  unit      TEXT,
  PRIMARY KEY (entity_id, ts)
) WITHOUT ROWID;
