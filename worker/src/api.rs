//! JSON APIs: health snapshot, SVG badge, history, incidents, latency.

use crate::db::Db;
use crate::reads::{
    clock, latest, latest_by_monitor, recent_gaps, series, uptime_by_monitor, SampleOut, Uptime,
};
use vigil_core::config::{Monitor, Site};
use vigil_core::rollup::percentile_triple;
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
    // Two batched readers cover every monitor (each chunks internally; was:
    // two per monitor).
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let base = at.max(1);
    // Month uptime needs 30 days of gaps, but the SQL aggregate clamps each
    // gap to the day/week/month windows and returns three numbers per
    // monitor — no gap rows cross the wire, and a flapping monitor cannot
    // inflate the response or the Worker's memory.
    let (latest, uptimes) = futures_util::try_join!(
        latest_by_monitor(db, &ids),
        uptime_by_monitor(db, &ids, base, [DAY_SECS, WEEK_SECS, MONTH_SECS]),
    )?;
    let mut items = serde_json::Map::new();
    for monitor in &site.monitors {
        items.insert(
            monitor.id.clone(),
            monitor_status(latest.get(&monitor.id), uptimes.get(&monitor.id), base),
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
/// Status comes from the SQL gap summary (an open gap = down), NOT from the
/// latest `samples` row: split cadence writes samples hourly but gaps every
/// 5 minutes, so the sample's `up` flag can lag reality by ~1h. Latency
/// (`ms`/`at`) still comes from the latest sample — hourly-stale by
/// design. Missing samples read as up with zeroed readings, matching the
/// old per-monitor path.
fn monitor_status(
    current: Option<&SampleOut>,
    uptime: Option<&Uptime>,
    base: i64,
) -> serde_json::Value {
    // Open gap = down right now, even when the last hourly sample says up.
    let (is_up, day, week, month) = uptime.map_or((true, 100.0, 100.0, 100.0), |u| {
        (!u.open, u.day, u.week, u.month)
    });
    let is_up = is_up && current.map_or(true, |r| r.up == 1);
    serde_json::json!({
        "up": is_up,
        "ms": current.map_or(0, |r| r.ms),
        "at": current.map_or(base, |r| r.at),
        "place": current.map_or("", |r| r.place.as_str()),
        "hasData": current.is_some(),
        "uptime": {
            "day": day,
            "week": week,
            "month": month,
        },
    })
}

/// Shields-style SVG badge for one monitor (`?id=`) or the whole site.
pub async fn badge(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let id = query_param(url, "id");
    let (label, is_up) = match id.as_deref() {
        None => {
            let counts = db.get("counts").await?.unwrap_or_else(default_counts);
            ("status".to_owned(), parse_counts(&counts).1 == 0)
        }
        Some(id) => {
            let Some(monitor) = site.get(id) else {
                // A renamed or removed monitor must not leave a broken image
                // in someone else's README. Render a neutral badge and let
                // the normal cache policy retire it.
                return badge_svg(id, "unknown", UNKNOWN_BADGE_COLOR);
            };
            // Status from gaps (open gap = down), not the hourly sample: split
            // cadence probes uptime every 5m but samples hourly. One extra
            // capped read only on badge hits, not on every page load.
            let (current, gaps) =
                futures_util::try_join!(latest(db, &monitor.id), recent_gaps(db, &monitor.id, 5),)?;
            let open = gaps.iter().any(|g| g.to.is_none());
            (
                monitor.name.clone(),
                !open && current.as_ref().map_or(true, |r| r.up == 1),
            )
        }
    };
    let (word, color) = vigil_core::views::badge(is_up);
    badge_svg(&label, word, color)
}

/// Neutral color for a badge whose monitor no longer exists.
const UNKNOWN_BADGE_COLOR: &str = "#6e7781";

/// Build the SVG response shared by live and unknown-id badges.
fn badge_svg(label: &str, word: &str, color: &str) -> Result<Response> {
    let svg = badge_markup(label, word, color);
    // Cache policy comes from the shared `cache_control` in `http.rs`
    // (5min public): badge embeds poll constantly, and every uncached hit
    // costs D1 reads.
    Ok(Response::builder()
        .with_header("content-type", "image/svg+xml")?
        .body(ResponseBody::Body(svg.into_bytes())))
}

/// SVG text behind [`badge_svg`], split out so tests can assert markup.
fn badge_markup(label: &str, word: &str, color: &str) -> String {
    vigil_core::views::svg(label, word, color)
}

/// Raw latency samples for charts (`?id=`, up to [`MAX_ROWS`]).
pub async fn history(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let Some(monitor) = requested_monitor(site, url) else {
        return Response::error("unknown id", 404);
    };
    Response::from_json(&series(db, &monitor.id, MAX_ROWS).await?)
}

/// Recent downtime windows (`?id=`, `?limit=` clamped to [`MAX_ROWS`]).
pub async fn incidents(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let Some(monitor) = requested_monitor(site, url) else {
        return Response::error("unknown id", 404);
    };
    let limit: i64 = query_param(url, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROWS)
        .clamp(1, MAX_ROWS);
    Response::from_json(&recent_gaps(db, &monitor.id, limit).await?)
}

/// Latency percentiles plus raw samples (`?id=`).
pub async fn latency(db: &Db, site: &Site, url: &url::Url) -> Result<Response> {
    let Some(monitor) = requested_monitor(site, url) else {
        return Response::error("unknown id", 404);
    };
    let rows = series(db, &monitor.id, MAX_ROWS).await?;
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

/// `true` when every query pair uses an allowed key exactly once. Unknown
/// or duplicated keys would otherwise produce cache-busting URLs and
/// ambiguous reads.
pub(crate) fn query_ok(url: &url::Url, allowed: &[&str]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    url.query_pairs()
        .all(|(key, _)| allowed.contains(&key.as_ref()) && seen.insert(key.into_owned()))
}

/// The configured monitor named by `?id=`, or `None` when missing/unknown
/// (including ids removed from config whose history is still retained).
fn requested_monitor<'a>(site: &'a Site, url: &url::Url) -> Option<&'a Monitor> {
    query_param(url, "id")
        .as_deref()
        .and_then(|id| site.get(id))
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
    use super::{
        badge_markup, default_counts, monitor_status, parse_counts, query_ok, query_param,
        requested_monitor, UNKNOWN_BADGE_COLOR,
    };
    use crate::reads::{SampleOut, Uptime};
    use vigil_core::config::Site;

    fn site() -> Site {
        serde_yaml::from_str(
            "monitors:\n  - id: a\n    name: A\n    check:\n      via: tcp\n      host: example.com\n      port: 80\n",
        )
        .expect("fixture parses")
    }

    #[test]
    fn query_validation_rejects_unknown_and_duplicate_params() {
        let ok = url::Url::parse("https://status.example/api/history?id=a&limit=5").unwrap();
        assert!(query_ok(&ok, &["id", "limit"]));
        assert!(!query_ok(&ok, &["id"]), "limit is unknown here");
        let dup = url::Url::parse("https://status.example/api/history?id=a&id=b").unwrap();
        assert!(!query_ok(&dup, &["id"]), "duplicate params are ambiguous");
        let bare = url::Url::parse("https://status.example/api/health").unwrap();
        assert!(query_ok(&bare, &[]));
        let bust = url::Url::parse("https://status.example/api/health?x=1").unwrap();
        assert!(!query_ok(&bust, &[]), "cache-busting params are rejected");
    }

    #[test]
    fn requested_monitor_requires_a_configured_id() {
        let site = site();
        let known = url::Url::parse("https://status.example/api/history?id=a").unwrap();
        assert!(requested_monitor(&site, &known).is_some());
        let unknown = url::Url::parse("https://status.example/api/history?id=gone").unwrap();
        assert!(requested_monitor(&site, &unknown).is_none());
        let missing = url::Url::parse("https://status.example/api/history").unwrap();
        assert!(requested_monitor(&site, &missing).is_none());
    }

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
    fn unknown_badge_renders_a_neutral_pill() {
        let svg = badge_markup("retired", "unknown", UNKNOWN_BADGE_COLOR);
        assert!(svg.starts_with("<svg "));
        assert!(svg.contains(">retired<"));
        assert!(svg.contains(">unknown<"));
        assert!(!svg.contains("404"));
    }

    fn clean() -> Uptime {
        Uptime {
            open: false,
            day: 100.0,
            week: 100.0,
            month: 100.0,
        }
    }

    #[test]
    fn status_shapes_up_monitor() {
        let v = monitor_status(Some(&tried(1_000, 54, 1)), Some(&clean()), 2_000);
        assert_eq!(v["up"].as_bool(), Some(true));
        assert_eq!(v["ms"].as_i64(), Some(54));
        assert_eq!(v["at"].as_i64(), Some(1_000));
        assert_eq!(v["place"].as_str(), Some("BOM"));
        assert_eq!(v["hasData"].as_bool(), Some(true));
        assert_eq!(v["uptime"]["day"].as_f64(), Some(100.0));
    }

    #[test]
    fn status_falls_back_without_samples() {
        let v = monitor_status(None, None, 2_000);
        assert_eq!(v["up"].as_bool(), Some(true));
        assert_eq!(v["ms"].as_i64(), Some(0));
        assert_eq!(v["at"].as_i64(), Some(2_000));
        assert_eq!(v["hasData"].as_bool(), Some(false));
    }

    #[test]
    fn status_marks_down_monitor() {
        let v = monitor_status(
            Some(&tried(1_500, 0, 0)),
            Some(&Uptime {
                open: false,
                day: 99.4,
                week: 99.9,
                month: 100.0,
            }),
            2_000,
        );
        assert_eq!(v["up"].as_bool(), Some(false));
        assert!(v["uptime"]["day"].as_f64().unwrap() < 100.0);
    }

    #[test]
    fn status_reads_open_gap_despite_stale_up_sample() {
        // Split cadence: samples land hourly, gaps every 5m. A monitor
        // that went down after its last hourly sample must read down,
        // not up, until the next sample lands.
        let v = monitor_status(
            Some(&tried(1_000, 54, 1)),
            Some(&Uptime {
                open: true,
                ..clean()
            }),
            2_000,
        );
        assert_eq!(v["up"].as_bool(), Some(false));
        // Latency still comes from the hourly sample (stale by design).
        assert_eq!(v["ms"].as_i64(), Some(54));
        assert_eq!(v["at"].as_i64(), Some(1_000));
    }
}
