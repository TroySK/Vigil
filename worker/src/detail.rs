//! Per-monitor detail: status, range bars, latency chart, incidents.
//!
//! Every section covers the selected [`Range`] (24 hours plus 7/30/90 days,
//! defaulting to 90): history bars switch hourly/daily granularity, the
//! latency chart draws an honest spread of the window, the regional table
//! and incident list filter to it, and pills after the monitor name switch
//! ranges with a page reload.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::page::{default_counts, links_html, shell, time_html};
use crate::range::{bars_for, Range};
use crate::reads::{recent_gaps, region_latest, series_ranged, SampleOut};
use vigil_core::config::Site;
use vigil_core::rollup::{latency_chart, percentile_pair, sparkline_points};
use worker::{Response, Result};

/// Whether a downtime window touches `[cutoff, now]`: ongoing gaps always
/// do; closed ones overlap when they end at or after the cutoff and start
/// at or before now.
fn gap_in_range(at: i64, to: Option<i64>, cutoff: i64, now: i64) -> bool {
    at <= now && to.unwrap_or(now) >= cutoff
}

/// Latency chart points per range view: enough for a smooth line without
/// bloating the inline SVG on long ranges.
pub const DETAIL_LATENCY_POINTS: i64 = 300;
/// Incident rows shown on the detail page.
pub const DETAIL_INCIDENTS: usize = 50;

