-- Cars that TeslaMate does not track (a plug-in hybrid, say) and their petrol
-- fills. Edited on the Cars tab and the Admin page; fuel_log.rs works out the
-- fuel economy and what the petrol cost.

CREATE TABLE manual_car (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL,
  model      TEXT,
  kind       TEXT NOT NULL DEFAULT 'phev', -- 'phev' | 'petrol' | 'hybrid' | 'diesel', display only
  created_at INTEGER NOT NULL              -- unix seconds, UTC
);

-- One visit to the pump. Deleting a car deletes its fills (the pool turns
-- foreign keys on, see db::open).
CREATE TABLE fuel_fill (
  id           INTEGER PRIMARY KEY,
  car_id       INTEGER NOT NULL REFERENCES manual_car(id) ON DELETE CASCADE,
  date         TEXT NOT NULL,              -- 'YYYY-MM-DD', local day
  litres       REAL NOT NULL,
  cost_dollars REAL NOT NULL,
  odometer_km  REAL,
  full_tank    INTEGER NOT NULL DEFAULT 1, -- 1 when the tank was filled to full
  note         TEXT NOT NULL DEFAULT ''
);

CREATE INDEX fuel_fill_car_date ON fuel_fill (car_id, date);
