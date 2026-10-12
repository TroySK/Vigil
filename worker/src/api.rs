//! JSON APIs: health snapshot, SVG badge, history, incidents, latency.

use std::collections::BTreeMap;

use crate::db::Db;
use crate::reads::{
    latest, latest_by_monitor, recent_gaps, recent_gaps_by_monitor, series, SampleOut,
};
use vigil_core::config::Site;
use vigil_core::rollup::percentile;
use vigil_core::state::{Gap, Track};
use worker::{Response, ResponseBody, Result};

/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const DAY_SECS: i64 = 86_400;
/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const WEEK_SECS: i64 = 7 * DAY_SECS;
/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const MONTH_SECS: i64 = 30 * DAY_SECS;
/// Max rows served by list endpoints.
pub const MAX_ROWS: i64 = 200;
/// Default rows when `?limit=` is missing or unparseable.
pub const DEFAULT_ROWS: i64 = 50;
/// Rows scanned for history-bar rollups.
pub const GAP_SCAN_ROWS: i64 = 500;

/// Aggregate snapshot: counts plus per-monitor status and uptime windows.
pub async fn health(db: &Db, site: &Site) -> Result<Response> {
    let at = last_tick_at(db).await?;
    let (up, down) = parse_counts(&db.get("counts").await?.unwrap_or_else(default_counts));
    // Two batched round trips cover every monitor (was: two per monitor).
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let (latest, gap_lists) = futures_util::try_join!(
        latest_by_monitor(db, &ids),
        recent_gaps_by_monitor(db, &ids, GAP_SCAN_ROWS),
    )?;
    let mut items = serde_json::Map::new();
    for monitor in &site.monitors {
        items.insert(
            monitor.id.clone(),
            monitor_status(
                latest.get(&monitor.id),
                gap_lists.get(&monitor.id).map_or(&[], Vec::as_slice),
                at.max(1),
            ),
        );
    }
    Response::from_json(&serde_json::json!({
        "title": site.title, "up": up, "down": down, "at": at, "monitors": items,
    }))
}

/// One monitor's health entry: status, latency, and day/week/month uptime.
/// Pure shaping over already-fetched rows, so the snapshot assembly is
/// unit-testable without D1. Missing samples read as up with zeroed
/// readings, matching the old per-monitor path.
fn monitor_status(current: Option<&SampleOut>, gaps: &[Gap], base: i64) -> serde_json::Value {
    let is_up = current.map_or(true, |r| r.up == 1);
    let track = Track {
        gaps: gaps.to_vec(),
    };
    serde_json::json!({
        "up": is_up,
        "ms": current.map_or(0, |r| r.ms),
        "at": current.map_or(base, |r| r.at),
        "uptime": {
            "day": track.uptime_pct(base, DAY_SECS),
            "week": track.uptime_pct(base, WEEK_SECS),
            "month": track.uptime_pct(base, MONTH_SECS),
        },
    })
}

/// Shields-style SVG badge for one monitor (`?id=`) or the whole site.
pub async fn badge(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id");
    let (label, is_up) = if let Some(monitor) = id.as_deref().and_then(|i| site.get(i)) {
        let current = latest(db, &monitor.id).await?;
        (
            monitor.name.clone(),
            current.as_ref().map_or(true, |r| r.up == 1),
        )
    } else {
        let counts = db.get("counts").await?.unwrap_or_else(default_counts);
        ("status".to_owned(), parse_counts(&counts).1 == 0)
    };
    let (word, color) = vigil_core::views::badge(is_up);
    let svg = vigil_core::views::svg(&label, word, color);
    Ok(Response::builder()
        .with_header("content-type", "image/svg+xml")?
        .with_header("cache-control", "no-store")?
        .body(ResponseBody::Body(svg.into_bytes())))
}

