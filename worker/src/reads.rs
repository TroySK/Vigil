//! Typed D1 read models: gap rows and latency samples.

use serde::{Deserialize, Serialize};
use worker::Result;

use crate::db::{num, Db};

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
