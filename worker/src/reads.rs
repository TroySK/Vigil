//! Typed D1 read models: gap rows and latency samples.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use worker::Result;

use crate::db::{num, Db};
use vigil_core::state::{Gap, GapSpan};

/// One downtime window row for API responses and rollups.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GapOut {
    /// Start (unix seconds).
    pub at: i64,
    /// End (unix seconds), or `None` while ongoing.
    /// Deserialized from the `upto` column (`to` is reserved in SQLite, so
    /// the query selects `upto` directly); serialized as `to` for the API.
    #[serde(alias = "upto")]
    pub to: Option<i64>,
    /// Human-readable cause.
    pub why: String,
}

impl GapSpan for GapOut {
    fn at(&self) -> i64 {
        self.at
    }
    fn to(&self) -> Option<i64> {
        self.to
    }
}

/// One probe sample row for API responses and charts.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SampleOut {
    /// Sample time (unix seconds).
    pub at: i64,
    /// Probe duration in milliseconds.
    pub ms: i64,
    /// `1` = up, `0` = down.
    pub up: i64,
    /// Origin colo (or `"CRON"` when unknown).
    pub place: String,
}

/// Most recent gaps for a monitor, newest first.
pub async fn recent_gaps(db: &Db, id: &str, limit: i64) -> Result<Vec<GapOut>> {
    db.q("SELECT at, upto, why FROM gaps WHERE id = ?1 ORDER BY at DESC LIMIT ?2")
        .bind(&[id.into(), num(limit.max(1))])?
        .all()
        .await?
        .results()
}

/// Latest sample for a monitor, if any.
pub async fn latest(db: &Db, id: &str) -> Result<Option<SampleOut>> {
    let rows: Vec<SampleOut> = db
        .q("SELECT at, ms, up, place FROM samples WHERE id = ?1 ORDER BY at DESC LIMIT 1")
        .bind(&[id.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().next())
}

/// Recent samples for a monitor, newest first.
pub async fn series(db: &Db, id: &str, limit: i64) -> Result<Vec<SampleOut>> {
    db.q("SELECT at, ms, up, place FROM samples WHERE id = ?1 ORDER BY at DESC LIMIT ?2")
        .bind(&[id.into(), num(limit.max(1))])?
        .all()
        .await?
        .results()
}

/// `?1, ?2, …` placeholders for an `IN` list. Callers with no ids skip
/// their query instead (empty `IN ()` is invalid SQL).
fn placeholders(n: usize) -> String {
    (0..n)
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",")
}

/// Bind `ids` in order for an `IN` list query.
fn bind_ids(ids: &[&str]) -> Vec<wasm_bindgen::JsValue> {
    ids.iter().map(|id| (*id).into()).collect()
}

/// Newest sample per monitor, keyed by monitor id, from batched round
/// trips (one query per [`MAX_PARAMS`](crate::db::MAX_PARAMS)-id chunk).
/// Each id gets its own indexed `ORDER BY at DESC LIMIT 1`: a grouped
/// `MAX(at)` would scan every retained sample for that monitor (64,800
/// rows at 30 monitors × 90 days) to return one row. The earliest sample
/// comes from the `first:` kv stamps (see [`effective_since`]), so this
/// query never scans the full retention window.
pub async fn latest_by_monitor(db: &Db, ids: &[&str]) -> Result<BTreeMap<String, SampleOut>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        at: i64,
        ms: i64,
        up: i64,
        place: String,
    }
    let mut latest = BTreeMap::new();
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let rows: Vec<Row> = db
            .q(&latest_sql(chunk.len()))
            .bind(&bind_ids(chunk))?
            .all()
            .await?
            .results()?;
        for row in rows {
            latest.insert(
                row.id.clone(),
                SampleOut {
                    at: row.at,
                    ms: row.ms,
                    up: row.up,
                    place: row.place.clone(),
                },
            );
        }
    }
    Ok(latest)
}

