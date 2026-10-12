//! Per-monitor detail: status, 90-day bars, latency chart, incidents.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::page::{default_counts, last_tick_at, links_html, shell, time_html};
use crate::reads::{first_sample_at, latest, recent_gaps, series, GapOut, SampleOut};
use vigil_core::config::Site;
use vigil_core::rollup::{bar_color, latency_chart, month_bars, percentile, sparkline_points};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Days of history on the detail page.
pub const DETAIL_HISTORY_DAYS: usize = 90;
/// Latency samples on the detail page.
pub const DETAIL_LATENCY_SAMPLES: i64 = 200;
/// Days covered by the per-region latency table.
pub const GEO_WINDOW_DAYS: i64 = 7;
/// Incident rows shown on the detail page.
pub const DETAIL_INCIDENTS: usize = 50;

/// Per-monitor detail: status, 90-day bars, latency p50/p95 + sparkline,
/// incident table, upkeep scoped to this monitor.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub async fn render(db: &Db, site: &Site, id: &str) -> Result<Response> {
    let Some(monitor) = site.get(id) else {
        return Response::error("unknown monitor", 404);
    };
    let at = last_tick_at(db).await?;
    let counts = db.get("counts").await?.unwrap_or_else(default_counts);
    // One D1 round trip dominates these reads, so fire all four at once
    // instead of awaiting them one by one.
    let (current, gaps, since, lat): (
        Option<SampleOut>,
        Vec<GapOut>,
        Option<i64>,
        Vec<SampleOut>,
    ) = futures_util::try_join!(
        latest(db, id),
        recent_gaps(db, id, crate::api::GAP_SCAN_ROWS),
        first_sample_at(db, id),
        series(db, id, DETAIL_LATENCY_SAMPLES),
    )?;
    let is_up = current.as_ref().map_or(true, |r| r.up == 1);
    let core: Vec<Gap> = gaps
        .iter()
        .map(|g| Gap {
            at: g.at,
            to: g.to,
            why: g.why.clone(),
        })
        .collect();
    let mut cells = String::new();
    let mut covered = 0_usize;
    let mut sum = 0.0_f64;
    for bar in month_bars(&core, at.max(1), DETAIL_HISTORY_DAYS, since) {
        if bar.has_data {
            covered += 1;
            sum += bar.pct;
            let v = format!("{:.1}%", bar.pct);
            let _ = write!(
                cells,
                "<i aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"date\" data-v=\"{v}\" style=\"background:{}\">{:.1}%</i>",
                bar_color(bar.pct),
                bar.pct,
                day = bar.day,
                tip = format!(
                    "{}: {:.1}%",
                    vigil_core::date_label(bar.day),
                    bar.pct
                ),
            );
        } else {
            let _ = write!(
                cells,
                "<i class=nodata aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"date\"></i>",
                day = bar.day,
                tip = format!("{}: no data", vigil_core::date_label(bar.day)),
            );
        }
    }
    let bars_label = if covered == 0 {
        format!("Last {DETAIL_HISTORY_DAYS} days: no data yet")
    } else {
        format!(
            "Last {DETAIL_HISTORY_DAYS} days: {:.1}% average uptime over {} days with data.",
            sum / covered as f64,
            covered
        )
    };
    let vals: Vec<i64> = lat.iter().rev().map(|r| r.ms).collect();
    let points: Vec<(i64, i64, &str)> =
        lat.iter().rev().map(|r| (r.at, r.ms, r.place.as_str())).collect();
    let p50 = percentile(&vals, 50.0);
    let p95 = percentile(&vals, 95.0);
    let spark = latency_chart(&points, 600, 150);
    let spark_html = if spark.is_empty() {
        "<p class=empty>No latency samples yet — the chart fills in after the next probe.</p>"
            .to_owned()
    } else {
        format!(
            "<span role=img aria-label=\"Latency over {} samples: p50 {}ms, p95 {}ms.\">{spark}</span>",
            vals.len(),
            p50,
            p95
        )
    };
    let mut rows = String::new();
    for gap in gaps.iter().take(DETAIL_INCIDENTS) {
        let to = gap.to.map_or_else(
            || "ongoing".to_owned(),
            |t| time_html(t, "full", &vigil_core::time_label(t)),
        );
        let _ = write!(
            rows,
            "<tr><td data-label=\"From\">{}</td><td data-label=\"To\">{to}</td><td data-label=\"Reason\">{}</td></tr>",
            time_html(gap.at, "full", &vigil_core::time_label(gap.at)),
            esc(&gap.why)
        );
    }
    let geo_table = geo_latency_table(&lat, at);
    let mut notes = String::new();
    for upkeep in &site.upkeep {
        if !upkeep.only.is_empty() && !upkeep.only.iter().any(|only| only == id) {
            continue;
        }
        if let Some((kind, text)) = vigil_core::upkeep::upkeep_banner(upkeep, at) {
            let _ = write!(notes, "<div class=\"note {kind}\">{}</div>", esc(&text));
        }
    }
    let body = format!(
        "<nav class=crumb aria-label=Breadcrumb><a href=\"/\">&lsaquo; all monitors</a></nav>\
        <section class=detail-hero aria-labelledby=detail-name><h2 id=detail-name>{logo}{}</h2>\
        <p class=status-line role=status><span class=status-word data-up=\"{flag}\">{word}</span> \
        <span class=tnum>{ms}</span></p>{notes}</section>\
        <h3 class=sect>Last 90 days</h3><div class=bars90 role=img aria-label=\"{bars_label}\" tabindex=0>{cells}</div>\
        <h3 class=sect>Latency</h3><p class=tnum>p50 {p50}ms &middot; p95 {p95}ms &middot; {n} samples</p>{spark} \
        <h3 class=sect>Latency by region &middot; last {geo_days} days</h3><p class=tnum>Per-region latency over the last {geo_days} days, grouped by probe origin.</p>{geo_table} \
        <h3 class=sect>Incidents</h3><table class=inc><caption class=sr-only>Downtime windows for {name}</caption><thead><tr><th scope=col>From</th><th scope=col>To</th><th scope=col>Reason</th></tr></thead><tbody>{rows}</tbody></table>",
        esc(&monitor.name),
        logo = crate::page::favicon_img(monitor, 24),
        word = if is_up { "UP" } else { "DOWN" },
        flag = if is_up { "1" } else { "0" },
        ms = last_probe_label(current.as_ref()),
        p50 = p50,
        p95 = p95,
        n = vals.len(),
        spark = spark_html,
        geo_days = GEO_WINDOW_DAYS,
        geo_table = geo_table,
        name = esc(&monitor.name),
    );

    let links = links_html(&site.links);
    Response::from_html(shell(&monitor.name, &site.title, &links, &counts, at, site.refresh_ms(), &body))
}

