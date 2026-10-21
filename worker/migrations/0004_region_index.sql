-- 0004: regional-latest index.
--
-- worker/src/reads.rs::region_latest groups one monitor's samples by
-- `place` and takes MAX(at) per group. With only idx_samples (id, at),
-- SQLite scanned every sample the monitor ever recorded to find each
-- region's newest row. (id, place, at DESC) keeps that lookup index-only.
CREATE INDEX IF NOT EXISTS idx_samples_region ON samples (id, place, at DESC);
