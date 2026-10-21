-- 0003: daily latency rollup for long-range sparklines.
--
-- The homepage asked `samples` for every row in a 90-day window and then
-- ranked/counted all of them just to keep 24 chart points. At hourly
-- cadence that read 64,800 rows; at 5-minute cadence, 777,600. This table
-- keeps one representative sample per monitor per UTC day, maintained by
-- the tick whenever it writes a `samples` row.
--
-- Backfill uses SQLite's bare-column rule with MAX(at): `ms`, `up`, and
-- `place` come from each (id, day) group's newest row, so existing
-- history starts with the same value it would have had on the next tick.
CREATE TABLE IF NOT EXISTS monitor_daily (
  id TEXT NOT NULL,
  day INTEGER NOT NULL,
  at INTEGER NOT NULL,
  ms INTEGER NOT NULL,
  up INTEGER NOT NULL,
  place TEXT NOT NULL,
  PRIMARY KEY (id, day)
);

INSERT OR IGNORE INTO monitor_daily (id, day, at, ms, up, place)
SELECT id, (at / 86400) * 86400 AS day, MAX(at) AS at, ms, up, place
FROM samples
GROUP BY id, day;
