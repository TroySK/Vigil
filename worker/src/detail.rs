//! Per-monitor detail: status, range bars, latency chart, incidents.
//!
//! Every section covers the selected [`Range`] (24 hours plus 7/30/90 days,
//! defaulting to 90): history bars switch hourly/daily granularity, the
//! latency chart draws an honest spread of the window, the regional table
//! and incident list filter to it, and pills after the monitor name switch
//! ranges with a page reload.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::page::{default_counts, last_tick_at, links_html, shell, time_html};
use crate::reads::{first_sample_at, recent_gaps, region_latest, series_ranged, GapOut, SampleOut};
use vigil_core::config::Site;
use vigil_core::rollup::{
    bar_color, hour_bars, latency_chart, month_bars, percentile, sparkline_points, DayBar,
};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Detail time range: everything on the page covers exactly this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// Calendar days covered (1 selects the 24-hour view).
    pub days: i64,
    /// Human span used in headings ("24 hours", "7 days", …).
    pub label: &'static str,
    /// `?range=` value selecting this range.
    pub param: &'static str,
    /// Short pill text.
    pub short: &'static str,
}

impl Range {
    /// Seconds covered.
    fn secs(self) -> i64 {
        self.days.saturating_mul(86_400)
    }

    /// The 24-hour view renders hourly cells; day ranges render daily cells.
    fn hourly(self) -> bool {
        self.days <= 1
    }
}

/// Range pills in display order.
pub const RANGES: [Range; 4] = [RANGE_24H, RANGE_7D, RANGE_30D, RANGE_90D];
pub const RANGE_24H: Range = Range {
    days: 1,
    label: "24 hours",
    param: "24h",
    short: "24H",
};
pub const RANGE_7D: Range = Range {
    days: 7,
    label: "7 days",
    param: "7d",
    short: "7D",
};
pub const RANGE_30D: Range = Range {
    days: 30,
    label: "30 days",
    param: "30d",
    short: "30D",
};
pub const RANGE_90D: Range = Range {
    days: 90,
    label: "90 days",
    param: "90d",
    short: "90D",
};
/// Default range: unknown or missing `?range=` values fall back here.
pub const DEFAULT_RANGE: Range = RANGE_90D;

/// Parse `?range=` (`24h`, `7d`, `30d`, `90d`, case-insensitive); anything
/// else — including a missing value — selects the 90-day default.
#[must_use]
pub fn parse_range(raw: Option<&str>) -> Range {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("24h") => RANGE_24H,
        Some("7d") => RANGE_7D,
        Some("30d") => RANGE_30D,
        _ => DEFAULT_RANGE,
    }
}

