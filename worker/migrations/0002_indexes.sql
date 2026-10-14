-- 0002: covering indexes for the request-path reads.
--
-- 0001 shipped only idx_samples/id+at and idx_gaps/id+at, while Db::init
-- (worker/src/db.rs) later added three more. Databases migrated via CI
-- never ran init on the request path, so prod kept full-scanning:
-- samples-by-time, gaps-by-upto, and open-gap lookups all missed an index
-- until the hourly tick self-heal ran. This migration closes that gap at
-- deploy time (one DDL hit during migration, not per-request scans after).
CREATE INDEX IF NOT EXISTS idx_samples_at ON samples (at);
CREATE INDEX IF NOT EXISTS idx_gaps_upto ON gaps (upto);
CREATE INDEX IF NOT EXISTS idx_gaps_open ON gaps (id, upto, at DESC);
