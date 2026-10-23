-- Summaries of TeslaMate's history, read from its Postgres database (see
-- `teslamate_history`). Only summaries are kept here, never TeslaMate's raw
-- rows. They cover every car TeslaMate knows, not just the one that
-- `[teslamate] car_id` follows over MQTT.
--
-- Car ids and charge ids are TeslaMate's own. A car that was re-added keeps
-- its history under shifted ids (say car 10001, with ids above 100,000,000);
-- they are only integers here. Times are unix seconds, UTC. Distances are km,
-- energy is kWh and battery levels are percent, as TeslaMate has them.

-- One row per car, rewritten on every sync.
CREATE TABLE tm_car (
  id         INTEGER PRIMARY KEY,
  name       TEXT,
  model      TEXT,
  updated_at INTEGER NOT NULL
);

-- The km driven by one car on one local day (YYYY-MM-DD, in the configured
-- time zone, by the drive's start), and how many drives that was. A day is
-- recomputed from all its drives each time it is touched, so a drive that
-- TeslaMate corrected is picked up. Days without drives have no row.
CREATE TABLE tm_drive_daily (
  car_id INTEGER NOT NULL,
  date   TEXT NOT NULL,
  km     REAL NOT NULL,
  drives INTEGER NOT NULL,
  PRIMARY KEY (car_id, date)
) WITHOUT ROWID;

-- One row per charging session. `ended_at` is NULL while it is still charging.
-- `energy_added_kwh` reaches the battery and `energy_used_kwh` is drawn from
-- the charger, so the difference is losses. `geofence` is the name of the
-- TeslaMate geofence it happened in, if any. `fast_charger` is 1 if the car
-- reported a DC fast charger at any point of the session.
CREATE TABLE tm_charge (
  id               INTEGER PRIMARY KEY,
  car_id           INTEGER NOT NULL,
  started_at       INTEGER NOT NULL,
  ended_at         INTEGER,
  energy_added_kwh REAL,
  energy_used_kwh  REAL,
  start_soc_pct    REAL,
  end_soc_pct      REAL,
  geofence         TEXT,
  fast_charger     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX tm_charge_car_started ON tm_charge (car_id, started_at);
