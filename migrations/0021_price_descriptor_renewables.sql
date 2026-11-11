-- Amber's own labels for an interval: `descriptor` (extremelyLow, veryLow,
-- low, neutral, high, spike) and `renewables` (percent of the grid that is
-- renewable). Both are NULL for sources or rows that do not have them.
ALTER TABLE price_interval ADD COLUMN descriptor TEXT;
ALTER TABLE price_interval ADD COLUMN renewables REAL;