/// Per-region latency table: one row per probe origin (`place`) with its
/// latest sample and a sparkline of its samples from the last
/// [`GEO_WINDOW_DAYS`] days (`lat` is newest-first).
///
/// Rows sort by most recently seen region first. Regions with no samples
/// inside the window keep their row with a placeholder instead of a chart.
#[must_use]
pub fn geo_latency_table(lat: &[SampleOut], at: i64) -> String {
    if lat.is_empty() {
        return "<p class=empty>No regional samples yet — the table fills in after the next probe.</p>"
            .to_owned();
    }
    let cutoff = at.max(1).saturating_sub(GEO_WINDOW_DAYS.saturating_mul(86_400));
    let mut by_place: BTreeMap<&str, Vec<&SampleOut>> = BTreeMap::new();
    for sample in lat {
        by_place
            .entry(sample.place.as_str())
            .or_default()
            .push(sample);
    }
    // Newest-first `lat` means each group's first entry is its latest.
    let mut groups: Vec<(&str, Vec<&SampleOut>)> = by_place.into_iter().collect();
    groups.sort_by_key(|(_, samples)| std::cmp::Reverse(samples[0].at));
    let mut rows = String::new();
    for (place, samples) in &groups {
        let recent = samples[0];
        let window: Vec<(i64, i64)> = samples
            .iter()
            .rev()
            .filter(|s| s.at >= cutoff)
            .map(|s| (s.at, s.ms))
            .collect();
        let chart = if window.is_empty() {
            "<span class=empty>No samples in last 7 days</span>".to_owned()
        } else {
            sparkline_points(&window, 160, 36)
        };
        let _ = write!(
            rows,
            "<tr><td data-label=\"Region\">{}</td>\
            <td data-label=\"Last checked\">{}</td>\
            <td data-label=\"Latency\">{ms}ms</td>\
            <td data-label=\"Last 7 days\">{chart}</td></tr>",
            esc(place),
            time_html(recent.at, "full", &vigil_core::time_label(recent.at)),
            ms = recent.ms,
        );
    }
    format!(
        "<table class=\"inc geo\"><caption class=sr-only>Latency by probe region over the last {GEO_WINDOW_DAYS} days</caption>\
        <thead><tr><th scope=col>Region</th><th scope=col>Last checked</th><th scope=col>Latency</th><th scope=col>Last 7 days</th></tr></thead>\
        <tbody>{rows}</tbody></table>"
    )
}

