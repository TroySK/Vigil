//! Typed D1 read models: gap rows and latency samples.

use serde::{Deserialize, Serialize};
use worker::Result;

use crate::db::Db;

/// One downtime window row for API responses and rollups.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GapOut {
    /// Start (unix seconds).
    pub at: i64,
    /// End (unix seconds), or `None` while ongoing.
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
}

/// Most recent gaps for a monitor, newest first.
pub async fn recent_gaps(db: &Db, id: &str, limit: i64) -> Result<Vec<GapOut>> {
    db.q("SELECT at, upto AS to, why FROM gaps WHERE id = ?1 ORDER BY at DESC LIMIT ?2")
        .bind(&[id.into(), limit.max(1).into()])?
        .all()
        .await?
        .results()
}

/// Latest sample for a monitor, if any.
pub async fn latest(db: &Db, id: &str) -> Result<Option<SampleOut>> {
    let rows: Vec<SampleOut> = db
        .q("SELECT at, ms, up FROM samples WHERE id = ?1 ORDER BY at DESC LIMIT 1")
        .bind(&[id.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().next())
}

/// Recent samples for a monitor, newest first.
pub async fn series(db: &Db, id: &str, limit: i64) -> Result<Vec<SampleOut>> {
    db.q("SELECT at, ms, up FROM samples WHERE id = ?1 ORDER BY at DESC LIMIT ?2")
        .bind(&[id.into(), limit.max(1).into()])?
        .all()
        .await?
        .results()
}
