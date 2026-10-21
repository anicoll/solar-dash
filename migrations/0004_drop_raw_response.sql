-- Nothing reads the raw API responses, and they made the database and its
-- backups grow for no benefit.
DROP TABLE raw_response;