/// "418ms last probe · BOM" for the detail hero, or the empty state.
fn last_probe_label(current: Option<&SampleOut>) -> String {
    current.map_or_else(
        || "No probes yet".to_owned(),
        |sample| {
            format!(
                "{}ms last probe &middot; {}",
                sample.ms,
                esc(&sample.place)
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{geo_latency_table, last_probe_label, GEO_WINDOW_DAYS};
    use crate::reads::SampleOut;

    fn sample(up: i64) -> SampleOut {
        SampleOut {
            at: 1_000,
            ms: 418,
            up,
            place: "BOM".into(),
        }
    }

    fn placed(at: i64, ms: i64, place: &str) -> SampleOut {
        SampleOut {
            at,
            ms,
            up: 1,
            place: place.into(),
        }
    }

    #[test]
    fn label_carries_latency_and_origin() {
        assert_eq!(
            last_probe_label(Some(&sample(1))),
            "418ms last probe &middot; BOM"
        );
        assert_eq!(last_probe_label(None), "No probes yet");
    }

    #[test]
    fn geo_table_groups_by_region_with_latest_first() {
        // Newest-first input; SIN's newest sample is older than BOM's.
        let lat = vec![
            placed(2_000, 120, "BOM"),
            placed(1_900, 300, "SIN"),
            placed(1_800, 110, "BOM"),
        ];
        let html = geo_latency_table(&lat, 2_000);
        assert!(html.contains("BOM"));
        assert!(html.contains("SIN"));
        assert!(html.contains("120ms"));
        assert!(html.contains("300ms"));
        // Most recently seen region sorts first.
        assert!(html.find("BOM").unwrap() < html.find("SIN").unwrap());
        assert!(html.contains("Last 7 days"));
        // Timestamps ride along for the client-side UTC/local toggle.
        assert!(html.contains("data-ts=\"2000\""));
        assert!(html.contains("<time data-ts="));
    }

    #[test]
    fn geo_table_keeps_only_last_seven_days_in_charts() {
        let at = 10 * 86_400;
        let cutoff = at - GEO_WINDOW_DAYS * 86_400;
        let lat = vec![
            placed(at, 100, "BOM"),
            placed(cutoff - 1, 999, "OLD"),
        ];
        let html = geo_latency_table(&lat, at);
        // OLD's only sample falls outside the window: row stays, chart does not.
        assert!(html.contains("OLD"));
        assert!(html.contains("No samples in last 7 days"));
    }

    #[test]
    fn geo_table_empty_without_samples() {
        assert!(geo_latency_table(&[], 1_000).contains("No regional samples yet"));
    }
}
