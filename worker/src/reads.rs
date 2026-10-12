//! Typed D1 read models: gap rows and latency samples.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use worker::Result;

use crate::db::{num, Db};
use vigil_core::state::Gap;

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

/// Earliest recorded sample for a monitor, if any.
pub async fn first_sample_at(db: &Db, id: &str) -> Result<Option<i64>> {
    // `MIN` over zero rows yields one `NULL` row, so read an optional
    // integer and flatten: missing monitor and empty history both map
    // to `None` instead of erroring on `NULL`.
    let row: Option<Option<i64>> = db
        .q("SELECT MIN(at) AS m FROM samples WHERE id = ?1")
        .bind(&[id.into()])?
        .first(Some("m"))
        .await?;
    Ok(row.flatten())
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

/// Newest sample per monitor, keyed by monitor id, from one round trip.
/// SQLite resolves the bare `ms`/`up`/`place` columns to the `MAX(at)` row
/// within each group (documented bare-column behavior with a single
/// min/max aggregate).
pub async fn latest_by_monitor(db: &Db, ids: &[&str]) -> Result<BTreeMap<String, SampleOut>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        at: i64,
        ms: i64,
        up: i64,
        place: String,
    }
    let mut out = BTreeMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<Row> = db
        .q(&format!(
            "SELECT id, MAX(at) AS at, ms, up, place FROM samples WHERE id IN ({}) GROUP BY id",
            placeholders(ids.len())
        ))
        .bind(&bind_ids(ids))?
        .all()
        .await?
        .results()?;
    for row in rows {
        out.insert(
            row.id.clone(),
            SampleOut {
                at: row.at,
                ms: row.ms,
                up: row.up,
                place: row.place.clone(),
            },
        );
    }
    Ok(out)
}

/// Gaps overlapping `[cutoff, now]` per monitor, keyed by id. Older closed
/// gaps cannot affect history cells, so they never leave D1.
pub async fn gaps_window_by_monitor(
    db: &Db,
    ids: &[&str],
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
    if ids.is_empty() {
        return Ok(out);
    }
    let mut binds = bind_ids(ids);
    binds.push(num(cutoff));
    let rows: Vec<Row> = db
        .q(&format!(
            "SELECT id, at, upto, why FROM gaps WHERE id IN ({}) AND (upto IS NULL OR upto >= ?{})",
            placeholders(ids.len()),
            ids.len() + 1
        ))
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
    Ok(out)
}

/// Earliest sample per monitor, keyed by id: days before it render as
/// "no data", never as 100% uptime.
pub async fn first_by_monitor(db: &Db, ids: &[&str]) -> Result<BTreeMap<String, i64>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        id: String,
        m: i64,
    }
    let mut out = BTreeMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<Row> = db
        .q(&format!(
            "SELECT id, MIN(at) AS m FROM samples WHERE id IN ({}) GROUP BY id",
            placeholders(ids.len())
        ))
        .bind(&bind_ids(ids))?
        .all()
        .await?
        .results()?;
    for row in rows {
        out.insert(row.id, row.m);
    }
    Ok(out)
}

/// Up to `limit` newest gaps per monitor, keyed by id, newest first. One
/// window-function query replaces a round trip per monitor.
pub async fn recent_gaps_by_monitor(
    db: &Db,
    ids: &[&str],
    limit: i64,
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
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<Row> = db
        .q(&format!(
            "SELECT id, at, upto, why FROM (SELECT id, at, upto, why, ROW_NUMBER() OVER (PARTITION BY id ORDER BY at DESC) AS rn FROM gaps WHERE id IN ({})) WHERE rn <= ?{} ORDER BY id ASC, at DESC",
            placeholders(ids.len()),
            ids.len() + 1
        ))
        .bind(
            &bind_ids(ids)
                .into_iter()
                .chain(std::iter::once(num(limit.max(1))))
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
    if ids.is_empty() {
        return Ok(out);
    }
    let max_points = max_points.max(1);
    let rows: Vec<Row> = db
        .q(&series_window_sql(ids.len()))
        .bind(
            &bind_ids(ids)
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
    Ok(out)
}

/// Row stride spreading a window of `count` samples over at most
/// `max_points` chart points: full fidelity while the window fits, every
/// Nth row by recency rank beyond that.
#[must_use]
pub fn stride_for(count: i64, max_points: i64) -> i64 {
    let max_points = max_points.max(1);
    ((count.max(0) + max_points - 1) / max_points).max(1)
}

/// Exact query behind [`series_ranged`], exposed so tests execute the
/// shipped text against bundled SQLite (`?1` id, `?2` cutoff, `?3` stride,
/// `?4` limit). Rows are 1-based recency ranks: `(rn - 1) % stride = 0`
/// keeps the newest sample plus every stride-th one after it. (A bare
/// `rn % stride = 1` matches nothing when stride is 1 and once emptied
/// every short-range chart in production.)
fn series_ranged_sql() -> &'static str {
    "SELECT at, ms, up, place FROM (SELECT at, ms, up, place, ROW_NUMBER() OVER (ORDER BY at DESC) AS rn FROM samples WHERE id = ?1 AND at >= ?2) WHERE (rn - 1) % ?3 = 0 ORDER BY at DESC LIMIT ?4"
}

/// Samples inside `[cutoff, +inf)` spread over at most `max_points` chart
/// points, newest first. Short ranges keep every sample; long ranges (where
/// cadence × span reaches tens of thousands of rows) take every Nth row by
/// recency rank, so the chart always spans the full window honestly.
pub async fn series_ranged(
    db: &Db,
    id: &str,
    cutoff: i64,
    max_points: i64,
) -> Result<Vec<SampleOut>> {
    let max_points = max_points.max(1);
    let count: Option<i64> = db
        .q("SELECT COUNT(*) AS n FROM samples WHERE id = ?1 AND at >= ?2")
        .bind(&[id.into(), num(cutoff)])?
        .first(Some("n"))
        .await?;
    let stride = stride_for(count.unwrap_or(0), max_points).max(1);
    db.q(series_ranged_sql())
        .bind(&[id.into(), num(cutoff), num(stride), num(max_points)])?
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
    use super::{series_ranged_sql, series_window_sql, stride_for};
    use rusqlite::params;

    #[test]
    fn stride_keeps_short_ranges_at_full_fidelity() {
        assert_eq!(stride_for(0, 300), 1);
        assert_eq!(stride_for(288, 300), 1);
        assert_eq!(stride_for(300, 300), 1);
    }

    #[test]
    fn stride_spreads_long_ranges_evenly() {
        assert_eq!(stride_for(301, 300), 2);
        assert_eq!(stride_for(2_016, 300), 7);
        assert_eq!(stride_for(25_920, 300), 87);
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
                &1.0,
                &300.0,
            ],
        );
        assert_eq!(ats.len(), 300);
        assert_eq!(ats[0], 1_800_000_000);
    }

    #[test]
    fn ranged_series_spreads_evenly_at_stride_seven() {
        let conn = seed_db();
        let ats = col0(&conn, series_ranged_sql(), &[&"m3", &0.0, &7.0, &300.0]);
        // 2,016 rows at stride 7 → ranks 1, 8, …, 2011 → 288 points.
        assert_eq!(ats.len(), 288);
        assert_eq!(ats[0], 1_800_000_000);
        let steps: std::collections::BTreeSet<i64> = ats.windows(2).map(|w| w[0] - w[1]).collect();
        assert_eq!(steps.into_iter().collect::<Vec<_>>(), vec![2_100]);
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
