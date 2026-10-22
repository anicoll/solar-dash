-- Data from the GloBird customer portal (see `globird::sync`). The portal has
-- no official API, so these tables hold what its pages show, as it sent it.
--
-- The three tables are rewritten one day at a time: the sync deletes all of a
-- day's rows and inserts the fresh ones in one transaction, so a line GloBird
-- revised or removed does not linger. `date` is the local day, YYYY-MM-DD,
-- and `fetched_at` is when we read it (unix seconds, UTC).
--
-- There is no service column: the tables assume one electricity service
-- (the sync keeps each service's backfill progress apart in `sync_state`).

-- What each day cost, one row per charge category. The category is GloBird's
-- own text ("USAGE", "SOLAR", "SUPPLY", "ZEROHERO Credit", ...) and is never
-- interpreted here, because new ones can turn up. Money is in cents and
-- includes GST; a negative amount is a credit. A day counts as complete once
-- it has any category other than SUPPLY, which GloBird posts first.
CREATE TABLE globird_cost (
  date               TEXT NOT NULL,
  category           TEXT NOT NULL,
  amount_cents       INTEGER NOT NULL,
  quantity           REAL NOT NULL,     -- kWh; 1 for ZeroHero credits; 0 for SUPPLY
  meter_read_missing INTEGER NOT NULL,  -- 1 if GloBird had no meter read that day
  fetched_at         INTEGER NOT NULL,
  PRIMARY KEY (date, category)
) WITHOUT ROWID;

-- The kWh of each charge period of the day. `suffix` is the meter register:
-- one starting with E is import and one starting with B is export (see
-- `globird_interval`). `charge_type` is GloBird's text, e.g. 'Peak Usage'
-- or 'Super Export top up'. `quality` is the meter data's quality method.
CREATE TABLE globird_usage (
  date          TEXT NOT NULL,
  suffix        TEXT NOT NULL,
  charge_type   TEXT NOT NULL,
  category_code TEXT,
  kwh           REAL NOT NULL,
  quality       TEXT,
  fetched_at    INTEGER NOT NULL,
  PRIMARY KEY (date, suffix, charge_type)
) WITHOUT ROWID;

-- The meter's 5-minute readings: one row per register per day. `intervals` is
-- a JSON array of the 288 kWh values exactly as GloBird sent them, and
-- `kwh_total` is their sum. The portal repeats the same array on every
-- charge-period row of a register, so we keep the first one only.
--
-- NOT VERIFIED YET: which local time each index matches. Index 0 is probably
-- 00:00-00:05 of the local day, but that has not been checked against our own
-- inverter samples (and a day with a clock change has 276 or 300 real
-- intervals, not 288). Do not rely on the alignment until it is checked.
CREATE TABLE globird_interval (
  date       TEXT NOT NULL,
  suffix     TEXT NOT NULL,
  direction  TEXT NOT NULL,  -- 'import' or 'export'
  kwh_total  REAL NOT NULL,
  intervals  TEXT NOT NULL,
  fetched_at INTEGER NOT NULL,
  PRIMARY KEY (date, suffix)
) WITHOUT ROWID;
