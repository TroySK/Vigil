//! JSON APIs: health snapshot, SVG badge, history, incidents, latency.

use crate::db::Db;
use crate::reads::{
    clock, latest, latest_by_monitor, recent_gaps, recent_gaps_by_monitor, series, SampleOut,
};
use vigil_core::config::Site;
use vigil_core::rollup::percentile_triple;
use vigil_core::state::{Gap, Track};
use worker::{Response, ResponseBody, Result};

/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const DAY_SECS: i64 = 86_400;
/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const WEEK_SECS: i64 = 7 * DAY_SECS;
/// Seconds in one day / week / month windows for `/api/health` uptime.
pub const MONTH_SECS: i64 = 30 * DAY_SECS;
/// Max rows served by list endpoints. Hourly samples need far fewer rows
/// than 5-minute probes (24/day vs 288/day): 100 rows still cover 4 days
/// of hourly data while capping D1 rows scanned per request.
pub const MAX_ROWS: i64 = 100;
/// Default rows when `?limit=` is missing or unparseable.
pub const DEFAULT_ROWS: i64 = 50;
/// Rows scanned for history-bar rollups.
pub const GAP_SCAN_ROWS: i64 = 500;

/// Aggregate snapshot: counts plus per-monitor status and uptime windows.
pub async fn health(db: &Db, site: &Site) -> Result<Response> {
    // One batched `kv` read for the tick clock, counts, and probe clock
    // (was: three sequential round trips). The refresh poller reloads on
    // the probe clock, not the tick clock, so skip-only ticks cost a
    // cheap poll instead of a full page render.
    let (at, counts, probed) = clock(db).await?;
    let (up, down) = parse_counts(&counts.unwrap_or_else(default_counts));
    // Two batched round trips cover every monitor (was: two per monitor).
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let base = at.max(1);
    // Month uptime needs 30 days of gaps; older closures cannot affect it.
    // The ranked per-monitor query caps rows at GAP_SCAN_ROWS inside the
    // window, so a flapping monitor can't blow up rows-read on this
    // highest-fan-out endpoint.
    let (latest, gap_lists) = futures_util::try_join!(
        latest_by_monitor(db, &ids),
        recent_gaps_by_monitor(db, &ids, GAP_SCAN_ROWS, base.saturating_sub(MONTH_SECS)),
    )?;
    let mut items = serde_json::Map::new();
    for monitor in &site.monitors {
        items.insert(
            monitor.id.clone(),
            monitor_status(
                latest.get(&monitor.id),
                gap_lists.get(&monitor.id).map_or(&[], Vec::as_slice),
                base,
            ),
        );
    }
    Response::from_json(&serde_json::json!({
        "title": site.title, "up": up, "down": down, "at": at, "dataAt": probed, "monitors": items,
    }))
}

/// One monitor's health entry: status, latency, and day/week/month uptime.
/// Pure shaping over already-fetched rows, so the snapshot assembly is
/// unit-testable without D1.
///
/// Status comes from gaps (an open gap = down), NOT from the latest
/// `samples` row: split cadence writes samples hourly but gaps every 5
/// minutes, so the sample's `up` flag can lag reality by ~1h. Latency
/// (`ms`/`at`) still comes from the latest sample — hourly-stale by
/// design. Missing samples read as up with zeroed readings, matching the
/// old per-monitor path.
fn monitor_status(current: Option<&SampleOut>, gaps: &[Gap], base: i64) -> serde_json::Value {
    let track = Track {
        gaps: gaps.to_vec(),
    };
    // Open gap = down right now, even when the last hourly sample says up.
    let is_up = track.open().is_none() && current.map_or(true, |r| r.up == 1);
    // One pass over gaps for all three windows (was: three scans).
    let uptimes = track.uptime_many(base, &[DAY_SECS, WEEK_SECS, MONTH_SECS]);
    serde_json::json!({
        "up": is_up,
        "ms": current.map_or(0, |r| r.ms),
        "at": current.map_or(base, |r| r.at),
        "uptime": {
            "day": uptimes[0],
            "week": uptimes[1],
            "month": uptimes[2],
        },
    })
}

/// Shields-style SVG badge for one monitor (`?id=`) or the whole site.
pub async fn badge(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id");
    let (label, is_up) = if let Some(monitor) = id.as_deref().and_then(|i| site.get(i)) {
        // Status from gaps (open gap = down), not the hourly sample: split
        // cadence probes uptime every 5m but samples hourly. One extra
        // capped read only on badge hits, not on every page load.
        let (current, gaps) = futures_util::try_join!(
            latest(db, &monitor.id),
            recent_gaps(db, &monitor.id, 5),
        )?;
        let open = gaps.iter().any(|g| g.to.is_none());
        (
            monitor.name.clone(),
            !open && current.as_ref().map_or(true, |r| r.up == 1),
        )
    } else {
        let counts = db.get("counts").await?.unwrap_or_else(default_counts);
        ("status".to_owned(), parse_counts(&counts).1 == 0)
    };
    let (word, color) = vigil_core::views::badge(is_up);
    let svg = vigil_core::views::svg(&label, word, color);
    // Cache policy comes from the shared `cache_control` in `http.rs`
    // (5min public): badge embeds poll constantly, and every uncached hit
    // costs D1 reads.
    Ok(Response::builder()
        .with_header("content-type", "image/svg+xml")?
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
    // One sort for all three percentiles (was: three sorts).
    let (p50, p95, p99) = percentile_triple(&vals, 50.0, 95.0, 99.0);
    Response::from_json(&serde_json::json!({
        "p50": p50,
        "p95": p95,
        "p99": p99,
        "samples": rows,
    }))
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

/// Extract one query parameter value with a single scan over the pairs
/// (the old map-collecting version re-parsed and allocated the full query
/// string on every call — twice per API request).
#[allow(clippy::needless_pass_by_value)]
fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
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

    #[test]
    fn status_reads_open_gap_despite_stale_up_sample() {
        // Split cadence: samples land hourly, gaps every 5m. A monitor
        // that went down after its last hourly sample must read down,
        // not up, until the next sample lands.
        let open = Gap {
            at: 1_900,
            to: None,
            why: "refused".into(),
        };
        let v = monitor_status(Some(&tried(1_000, 54, 1)), &[open], 2_000);
        assert_eq!(v["up"].as_bool(), Some(false));
        // Latency still comes from the hourly sample (stale by design).
        assert_eq!(v["ms"].as_i64(), Some(54));
        assert_eq!(v["at"].as_i64(), Some(1_000));
    }
}