/// Raw latency samples for charts (`?id=`, up to [`MAX_ROWS`]).
pub async fn history(db: &Db, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id").unwrap_or_default();
    Response::from_json(&series(db, &id, MAX_ROWS).await?)
}

/// Recent downtime windows (`?id=`, `?limit=` clamped to [`MAX_ROWS`]).
pub async fn incidents(db: &Db, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id").unwrap_or_default();
    let limit: i64 = query_param(url, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_ROWS);
    Response::from_json(&recent_gaps(db, &id, limit).await?)
}

/// Latency percentiles plus raw samples (`?id=`).
pub async fn latency(db: &Db, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id").unwrap_or_default();
    let rows = series(db, &id, MAX_ROWS).await?;
    let vals: Vec<i64> = rows.iter().map(|r| r.ms).collect();
    Response::from_json(&serde_json::json!({
        "p50": percentile(&vals, 50.0),
        "p95": percentile(&vals, 95.0),
        "p99": percentile(&vals, 99.0),
        "samples": rows,
    }))
}

/// Last cron tick time, `0` when the worker never ticked.
async fn last_tick_at(db: &Db) -> Result<i64> {
    Ok(db
        .get("at")
        .await?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// Default `up/down` counts string before the first tick.
fn default_counts() -> String {
    "0/0".to_owned()
}

/// Parse an `up/down` counts string; malformed input yields `(0, 0)`.
fn parse_counts(counts: &str) -> (u32, u32) {
    counts.split_once('/').map_or((0, 0), |(up, down)| {
        (up.parse().unwrap_or(0), down.parse().unwrap_or(0))
    })
}

/// Extract one query parameter value (uses a map so repeated scans are cheap
/// when several params are read from the same URL).
#[allow(clippy::needless_pass_by_value)]
fn query_param(url: &url::Url, name: &str) -> Option<String> {
    let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    params.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::{default_counts, monitor_status, parse_counts, query_param};
    use crate::reads::SampleOut;
    use vigil_core::state::Gap;

    #[test]
    fn health_counts_fall_back_to_zero() {
        assert_eq!(parse_counts("28/2"), (28, 2));
        assert_eq!(parse_counts(&default_counts()), (0, 0));
        assert_eq!(parse_counts("bogus"), (0, 0));
    }

    #[test]
    fn query_params_extract_by_name() {
        let url = url::Url::parse("https://status.example/api/history?id=a&limit=5").unwrap();
        assert_eq!(query_param(&url, "id").as_deref(), Some("a"));
        assert_eq!(query_param(&url, "limit").as_deref(), Some("5"));
        assert_eq!(query_param(&url, "missing"), None);
    }

    fn tried(at: i64, ms: i64, up: i64) -> SampleOut {
        SampleOut {
            at,
            ms,
            up,
            place: "BOM".into(),
        }
    }

    #[test]
    fn status_shapes_up_monitor() {
        let v = monitor_status(Some(&tried(1_000, 54, 1)), &[], 2_000);
        assert_eq!(v["up"].as_bool(), Some(true));
        assert_eq!(v["ms"].as_i64(), Some(54));
        assert_eq!(v["at"].as_i64(), Some(1_000));
        assert_eq!(v["uptime"]["day"].as_f64(), Some(100.0));
    }

    #[test]
    fn status_falls_back_without_samples() {
        let v = monitor_status(None, &[], 2_000);
        assert_eq!(v["up"].as_bool(), Some(true));
        assert_eq!(v["ms"].as_i64(), Some(0));
        assert_eq!(v["at"].as_i64(), Some(2_000));
    }

    #[test]
    fn status_marks_down_monitor() {
        let gap = Gap {
            at: 1_000,
            to: Some(1_500),
            why: "refused".into(),
        };
        let v = monitor_status(Some(&tried(1_500, 0, 0)), &[gap], 2_000);
        assert_eq!(v["up"].as_bool(), Some(false));
        assert!(v["uptime"]["day"].as_f64().unwrap() < 100.0);
    }
}