/// Exact query behind [`latest_by_monitor`], exposed so tests execute the
/// shipped text against bundled SQLite. `?1..?N` monitor ids. Each branch
/// is wrapped so its `LIMIT 1` applies per monitor, not to the compound.
fn latest_sql(n: usize) -> String {
    (1..=n)
        .map(|i| {
            format!(
                "SELECT id, at, ms, up, place FROM (SELECT id, at, ms, up, place FROM samples WHERE id = ?{i} ORDER BY at DESC LIMIT 1)"
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// Exact query behind [`gaps_window_by_monitor`], exposed so tests execute
/// the shipped text against bundled SQLite. `?1..?N` monitor ids, then
/// cutoff, then now: only gaps touching `[cutoff, now]` leave D1, so a
/// future-dated row (clock skew, bad write) never burns rows-read quota
/// or paints phantom downtime on the highest-traffic page.
fn gaps_window_sql(n: usize) -> String {
    format!(
        "SELECT id, at, upto, why FROM gaps WHERE id IN ({}) AND at <= ?{} AND (upto IS NULL OR upto >= ?{})",
        placeholders(n),
        n + 2,
        n + 1,
    )
}
/// Gaps overlapping `[cutoff, now]` per monitor, keyed by id. Older closed
/// gaps cannot affect history cells, so they never leave D1.
pub async fn gaps_window_by_monitor(
    db: &Db,
    ids: &[&str],
    cutoff: i64,
    now: i64,
) -> Result<BTreeMap<String, Vec<Gap>>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        at: i64,
        #[serde(alias = "upto")]
        to: Option<i64>,
        why: String,
    }
    let mut out: BTreeMap<String, Vec<Gap>> = BTreeMap::new();
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let mut binds = bind_ids(chunk);
        binds.push(num(cutoff));
        binds.push(num(now));
        let rows: Vec<Row> = db
            .q(&gaps_window_sql(chunk.len()))
            .bind(&binds)?
            .all()
            .await?
            .results()?;
        for row in rows {
            out.entry(row.id.clone()).or_default().push(Gap {
                at: row.at,
                to: row.to,
                why: row.why.clone(),
            });
        }
    }
    Ok(out)
}

/// Earliest retained sample per monitor, keyed by id: the tick's one-time
/// backfill for `first:` kv stamps on pre-existing fleets (see
/// [`effective_since`]). Days before it render as "no data", never as
/// 100% uptime.
pub async fn first_by_monitor(db: &Db, ids: &[&str]) -> Result<BTreeMap<String, i64>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        m: i64,
    }
    let mut out = BTreeMap::new();
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let rows: Vec<Row> = db
            .q(&format!(
                "SELECT id, MIN(at) AS m FROM samples WHERE id IN ({}) GROUP BY id",
                placeholders(chunk.len())
            ))
            .bind(&bind_ids(chunk))?
            .all()
            .await?
            .results()?;
        for row in rows {
            out.insert(row.id, row.m);
        }
    }
    Ok(out)
}

