-- 0005: per-region latest sample.
--
-- 0004's (id, place, at DESC) index made MAX(at) per place index-only, but
-- the detail page still walked every retained sample for that monitor on
-- each render (up to 25,920 rows at 90 days x 5-minute samples). Keep one
-- row per (id, place), upserted by the tick whenever it writes a sample, so
-- the read is a single primary-key lookup.
CREATE TABLE IF NOT EXISTS region_latest (
  id TEXT NOT NULL,
  place TEXT NOT NULL,
  at INTEGER NOT NULL,
  ms INTEGER NOT NULL,
  up INTEGER NOT NULL,
  PRIMARY KEY (id, place)
);

-- Bare columns with MAX(at) take their values from each group's newest row
-- (SQLite's documented min/max bare-column behavior), so existing history
-- starts with the same row the old scan returned.
INSERT OR REPLACE INTO region_latest (id, place, at, ms, up)
SELECT id, place, MAX(at), ms, up
FROM samples
GROUP BY id, place;