/// Per-monitor detail for one [`Range`]: status, range bars, latency
/// P50/P95 + sparkline, regional table, incident table, upkeep scoped to
/// this monitor. `favicons` is the parse-time origin map (`None` entries
/// fall back to a live parse).
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_precision_loss
)]
pub async fn render(
    db: &Db,
    site: &Site,
    favicons: &HashMap<String, String>,
    id: &str,
    range: Range,
) -> Result<Response> {
    let Some(monitor) = site.get(id) else {
        return Response::error("unknown monitor", 404);
    };
    let (at, counts, probed) = crate::reads::clock(db).await?;
    let counts = counts.unwrap_or_else(default_counts);
    let now = at.max(1);
    let cutoff = now.saturating_sub(range.secs());
    // D1 round trips dominate these reads, so fire them all at once
    // instead of awaiting them one by one. The global status comes from
    // the newest per-region sample, so no separate latest-sample query.
    // The gap fetch stays capped (GAP_SCAN_ROWS newest): an uncapped window
    // scan would let one flapping monitor blow up rows-read here. The
    // history cutoff is the tick-maintained `first:` stamp, not a MIN(at)
    // scan over the retention window.
    let first_key = format!("first:{id}");
    let (regions, gaps, first_raw, lat) = futures_util::try_join!(
        region_latest(db, id),
        recent_gaps(db, id, crate::api::GAP_SCAN_ROWS),
        db.get(&first_key),
        series_ranged(db, id, cutoff, DETAIL_LATENCY_POINTS),
    )?;
    let since = crate::reads::effective_since(first_raw.and_then(|s| s.parse().ok()), now);
    let current = regions.first();
    let is_up = current.map_or(true, |r| r.up == 1);
    // The 24-hour view renders 24 hourly cells; day ranges render one
    // cell per day (see range::bars_for). Both share the DayBar shape,
    // so one loop paints them.
    // `bars_for` only reads gap bounds, so the API rows paint directly —
    // no reason-string clones into a throwaway vector.
    let strip = bars_for(&gaps, now, range, since);
    let mut cells = String::new();
    let mut covered = 0_usize;
    let mut sum = 0.0_f64;
    let label = strip.label;
    for bar in &strip.cells {
        // Daily cells keep their percentage as (visually hidden) content,
        // matching the old 90-day markup; hourly cells stay empty. The
        // data-v percentage rides along in both cases for the tooltip.
        if bar.has_data {
            covered += 1;
            sum += bar.pct;
            let v = format!("{:.1}%", bar.pct);
            let content = if range.hourly() { "" } else { &v };
            cells.push_str(&crate::range::bar_cell(
                bar,
                strip.kind,
                &label,
                content,
                Some(&v),
            ));
        } else {
            cells.push_str(&crate::range::bar_cell(bar, strip.kind, &label, "", None));
        }
    }
    let bars_label = if covered == 0 {
        format!("Last {}: no data yet", range.label)
    } else {
        format!(
            "Last {}: {:.1}% average uptime over {} {} with data.",
            range.label,
            sum / covered as f64,
            covered,
            strip.period
        )
    };
    // One reverse pass builds chart points; latencies derive from them
    // (was: two reverse passes plus a third copy inside the sort).
    let points: Vec<(i64, i64, &str)> = lat
        .iter()
        .rev()
        .map(|r| (r.at, r.ms, r.place.as_str()))
        .collect();
    let vals: Vec<i64> = points.iter().map(|&(_, ms, _)| ms).collect();
    // One sort for both percentiles (was: one sort each).
    let (p50, p95) = percentile_pair(&vals, 50.0, 95.0);
    let spark = latency_chart(&points, 600, 150, range.hourly());
    let spark_html = if spark.is_empty() {
        "<p class=empty>No latency samples yet — the chart fills in after the next probe.</p>"
            .to_owned()
    } else {
        format!(
            "<span role=img aria-label=\"Latency over {} samples: P50 {}ms, P95 {}ms.\">{spark}</span>",
            vals.len(),
            p50,
            p95
        )
    };
    let mut rows = String::new();
    for gap in gaps
        .iter()
        .filter(|g| gap_in_range(g.at, g.to, cutoff, now))
        .take(DETAIL_INCIDENTS)
    {
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
    let geo_table = geo_latency_table(&lat, now, range.days, range.label);
    let mut notes = String::new();
    for upkeep in &site.upkeep {
        if !upkeep.only.is_empty() && !upkeep.only.iter().any(|only| only == id) {
            continue;
        }
        if let Some((kind, text)) = vigil_core::upkeep::upkeep_banner(upkeep, at) {
            let _ = write!(notes, "<div class=\"note {kind}\">{}</div>", esc(&text));
        }
    }
    let live;
    let origin = match favicons.get(id) {
        Some(origin) => Some(origin.as_str()),
        None => {
            live = crate::page::favicon_origin(monitor);
            live.as_deref()
        }
    };
    let body = format!(
        "<nav class=crumb aria-label=Breadcrumb><a href=\"/?range={range_param}\">&lsaquo; all monitors</a></nav>\
        <section class=detail-hero aria-labelledby=detail-name><h2 id=detail-name>{logo}{}</h2>\
        <p class=status-line role=status><span class=status-word data-up=\"{flag}\">{word}</span> \
        <span class=tnum>{ms}</span></p>{notes}</section>\
        <h3 class=sect>Last {range_label}</h3><div class={bars_class} role=img aria-label=\"{bars_label}\" tabindex=0>{cells}</div>\
        <h3 class=sect>Latency</h3><p class=tnum>P50 {p50}ms &middot; P95 {p95}ms &middot; {n} samples</p>{spark} \
        <h3 class=sect>Latency by region &middot; last {range_label}</h3><p class=tnum>Per-region latency over the last {range_label}, grouped by probe origin.</p>{geo_table} \
        <h3 class=sect>Incidents &middot; last {range_label}</h3><table class=inc><caption class=sr-only>Downtime windows for {name} over the last {range_label}</caption><thead><tr><th scope=col>From</th><th scope=col>To</th><th scope=col>Reason</th></tr></thead><tbody>{rows}</tbody></table>",
        esc(&monitor.name),
        logo = crate::page::favicon_img(origin, 24),
        range_param = range.param,
        word = if is_up { "UP" } else { "DOWN" },
        flag = if is_up { "1" } else { "0" },
        ms = last_probe_label(current),
        p50 = p50,
        p95 = p95,
        n = vals.len(),
        spark = spark_html,
        range_label = range.label,
        bars_class = strip.class,
        geo_table = geo_table,
        name = esc(&monitor.name),
    );

    let links = links_html(&site.links);
    // The id escapes once here, not once per range option below.
    let id = esc(&monitor.id);
    let menu = crate::range::menu(range, |option| {
        format!("/monitor?id={id}&range={}", option.param)
    });
    let home = "/";
    let speculation = crate::page::speculation_rule("/");
    Response::from_html(shell(
        &monitor.name,
        &site.title,
        &links,
        &counts,
        at,
        site.refresh_ms(),
        &menu,
        &home,
        &speculation,
        probed,
        &body,
    ))
}

/// Sentence-case relative age for standalone display ("just now" →
/// "Just now"; digit-led strings pass through). Mid-sentence uses stay
/// lowercase via the `rel` kind; standalone cells use the `Rel` variant,
/// capitalized server- and client-side alike.
fn standalone_rel(now: i64, at: i64) -> String {
    let age = vigil_core::rel_label(now, at);
    let mut chars = age.chars();
    match chars.next() {
        // rel_label output is ASCII, so the first byte is one char.
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => age,
    }
}

/// Per-region latency table: one row per probe origin (`place`) with its
/// latest sample and a sparkline of its samples inside the selected window
/// (`lat` is newest-first and already spans the window).
///
/// Rows sort by most recently seen region first. Regions with no samples
/// inside the window keep their row with a placeholder instead of a chart.
#[must_use]
pub fn geo_latency_table(
    lat: &[SampleOut],
    at: i64,
    window_days: i64,
    window_label: &str,
) -> String {
    if lat.is_empty() {
        return "<p class=empty>No regional samples yet — the table fills in after the next probe.</p>"
            .to_owned();
    }
    let cutoff = at.max(1).saturating_sub(window_days.saturating_mul(86_400));
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
            format!("<span class=empty>No samples in last {window_label}</span>")
        } else {
            sparkline_points(&window, 160, 36)
        };
        let _ = write!(
            rows,
            "<tr><td data-label=\"Region\">{}</td>\
            <td data-label=\"Last checked\">{}</td>\
            <td data-label=\"Latency\">{ms}ms</td>\
            <td data-label=\"Last {window_label}\">{chart}</td></tr>",
            esc(place),
            time_html(recent.at, "Rel", &standalone_rel(at, recent.at)),
            ms = recent.ms,
        );
    }
    format!(
        "<table class=\"inc geo\"><caption class=sr-only>Latency by probe region over the last {window_label}</caption>\
        <thead><tr><th scope=col>Region</th><th scope=col>Last checked</th><th scope=col>Latency</th><th scope=col>Last {window_label}</th></tr></thead>\
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
    use super::{gap_in_range, geo_latency_table, last_probe_label, standalone_rel};
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
    fn gap_overlap_keeps_open_and_straddling_windows() {
        // Ongoing gaps always overlap the live edge.
        assert!(gap_in_range(100, None, 500, 1_000));
        // Fully inside, straddling the start, and touching the cutoff.
        assert!(gap_in_range(600, Some(700), 500, 1_000));
        assert!(gap_in_range(100, Some(600), 500, 1_000));
        assert!(gap_in_range(100, Some(500), 500, 1_000));
        // Entirely before the window or after now.
        assert!(!gap_in_range(100, Some(499), 500, 1_000));
        assert!(!gap_in_range(1_100, Some(1_200), 500, 1_000));
    }

    #[test]
    fn geo_table_groups_by_region_with_latest_first() {
        // Newest-first input; SIN's newest sample is older than BOM's.
        let lat = vec![
            placed(2_000, 120, "BOM"),
            placed(1_900, 300, "SIN"),
            placed(1_800, 110, "BOM"),
        ];
        let html = geo_latency_table(&lat, 2_000, 7, "7 days");
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
        // Standalone ages render sentence-cased via the Rel variant.
        assert!(html.contains("data-tk=\"Rel\""));
    }

    #[test]
    fn geo_table_keeps_only_window_samples_in_charts() {
        let at = 10 * 86_400;
        let cutoff = at - 7 * 86_400;
        let lat = vec![
            placed(at, 100, "BOM"),
            placed(cutoff - 1, 999, "OLD"),
        ];
        let html = geo_latency_table(&lat, at, 7, "7 days");
        // OLD's only sample falls outside the window: row stays, chart does not.
        assert!(html.contains("OLD"));
        assert!(html.contains("No samples in last 7 days"));
    }

    #[test]
    fn geo_table_empty_without_samples() {
        assert!(geo_latency_table(&[], 1_000, 7, "7 days").contains("No regional samples yet"));
    }

    #[test]
    fn geo_table_labels_follow_the_window() {
        let lat = vec![placed(2_000, 120, "BOM")];
        let html = geo_latency_table(&lat, 2_000, 90, "90 days");
        assert!(html.contains("Last 90 days"));
        assert!(!html.contains("7 days"));
    }

    #[test]
    fn standalone_rel_capitalizes_sentence_start() {
        assert_eq!(standalone_rel(1_000, 970), "Just now");
        assert_eq!(standalone_rel(1_000, 800), "3 minutes ago");
    }
}
