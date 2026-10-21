-- Energy drawn from the wall during a charging session, worked out by
-- adding up the charger power over time (see `teslamate::Recorder`).
-- `energy_added_kwh` is what reached the battery, so the difference is
-- charging losses. NULL for sessions recorded before this column existed.
ALTER TABLE ev_charge_session ADD COLUMN energy_used_kwh REAL;
