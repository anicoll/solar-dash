-- One row per time the car charged, worked out from TeslaMate's charging
-- state (see `teslamate::Recorder`). `ended_at` is NULL while the car is
-- still charging. The energy, level and power are updated about every 5
-- minutes while charging, so this table gets a few writes an hour at most.
CREATE TABLE ev_charge_session (
  id                INTEGER PRIMARY KEY,
  car_id            INTEGER NOT NULL,  -- TeslaMate's car id
  started_at        INTEGER NOT NULL,  -- unix seconds, UTC
  ended_at          INTEGER,           -- unix seconds, UTC; NULL while charging
  energy_added_kwh  REAL,
  start_soc_pct     REAL,              -- battery level when charging began, 0-100
  end_soc_pct       REAL,              -- latest battery level, 0-100
  max_power_kw      REAL,              -- highest charging power seen
  geofence          TEXT               -- the named place, such as 'Home'
);

CREATE INDEX ev_charge_session_started_at ON ev_charge_session (started_at);

-- A car can only have one open session, so a bug or a restart can never
-- leave two rows that both claim to be charging.
CREATE UNIQUE INDEX ev_charge_session_open ON ev_charge_session (car_id) WHERE ended_at IS NULL;