/// Exact query behind [`recent_gaps_by_monitor`], exposed so tests execute
/// the shipped text against bundled SQLite. `?1` cutoff, `?2` limit, then
/// `?3..?N` monitor ids. Each monitor gets its own indexed
/// `ORDER BY at DESC LIMIT` subquery: ranking the whole window with
/// `ROW_NUMBER()` read every gap a flapping monitor ever opened before
/// discarding all but the newest `limit`.
fn recent_gaps_window_sql(n: usize) -> String {
    (1..=n)
        .map(|i| {
            format!(
                "SELECT id, at, upto, why FROM (SELECT id, at, upto, why FROM gaps WHERE id = ?{} AND (upto IS NULL OR upto >= ?1) ORDER BY at DESC LIMIT ?2)",
                i + 2
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// Up to `limit` newest gaps per monitor inside `[cutoff, +inf)`, keyed by
/// id, newest first. One union query replaces a round trip per monitor;
/// predated closures never leave D1, and each monitor's index scan stops
/// after its own `limit` rows.
pub async fn recent_gaps_by_monitor(
    db: &Db,
    ids: &[&str],
    limit: i64,
    cutoff: i64,
) -> Result<BTreeMap<String, Vec<Gap>>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        at: i64,
        #[serde(alias = "upto")]
        to: Option<i64>,
        why: String,
    }
    let mut out: BTreeMap<String, Vec<Gap>> = BTreeMap::new();
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let rows: Vec<Row> = db
            .q(&recent_gaps_window_sql(chunk.len()))
            .bind(
                &[num(cutoff), num(limit.max(1))]
                    .into_iter()
                    .chain(bind_ids(chunk))
                    .collect::<Vec<_>>(),
            )?
            .all()
            .await?
            .results()?;
        for row in rows {
            out.entry(row.id.clone()).or_default().push(Gap {
                at: row.at,
                to: row.to,
                why: row.why.clone(),
            });
        }
    }
    Ok(out)
}

/// Exact query behind [`series_by_monitor`], exposed so tests execute the
/// shipped text against bundled SQLite. `?1..?N` monitor ids, then cutoff,
/// `max_points - 1`, `max_points`: the stride derives from each monitor's
/// own window count, so sparse and dense monitors alike spread at most
/// `max_points` points over the full window, newest included.
fn series_window_sql(n: usize) -> String {
    format!(
        "SELECT id, at, ms, up, place FROM (SELECT id, at, ms, up, place, ROW_NUMBER() OVER (PARTITION BY id ORDER BY at DESC) AS rn, COUNT(*) OVER (PARTITION BY id) AS cnt FROM samples WHERE id IN ({}) AND at >= ?{}) WHERE (rn - 1) % ((cnt + ?{}) / ?{}) = 0 ORDER BY id ASC, at DESC",
        placeholders(n),
        n + 1,
        n + 2,
        n + 3,
    )
}

/// Up to `max_points` samples per monitor inside `[cutoff, +inf)`, keyed
/// by id, newest first. One window-function query replaces a round trip
/// per monitor.
pub async fn series_by_monitor(
    db: &Db,
    ids: &[&str],
    cutoff: i64,
    max_points: i64,
) -> Result<BTreeMap<String, Vec<SampleOut>>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        at: i64,
        ms: i64,
        up: i64,
        place: String,
    }
    let mut out: BTreeMap<String, Vec<SampleOut>> = BTreeMap::new();
    let max_points = max_points.max(1);
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let rows: Vec<Row> = db
            .q(&series_window_sql(chunk.len()))
            .bind(
                &bind_ids(chunk)
                    .into_iter()
                    .chain([num(cutoff), num(max_points - 1), num(max_points)])
                    .collect::<Vec<_>>(),
            )?
            .all()
            .await?
            .results()?;
        for row in rows {
            out.entry(row.id.clone()).or_default().push(SampleOut {
                at: row.at,
                ms: row.ms,
                up: row.up,
                place: row.place.clone(),
            });
        }
    }
    Ok(out)
}

/// Last tick that probed at least one monitor, falling back to the tick
/// clock when no probe has ever landed (fresh database): pages then keep
/// reloading on ticks until the first real data arrives.
#[must_use]
pub fn data_at_of(stored: Option<String>, at: i64) -> i64 {
    stored.and_then(|s| s.parse().ok()).unwrap_or(at)
}

/// Bar-history cutoff from a `first:` kv stamp: days before the first
/// sample render as "no data", never as 100% uptime. Stamps older than
/// retention are clamped to the retention edge — samples there are pruned,
/// so an unclamped stamp would paint pre-retention days as perfect.
#[must_use]
pub fn effective_since(first: Option<i64>, now: i64) -> Option<i64> {
    first.map(|at| at.max(now.saturating_sub(crate::db::RETENTION_SECS)))
}

