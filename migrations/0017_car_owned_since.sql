-- When each car was bought, so the Cars tab can show how long it has been
-- owned. TeslaMate has no purchase date (and may have started recording well
-- after the car arrived), so solar-dash keeps the date itself.

-- Manual cars: 'YYYY-MM-DD', local day; NULL when not set.
ALTER TABLE manual_car ADD COLUMN owned_since TEXT;

-- TeslaMate cars. Apart from tm_car because the TeslaMate sync upserts that
-- table (db::upsert_tm_cars); no foreign key for the same reason.
CREATE TABLE tm_car_owned (
  car_id      INTEGER PRIMARY KEY,
  owned_since TEXT NOT NULL              -- 'YYYY-MM-DD', local day
);