/// One history strip: cells plus how to paint, label, and describe them.
struct Bars {
    cells: Vec<DayBar>,
    class: &'static str,
    kind: &'static str,
    label: fn(i64) -> String,
    period: &'static str,
}

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
/// p50/p95 + sparkline, regional table, incident table, upkeep scoped to
/// this monitor.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub async fn render(db: &Db, site: &Site, id: &str, range: Range) -> Result<Response> {
    let Some(monitor) = site.get(id) else {
        return Response::error("unknown monitor", 404);
    };
    let at = last_tick_at(db).await?;
    let now = at.max(1);
    let cutoff = now.saturating_sub(range.secs());
    let counts = db.get("counts").await?.unwrap_or_else(default_counts);
    // D1 round trips dominate these reads, so fire them all at once
    // instead of awaiting them one by one. The global status comes from
    // the newest per-region sample, so no separate latest-sample query.
    let (regions, gaps, since, lat): (Vec<SampleOut>, Vec<GapOut>, Option<i64>, Vec<SampleOut>) = futures_util::try_join!(
        region_latest(db, id),
        recent_gaps(db, id, crate::api::GAP_SCAN_ROWS),
        first_sample_at(db, id),
        series_ranged(db, id, cutoff, DETAIL_LATENCY_POINTS),
    )?;
    let current = regions.first();
    let is_up = current.map_or(true, |r| r.up == 1);
    let core: Vec<Gap> = gaps
        .iter()
        .map(|g| Gap {
            at: g.at,
            to: g.to,
            why: g.why.clone(),
        })
        .collect();
    // The 24-hour view renders 24 hourly cells; day ranges render one
    // cell per day. Both share the DayBar shape, so one loop paints them.
    let strip = if range.hourly() {
        Bars {
            cells: hour_bars(&core, now, 24, since),
            class: "bars",
            kind: "short",
            label: vigil_core::short_label,
            period: "hours",
        }
    } else {
        Bars {
            cells: month_bars(&core, now, usize::try_from(range.days).unwrap_or(90), since),
            class: "bars90",
            kind: "date",
            label: vigil_core::date_label,
            period: "days",
        }
    };
    let mut cells = String::new();
    let mut covered = 0_usize;
    let mut sum = 0.0_f64;
    let label = strip.label;
    for bar in &strip.cells {
        // Daily cells keep their percentage as (visually hidden) content,
        // matching the old 90-day markup; hourly cells stay empty.
        let content = if range.hourly() {
            String::new()
        } else {
            format!("{:.1}%", bar.pct)
        };
        if bar.has_data {
            covered += 1;
            sum += bar.pct;
            let v = format!("{:.1}%", bar.pct);
            let _ = write!(
                cells,
                "<i aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"{kind}\" data-v=\"{v}\" style=\"background:{}\">{content}</i>",
                bar_color(bar.pct),
                day = bar.day,
                kind = strip.kind,
                tip = format!("{}: {:.1}%", label(bar.day), bar.pct),
            );
        } else {
            let _ = write!(
                cells,
                "<i class=nodata aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"{kind}\"></i>",
                day = bar.day,
                kind = strip.kind,
                tip = format!("{}: no data", label(bar.day)),
            );
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
    let body = format!(
        "<nav class=crumb aria-label=Breadcrumb><a href=\"/\">&lsaquo; all monitors</a></nav>\
        <section class=detail-hero aria-labelledby=detail-name><h2 id=detail-name>{logo}{}</h2>{pills}\
        <p class=status-line role=status><span class=status-word data-up=\"{flag}\">{word}</span> \
        <span class=tnum>{ms}</span></p>{notes}</section>\
        <h3 class=sect>Last {range_label}</h3><div class={bars_class} role=img aria-label=\"{bars_label}\" tabindex=0>{cells}</div>\
        <h3 class=sect>Latency</h3><p class=tnum>p50 {p50}ms &middot; p95 {p95}ms &middot; {n} samples</p>{spark} \
        <h3 class=sect>Latency by region &middot; last {range_label}</h3><p class=tnum>Per-region latency over the last {range_label}, grouped by probe origin.</p>{geo_table} \
        <h3 class=sect>Incidents &middot; last {range_label}</h3><table class=inc><caption class=sr-only>Downtime windows for {name} over the last {range_label}</caption><thead><tr><th scope=col>From</th><th scope=col>To</th><th scope=col>Reason</th></tr></thead><tbody>{rows}</tbody></table>",
        esc(&monitor.name),
        logo = crate::page::favicon_img(monitor, 24),
        pills = range_pills(&monitor.id, range),
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
    Response::from_html(shell(&monitor.name, &site.title, &links, &counts, at, site.refresh_ms(), &body))
}

/// Range pills rendered directly after the monitor name: plain links (a
/// page reload switches ranges) with the active range marked current
/// instead of linked.
fn range_pills(id: &str, active: Range) -> String {
    let mut out = String::from("<nav class=range-pills aria-label=\"Time range\">");
    for option in RANGES {
        if option == active {
            let _ = write!(
                out,
                "<span aria-current=page title=\"Last {}\">{}</span>",
                option.label, option.short,
            );
        } else {
            let _ = write!(
                out,
                "<a href=\"/monitor?id={}&range={}\" aria-label=\"Show last {}\">{}</a>",
                esc(id),
                option.param,
                option.label,
                option.short,
            );
        }
    }
    out.push_str("</nav>");
    out
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
            <td data-label=\"Last 7 days\">{chart}</td></tr>",
            esc(place),
            time_html(recent.at, "rel", &vigil_core::time_label(recent.at)),
            ms = recent.ms,
        );
    }
    format!(
        "<table class=\"inc geo\"><caption class=sr-only>Latency by probe region over the last {window_label}</caption>\
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
    use super::{gap_in_range, geo_latency_table, last_probe_label, parse_range, RANGES};
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
    fn range_parses_known_params_and_defaults_to_90_days() {
        assert_eq!(parse_range(Some("24h")).days, 1);
        assert_eq!(parse_range(Some("7d")).days, 7);
        assert_eq!(parse_range(Some("30d")).days, 30);
        assert_eq!(parse_range(Some("90d")).days, 90);
        assert_eq!(parse_range(Some("7D")).days, 7);
        assert_eq!(parse_range(None).days, 90);
        assert_eq!(parse_range(Some("")).days, 90);
        assert_eq!(parse_range(Some("bogus")).days, 90);
        assert_eq!(RANGES.len(), 4);
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
}