/// `(tick clock, counts, probe clock)` for pages and APIs from one
/// batched `kv` read (was: three sequential round trips). `counts` is
/// `None` before the first tick; callers fall back to `default_counts`.
pub async fn clock(db: &Db) -> Result<(i64, Option<String>, i64)> {
    let keys: Vec<String> = crate::db::clock_keys()
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    let map = db.kv_map(&keys).await?;
    let at = map.get("at").and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((
        at,
        map.get("counts").cloned(),
        data_at_of(map.get("data_at").cloned(), at),
    ))
}

/// Exact query behind [`series_ranged`], exposed so tests execute the
/// shipped text against bundled SQLite (`?1` id, `?2` cutoff,
/// `?3` `max_points - 1`, `?4` `max_points`). The stride derives from
/// `COUNT(*) OVER ()` inline, so one round trip covers the window: a
/// separate `COUNT(*)` doubled D1 tail latency on every detail load.
/// Rows are 1-based recency ranks: `(rn - 1) % stride = 0` keeps the
/// newest sample plus every stride-th one after it. (A bare
/// `rn % stride = 1` matches nothing when stride is 1 and once emptied
/// every short-range chart in production.)
fn series_ranged_sql() -> &'static str {
    "SELECT at, ms, up, place FROM (SELECT at, ms, up, place, ROW_NUMBER() OVER (ORDER BY at DESC) AS rn, COUNT(*) OVER () AS cnt FROM samples WHERE id = ?1 AND at >= ?2) WHERE (rn - 1) % ((cnt + ?3) / ?4) = 0 ORDER BY at DESC LIMIT ?4"
}

/// Samples inside `[cutoff, +inf)` spread over at most `max_points` chart
/// points, newest first, from a single D1 round trip. Short ranges keep
/// every sample; long ranges (where cadence × span reaches tens of
/// thousands of rows) take every Nth row by recency rank, so the chart
/// always spans the full window honestly.
pub async fn series_ranged(
    db: &Db,
    id: &str,
    cutoff: i64,
    max_points: i64,
) -> Result<Vec<SampleOut>> {
    let max_points = max_points.max(1);
    db.q(series_ranged_sql())
        .bind(&[id.into(), num(cutoff), num(max_points - 1), num(max_points)])?
        .all()
        .await?
        .results()
}

