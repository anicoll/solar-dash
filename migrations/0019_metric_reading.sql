-- Readings by what they mean (a metric, such as 'pv_w'), not by the id one
-- data source gives them. `source` says where a reading came from, e.g.
-- 'isolarcloud:1234567'. The value is in the metric's own unit (the battery
-- charge is a percentage here; iSolarCloud stored it as a fraction).
CREATE TABLE metric_reading (
  source  TEXT NOT NULL,
  metric  TEXT NOT NULL,
  ts      INTEGER NOT NULL,  -- unix seconds, UTC
  value   REAL,
  PRIMARY KEY (source, metric, ts)
) WITHOUT ROWID;

-- Copy every `reading` row whose point we know. This is the same table as
-- `POINTS` in src/isolar/points.rs (a test checks they agree). It runs once, in
-- SQL, because `reading` is large (millions of rows on a long-running install)
-- and a row-by-row copy in Rust would be slow. Rows come out sorted by the new
-- key, which makes the inserts append to the end of the table's b-tree.
-- `NULL * scale` is NULL, so readings without a value are copied as NULL.
--
-- `reading` itself is left as it is: it is no longer written, and is kept for
-- one release as a fallback before a later migration drops it.
WITH map(point_id, metric, scale) AS (
  VALUES
    (83033, 'pv_w',                     1.0),
    (83067, 'pv_dc_w',                  1.0),
    (83106, 'load_w',                   1.0),
    (83328, 'grid_w',                   1.0),
    (83326, 'battery_w',                1.0),
    (83252, 'soc_pct',                100.0),
    (83022, 'pv_day_wh',                1.0),
    (83102, 'grid_import_day_wh',       1.0),
    (83072, 'grid_export_day_wh',       1.0),
    (83118, 'load_day_wh',              1.0),
    (83322, 'battery_charge_day_wh',    1.0),
    (83323, 'battery_discharge_day_wh', 1.0),
    (83097, 'self_consumed_day_wh',     1.0)
)
INSERT INTO metric_reading (source, metric, ts, value)
SELECT 'isolarcloud:' || r.plant_id, map.metric, r.ts, r.value * map.scale
FROM reading r
JOIN map ON map.point_id = r.point_id
ORDER BY 'isolarcloud:' || r.plant_id, map.metric, r.ts;
