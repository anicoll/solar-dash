-- Finished days' costs, so /api/costs does not re-read a day's counters each
-- time. Money is in cents, energy in kWh (the fields of `cost::DayCost`).
-- A row is only used while `tariff_key` matches the current tariff and time
-- zone; otherwise the day is worked out again and the row replaced.
CREATE TABLE daily_cost (
  date           TEXT PRIMARY KEY,  -- local day, 'YYYY-MM-DD'
  source         TEXT NOT NULL,     -- 'inverter' or 'cloud'
  import_cents   REAL NOT NULL,
  export_cents   REAL NOT NULL,
  supply_cents   REAL NOT NULL,
  credit_cents   REAL NOT NULL,     -- zero-import credit
  actual_cents   REAL NOT NULL,
  baseline_cents REAL NOT NULL,
  savings_cents  REAL NOT NULL,
  import_kwh     REAL NOT NULL,
  export_kwh     REAL NOT NULL,
  load_kwh       REAL NOT NULL,
  tariff_key     TEXT NOT NULL,
  computed_at    INTEGER NOT NULL   -- unix seconds, UTC
);
