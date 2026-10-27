-- Data edited on the Admin page. It used to live in config.toml; `serve`
-- copies it in once (see admin_import.rs) and ignores the config after that.

-- Electricity plans. A plan applies from valid_from (local day, inclusive)
-- until the day before the next plan's valid_from. The earliest plan may have
-- valid_from NULL (no known start). body = JSON of config::Tariff.
CREATE TABLE tariff_plan (
  id         INTEGER PRIMARY KEY,
  valid_from TEXT UNIQUE,          -- 'YYYY-MM-DD' or NULL
  body       TEXT NOT NULL
);

CREATE TABLE petrol_price (
  from_date       TEXT PRIMARY KEY, -- 'YYYY-MM-DD'
  cents_per_litre REAL NOT NULL
);

CREATE TABLE purchase (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL,
  date         TEXT NOT NULL,       -- 'YYYY-MM-DD'
  cost_dollars REAL NOT NULL,
  battery_kwh  REAL
);

CREATE TABLE bill (               -- same columns as the bills CSV
  from_date      TEXT PRIMARY KEY,
  to_date        TEXT NOT NULL,
  bill_total     REAL NOT NULL,
  excluded       REAL NOT NULL,
  import_kwh     REAL NOT NULL,
  import_dollars REAL NOT NULL,
  export_kwh     REAL NOT NULL,
  export_dollars REAL NOT NULL,
  supply_dollars REAL NOT NULL,
  note           TEXT NOT NULL DEFAULT ''
);

CREATE TABLE setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL              -- JSON
);

-- Every admin write and the one-time imports, newest last.
CREATE TABLE admin_change (
  id     INTEGER PRIMARY KEY,
  at     INTEGER NOT NULL,         -- unix seconds, UTC
  what   TEXT NOT NULL,            -- 'tariffs' | 'petrol' | 'purchases' | 'bills' | 'settings'
  action TEXT NOT NULL,            -- 'import' | 'create' | 'update' | 'delete' | 'replace'
  key    TEXT,                     -- row key (id, date, setting key) when one row changed
  before TEXT,                     -- JSON of the row before, NULL for create/import
  after  TEXT                      -- JSON of the row after, NULL for delete
);
