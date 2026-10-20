CREATE TABLE point (
  point_id  INTEGER PRIMARY KEY,
  code      TEXT,
  name      TEXT,
  unit      TEXT
);
CREATE TABLE reading (
  plant_id  INTEGER NOT NULL,
  point_id  INTEGER NOT NULL REFERENCES point,
  ts        INTEGER NOT NULL,  -- unix seconds, UTC
  value     REAL,
  PRIMARY KEY (plant_id, point_id, ts)
) WITHOUT ROWID;
CREATE TABLE raw_response (
  id           INTEGER PRIMARY KEY,
  endpoint     TEXT NOT NULL,
  window_start INTEGER,
  window_end   INTEGER,
  fetched_at   INTEGER NOT NULL,
  body         TEXT NOT NULL
);
CREATE TABLE sync_state (
  job        TEXT PRIMARY KEY,
  cursor_ts  INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE api_call (
  id         INTEGER PRIMARY KEY,
  endpoint   TEXT NOT NULL,
  called_at  INTEGER NOT NULL,
  ok         INTEGER NOT NULL
);
CREATE INDEX api_call_called_at ON api_call (called_at);
