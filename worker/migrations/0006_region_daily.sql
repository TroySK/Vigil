-- 0006: per-region daily rollup.
--
-- The detail page's regional sparklines need one point per origin per day
-- for 30/90-day ranges. Reading raw samples for that window scanned tens of
-- thousands of rows (and the global stride could drop whole regions). Keep
-- the newest sample per (monitor, origin, UTC day), maintained by the tick
-- alongside `monitor_daily`, so long-range reads stay bounded by days.
CREATE TABLE IF NOT EXISTS region_daily (
  id TEXT NOT NULL,
  place TEXT NOT NULL,
  day INTEGER NOT NULL,
  at INTEGER NOT NULL,
  ms INTEGER NOT NULL,
  up INTEGER NOT NULL,
  PRIMARY KEY (id, place, day)
);

INSERT OR REPLACE INTO region_daily (id, place, day, at, ms, up)
SELECT id, place, (at / 86400) * 86400 AS day, MAX(at) AS at, ms, up
FROM samples
GROUP BY id, place, day;
