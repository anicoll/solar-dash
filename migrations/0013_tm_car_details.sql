-- What TeslaMate knows about how a car is specified, from its `cars` table,
-- rewritten on every sync. All are NULL when TeslaMate does not have the
-- value (or its version has no such column).
ALTER TABLE tm_car ADD COLUMN trim_badging TEXT;
ALTER TABLE tm_car ADD COLUMN exterior_color TEXT;
ALTER TABLE tm_car ADD COLUMN wheel_type TEXT;
ALTER TABLE tm_car ADD COLUMN marketing_name TEXT;
