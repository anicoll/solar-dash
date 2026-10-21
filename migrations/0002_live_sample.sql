-- Live samples read straight from the inverter over Modbus (about one every
-- 15 seconds). There is no plant_id column: this app reads one inverter on
-- the local network, so there is only ever one plant here. (The `reading`
-- table keeps plant_id because the iSolarCloud API is per plant.)
CREATE TABLE live_sample (
  ts         INTEGER PRIMARY KEY,  -- unix seconds, UTC
  pv_w       REAL,                 -- solar power, W
  load_w     REAL,                 -- house consumption, W
  grid_w     REAL,                 -- positive = importing, negative = exporting
  battery_w  REAL,                 -- positive = charging, negative = discharging
  soc_pct    REAL                  -- battery state of charge, 0-100
) WITHOUT ROWID;
