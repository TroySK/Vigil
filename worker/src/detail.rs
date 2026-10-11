//! Per-monitor detail: status, 90-day bars, latency chart, incidents.

use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::page::{default_counts, last_tick_at, links_html, shell};
use crate::reads::{first_sample_at, latest, recent_gaps, series, GapOut, SampleOut};
use vigil_core::config::Site;
use vigil_core::rollup::{bar_color, latency_chart, month_bars, percentile};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Days of history on the detail page.
pub const DETAIL_HISTORY_DAYS: usize = 90;
/// Latency samples on the detail page.
pub const DETAIL_LATENCY_SAMPLES: i64 = 200;
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
            let _ = write!(
                cells,
                "<i aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" style=\"background:{}\">{:.1}%</i>",
                bar_color(bar.pct),
                bar.pct,
                tip = format!(
                    "{}: {:.1}%",
                    vigil_core::date_label(bar.day),
                    bar.pct
                ),
            );
        } else {
            let _ = write!(
                cells,
                "<i class=nodata aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\"></i>",
                tip = format!("{}: no data", vigil_core::date_label(bar.day)),
            );
        }
    }
    let bars_label = if covered == 0 {
        format!("{DETAIL_HISTORY_DAYS}-day history: no data yet")
    } else {
        format!(
            "{DETAIL_HISTORY_DAYS}-day history: {:.1}% average uptime over {} days with data.",
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
        let to = gap
            .to
            .map_or_else(|| "ongoing".to_owned(), vigil_core::time_label);
        let _ = write!(
            rows,
            "<tr><td data-label=\"From\">{}</td><td data-label=\"To\">{to}</td><td data-label=\"Reason\">{}</td></tr>",
            vigil_core::time_label(gap.at),
            esc(&gap.why)
        );
    }
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
        <h3 class=sect>90 days</h3><div class=bars90 role=img aria-label=\"{bars_label}\" tabindex=0>{cells}</div>\
        <h3 class=sect>Latency</h3><p class=tnum>p50 {p50}ms &middot; p95 {p95}ms &middot; {n} samples</p>{spark} \
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
        name = esc(&monitor.name),
    );

    let links = links_html(&site.links);
    Response::from_html(shell(&monitor.name, &site.title, &links, &counts, at, site.refresh_ms(), &body))
}

/// "418ms last probe · BOM" for the detail hero, or the empty state.
fn last_probe_label(current: Option<&SampleOut>) -> String {
    current.map_or_else(
        || "no probes yet".to_owned(),
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
    use super::last_probe_label;
    use crate::reads::SampleOut;

    fn sample(up: i64) -> SampleOut {
        SampleOut {
            at: 1_000,
            ms: 418,
            up,
            place: "BOM".into(),
        }
    }

    #[test]
    fn label_carries_latency_and_origin() {
        assert_eq!(
            last_probe_label(Some(&sample(1))),
            "418ms last probe &middot; BOM"
        );
        assert_eq!(last_probe_label(None), "no probes yet");
    }
}
