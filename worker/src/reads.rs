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

/// Gaps overlapping the last-24-hours window per monitor, keyed by id.
/// Older closed gaps cannot affect hourly cells, so they never leave D1.
pub async fn gaps_24h_by_monitor(
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

/// Up to `limit` newest samples per monitor, keyed by id, newest first.
/// One window-function query replaces a round trip per monitor.
pub async fn series_by_monitor(
    db: &Db,
    ids: &[&str],
    limit: i64,
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
    let rows: Vec<Row> = db
        .q(&format!(
            "SELECT id, at, ms, up, place FROM (SELECT id, at, ms, up, place, ROW_NUMBER() OVER (PARTITION BY id ORDER BY at DESC) AS rn FROM samples WHERE id IN ({})) WHERE rn <= ?{} ORDER BY id ASC, at DESC",
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
    db.q("SELECT at, ms, up, place FROM (SELECT at, ms, up, place, ROW_NUMBER() OVER (ORDER BY at DESC) AS rn FROM samples WHERE id = ?1 AND at >= ?2) WHERE rn % ?3 = 1 ORDER BY at DESC LIMIT ?4")
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
    use super::stride_for;

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
}