/// Newest sample per probe origin, newest first. SQLite resolves the bare
/// `ms`/`up` columns to the `MAX(at)` row within each group (documented
/// bare-column behavior with a single min/max aggregate), so each row is
/// that region's true latest sample.
pub async fn region_latest(db: &Db, id: &str) -> Result<Vec<SampleOut>> {
    db.q("SELECT MAX(at) AS at, ms, up, place FROM samples WHERE id = ?1 GROUP BY place ORDER BY at DESC")
        .bind(&[id.into()])?
        .all()
        .await?
        .results()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    use super::{
        data_at_of, effective_since, gaps_window_sql, latest_sql, recent_gaps_window_sql,
        series_ranged_sql, series_window_sql,
    };
    use rusqlite::types::Value;
    use rusqlite::{params, params_from_iter};
    use std::collections::BTreeMap;

    #[test]
    fn probed_clock_falls_back_to_tick_clock() {
        assert_eq!(data_at_of(Some("bogus".to_owned()), 2_000), 2_000);
        assert_eq!(data_at_of(Some("1500".to_owned()), 2_000), 1_500);
        assert_eq!(data_at_of(None, 2_000), 2_000);
    }

    #[test]
    fn latest_sql_seeks_one_newest_row_per_id() {
        // Health needs only the newest sample per monitor. A grouped MAX
        // scanned every retained row; one indexed ORDER BY ... LIMIT 1 per
        // id makes that an index seek instead.
        let sql = latest_sql(2);
        assert!(sql.contains("UNION ALL"));
        assert_eq!(sql.matches("ORDER BY at DESC LIMIT 1").count(), 2);
        assert!(!sql.contains("GROUP BY"));
        assert!(!sql.contains("MAX("));
        assert!(!sql.contains("MIN("));
    }

    #[test]
    fn latest_only_reads_chunk_within_param_budget() {
        // 150 ids exceed one query's param budget: split into chunks and
        // merge, exactly like `latest_by_monitor` must. Each id has a
        // history: the query must return its newest row, not the first.
        let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);
             CREATE INDEX idx_samples ON samples (id, at);",
        )
        .unwrap();
        for (i, id) in ids.iter().enumerate() {
            for step in 0..3_i64 {
                conn.execute(
                    "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id,
                        1_000.0 + i as f64 * 10.0 + step as f64,
                        50.0 + step as f64,
                        1.0,
                        "SIN"
                    ],
                )
                .unwrap();
            }
        }
        let mut found = 0_usize;
        let mut newest = 0_i64;
        let mut latest = BTreeMap::new();
        for chunk in ids.chunks(crate::db::MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= crate::db::MAX_PARAMS);
            let mut stmt = conn.prepare(&latest_sql(chunk.len())).unwrap();
            let mut rows = stmt.query(params_from_iter(id_binds(chunk, &[]))).unwrap();
            while let Some(row) = rows.next().unwrap() {
                found += 1;
                let id: String = row.get(0).unwrap();
                let at: i64 = row.get(1).unwrap();
                let ms: i64 = row.get(2).unwrap();
                newest = newest.max(at);
                latest.insert(id, (at, ms));
            }
        }
        assert_eq!(found, 150);
        assert_eq!(newest, 2_492);
        assert_eq!(latest.get("m0"), Some(&(1_002, 52)));
        assert_eq!(latest.get("m149"), Some(&(2_492, 52)));
    }

    #[test]
    fn latest_sql_has_no_full_sample_scan() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);
             CREATE INDEX idx_samples ON samples (id, at);",
        )
        .unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", latest_sql(2)))
            .unwrap()
            .query_map(params!["a", "b"], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("SEARCH samples USING INDEX idx_samples"),
            "latest must use the (id, at) index, got: {plan}"
        );
        assert!(
            !plan.contains("SCAN samples"),
            "latest must not table-scan: {plan}"
        );
    }

    #[test]
    fn effective_since_clamps_to_retention() {
        let now = 10_000_000_i64;
        // Fresh monitor: no clamp.
        assert_eq!(effective_since(Some(now - 100), now), Some(now - 100));
        // Older than retention: clamp to the retention edge so
        // pre-retention days render as data, never as phantom history.
        assert_eq!(
            effective_since(Some(1), now),
            Some(now - crate::db::RETENTION_SECS)
        );
        assert_eq!(effective_since(None, now), None);
    }

    /// Bind `ids` plus trailing float params the way the batched readers
    /// do (D1 sees every integer as a float via [`crate::db::num`]).
    fn id_binds(ids: &[String], extra: &[f64]) -> Vec<Value> {
        ids.iter()
            .map(|id| Value::Text(id.clone()))
            .chain(extra.iter().map(|f| Value::Real(*f)))
            .collect()
    }

    #[test]
    fn gaps_window_reads_chunk_within_param_budget() {
        let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        for id in &ids {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                params![id, 500.0, Option::<f64>::None, "x"],
            )
            .unwrap();
        }
        let mut found = 0_usize;
        for chunk in ids.chunks(crate::db::MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= crate::db::MAX_PARAMS);
            let mut stmt = conn.prepare(&gaps_window_sql(chunk.len())).unwrap();
            // Binds are ids, then cutoff, then now (see `gaps_window_sql`).
            let mut rows = stmt
                .query(params_from_iter(id_binds(chunk, &[100.0, 1_000.0])))
                .unwrap();
            while rows.next().unwrap().is_some() {
                found += 1;
            }
        }
        assert_eq!(found, 150);
    }

    #[test]
    fn recent_gaps_sql_limits_before_returning_rows() {
        // Per-monitor LIMIT must run inside each indexed branch. The old
        // ROW_NUMBER() ranked every matching gap before applying the cap.
        let sql = recent_gaps_window_sql(2);
        assert!(sql.contains("UNION ALL"));
        assert_eq!(sql.matches("ORDER BY at DESC LIMIT ?2").count(), 2);
        assert!(!sql.contains("ROW_NUMBER"));
        assert!(!sql.contains("PARTITION BY"));
    }

    #[test]
    fn recent_gaps_window_reads_chunk_within_param_budget() {
        let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);
             CREATE INDEX idx_gaps ON gaps (id, at);",
        )
        .unwrap();
        for id in &ids {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                params![id, 500.0, Option::<f64>::None, "x"],
            )
            .unwrap();
        }
        let mut found = 0_usize;
        for chunk in ids.chunks(crate::db::MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= crate::db::MAX_PARAMS);
            let mut stmt = conn.prepare(&recent_gaps_window_sql(chunk.len())).unwrap();
            // Binds are cutoff, limit, then ids.
            let mut binds = vec![Value::Real(100.0), Value::Real(500.0)];
            binds.extend(id_binds(chunk, &[]));
            let mut rows = stmt.query(params_from_iter(binds)).unwrap();
            while rows.next().unwrap().is_some() {
                found += 1;
            }
        }
        assert_eq!(found, 150);
    }

    #[test]
    fn windowed_series_reads_chunk_within_param_budget() {
        let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);",
        )
        .unwrap();
        for id in &ids {
            for at in [1_000.0, 2_000.0, 3_000.0] {
                conn.execute(
                    "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, at, 50.0, 1.0, "SIN"],
                )
                .unwrap();
            }
        }
        let mut found = 0_usize;
        for chunk in ids.chunks(crate::db::MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= crate::db::MAX_PARAMS);
            let mut stmt = conn.prepare(&series_window_sql(chunk.len())).unwrap();
            // Binds are ids, then cutoff, max_points - 1, max_points:
            // 3 rows over max 30 keep every row (stride 1).
            let mut rows = stmt
                .query(params_from_iter(id_binds(chunk, &[0.0, 29.0, 30.0])))
                .unwrap();
            while rows.next().unwrap().is_some() {
                found += 1;
            }
        }
        assert_eq!(found, 450);
    }

    /// Seeded SQLite mirror of the D1 schema (D1 is SQLite; unit tests
    /// have no D1). Binds floats like [`crate::db::num`] so affinity edge
    /// cases execute exactly as in production.
    #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    fn seed_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);
             CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        // m1: 300 samples on a 5-minute cadence ending at NOW.
        let now = 1_800_000_000_i64;
        for i in 0..300 {
            conn.execute(
                "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "m1",
                    (now - i as i64 * 300) as f64,
                    50.0 + (i % 7) as f64,
                    1.0,
                    "BOM"
                ],
            )
            .unwrap();
        }
        // m3: 2,016 samples for stride-spread checks.
        for i in 0..2016 {
            conn.execute(
                "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["m3", (now - i as i64 * 300) as f64, 50.0, 1.0, "SIN"],
            )
            .unwrap();
        }
        // m2: three stale samples plus two regions' latest rows.
        for (at, ms, place) in [(1_000, 10, "SIN"), (2_000, 20, "SIN"), (3_000, 99, "BOM")] {
            conn.execute(
                "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["m2", at as f64, ms as f64, 1.0, place],
            )
            .unwrap();
        }
        // Five gaps for m1, newest last.
        for (at, upto) in [
            (100, None),
            (200, Some(250)),
            (300, None),
            (400, Some(450)),
            (500, None),
        ] {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                params!["m1", at as f64, upto.map(|u| u as f64), "x"],
            )
            .unwrap();
        }
        conn
    }

    fn col0(conn: &rusqlite::Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Vec<i64> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map(args, |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn ranged_series_covers_full_window_at_stride_one() {
        // Regression: `rn % stride = 1` matches nothing when stride is 1
        // (`rn % 1` is always 0), emptying every short-range chart.
        let conn = seed_db();
        let ats = col0(
            &conn,
            series_ranged_sql(),
            &[
                &"m1",
                &((1_800_000_000_i64 - 90 * 86_400) as f64),
                &299.0,
                &300.0,
            ],
        );
        assert_eq!(ats.len(), 300);
        assert_eq!(ats[0], 1_800_000_000);
    }

    #[test]
    fn ranged_series_derives_stride_in_single_round_trip() {
        // P99 fix: the stride must come from COUNT(*) OVER () inside one
        // query, not a separate COUNT(*) round trip. Binds are
        // (?1 id, ?2 cutoff, ?3 max_points - 1, ?4 max_points), mirroring
        // series_window_sql; 2,016 rows over max 300 → stride 7 → 288 pts.
        let conn = seed_db();
        let ats = col0(&conn, series_ranged_sql(), &[&"m3", &0.0, &299.0, &300.0]);
        assert_eq!(ats.len(), 288);
        assert_eq!(ats[0], 1_800_000_000);
        let steps: std::collections::BTreeSet<i64> = ats.windows(2).map(|w| w[0] - w[1]).collect();
        assert_eq!(steps.into_iter().collect::<Vec<_>>(), vec![2_100]);
    }

    #[test]
    fn gaps_window_excludes_predated_and_future_rows() {
        // Homepage query: ongoing + straddling gaps pass; predated closures
        // and future-dated rows do not. Binds are (?1 id, ?2 cutoff, ?3 now).
        let conn = seed_db();
        conn.execute(
            "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
            params!["m1", 5_000.0, Option::<f64>::None, "future"],
        )
        .unwrap();
        let mut stmt = conn.prepare(&gaps_window_sql(1)).unwrap();
        let mut ats: Vec<i64> = stmt
            .query_map(params!["m1", 350.0, 1_000.0], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        ats.sort_unstable();
        // (200, 250) predates the window; (5000, NULL) starts after now.
        assert_eq!(ats, vec![100, 300, 400, 500]);
    }

    #[test]
    fn recent_gaps_window_excludes_predated_closures() {
        let conn = seed_db();
        let mut stmt = conn.prepare(&recent_gaps_window_sql(1)).unwrap();
        let ats: Vec<i64> = stmt
            .query_map(params![350.0, 500.0, "m1"], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Cutoff 350, limit 500: open gaps plus closures ending at/after
        // it, newest first. (200, 250) predates the window.
        assert_eq!(ats, vec![500, 400, 300, 100]);
    }

    #[test]
    fn windowed_series_respects_selected_range_cutoff() {
        // Homepage sparklines follow the selected range: samples before
        // the caller's cutoff (a 24h pulse, for example) must not leak in.
        let conn = seed_db();
        let mut stmt = conn.prepare(&series_window_sql(1)).unwrap();
        let ats: Vec<i64> = stmt
            .query_map(params!["m2", 2_500.0, 29.0, 30.0], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ats, vec![3_000]);
    }

    #[test]
    fn windowed_series_strides_each_monitor_to_thirty_points() {
        let conn = seed_db();
        let mut stmt = conn.prepare(&series_window_sql(2)).unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map(params!["m1", "m3", 0.0, 29.0, 30.0], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 60);
        // Per-id stride from the window count: 300 rows → every 10th.
        let (m1, m3) = (rows[0..30].to_vec(), rows[30..60].to_vec());
        assert!(m1.iter().all(|(id, _)| id == "m1"));
        assert!(m3.iter().all(|(id, _)| id == "m3"));
        assert_eq!(m1[0].1, 1_800_000_000);
        assert_eq!(m3[0].1, 1_800_000_000);
        let steps: std::collections::BTreeSet<i64> =
            m1.windows(2).map(|w| w[0].1 - w[1].1).collect();
        assert_eq!(steps.into_iter().collect::<Vec<_>>(), vec![3_000]);
        assert_eq!(m3.len(), 30);
    }
}
