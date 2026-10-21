-- Login sessions. The signed cookie carries only the random `id`; the server
-- decides whether it is still valid, so logging out (deleting the row) really
-- revokes it. Expiry slides: `last_seen` is bumped at most once an hour.
CREATE TABLE session (
  id          TEXT PRIMARY KEY,  -- 128 random bits, hex
  created_at  INTEGER NOT NULL,  -- unix seconds, UTC
  last_seen   INTEGER NOT NULL,  -- unix seconds, UTC
  expires_at  INTEGER NOT NULL   -- unix seconds, UTC
) WITHOUT ROWID;
