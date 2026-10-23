-- What each finished charging session at home cost, worked out once from the
-- house's energy (see `ev_cost::historic_session_cost`) and kept on the
-- session's row. The car's minute-by-minute power is read from TeslaMate when
-- a session is costed and not kept. Energy is in kWh, money in cents.
--
-- `cost_key` tags the rules, time zone, plan and bill a row was costed with;
-- NULL means not costed yet, and a key that no longer matches means the
-- session is costed again. `uncovered_kwh` is the part of the session in time
-- the house data does not cover; it is counted as grid, so it also says how
-- much of the cost is an estimate.
ALTER TABLE tm_charge ADD COLUMN cost_key TEXT;
ALTER TABLE tm_charge ADD COLUMN grid_kwh REAL;
ALTER TABLE tm_charge ADD COLUMN self_kwh REAL;
ALTER TABLE tm_charge ADD COLUMN uncovered_kwh REAL;
ALTER TABLE tm_charge ADD COLUMN cost_cents REAL;
ALTER TABLE tm_charge ADD COLUMN baseline_cents REAL;
ALTER TABLE tm_charge ADD COLUMN feed_in_forgone_cents REAL;
