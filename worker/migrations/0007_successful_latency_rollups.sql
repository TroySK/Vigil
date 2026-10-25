-- 0007: rebuild daily latency rollups from successful samples only.
--
-- 0003/0006 kept the newest sample per day regardless of outcome. A failed
-- probe records time-to-failure, not latency, so a down sample at the end
-- of a day could mask that day's last successful latency. Charts and
-- percentiles now read successful samples only; rebuild the rollups so
-- historical days keep their newest successful point (or disappear when
-- the whole day failed).
DELETE FROM monitor_daily;

INSERT INTO monitor_daily (id, day, at, ms, up, place)
SELECT id, (at / 86400) * 86400 AS day, MAX(at) AS at, ms, up, place
FROM samples
WHERE up = 1
GROUP BY id, day;

DELETE FROM region_daily;

INSERT INTO region_daily (id, place, day, at, ms, up)
SELECT id, place, (at / 86400) * 86400 AS day, MAX(at) AS at, ms, up
FROM samples
WHERE up = 1
GROUP BY id, place, day;
