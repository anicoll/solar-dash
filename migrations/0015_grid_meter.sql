-- The inverter's lifetime grid energy counters, in Wh. They count in steps of
-- 100 Wh and pick up the battery's fast hunting around 0 W that the 15-second
-- live samples never see. A row is saved only when either value changes (see
-- `modbus::meter_to_save`), so a quiet night is a handful of rows.
CREATE TABLE grid_meter (
  ts         INTEGER PRIMARY KEY,  -- unix seconds, UTC, when the inverter was read
  import_wh  REAL NOT NULL,        -- total energy imported from the grid, Wh
  export_wh  REAL NOT NULL         -- total energy exported to the grid, Wh
) WITHOUT ROWID;
