//! Per-monitor detail: status, 90-day bars, latency chart, incidents.

use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::page::{default_counts, last_tick_at, links_html, shell};
use crate::reads::{latest, recent_gaps, series};
use vigil_core::config::Site;
use vigil_core::rollup::{month_bars, percentile, sparkline};
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
pub async fn render(db: &Db, site: &Site, id: &str) -> Result<Response> {
    let Some(monitor) = site.get(id) else {
        return Response::error("unknown monitor", 404);
    };
    let at = last_tick_at(db).await?;
    let counts = db.get("counts").await?.unwrap_or_else(default_counts);
    let current = latest(db, id).await?;
    let is_up = current.as_ref().map_or(true, |r| r.up == 1);
    let gaps = recent_gaps(db, id, crate::api::GAP_SCAN_ROWS).await?;
    let core: Vec<Gap> = gaps
        .iter()
        .map(|g| Gap {
            at: g.at,
            to: g.to,
            why: g.why.clone(),
        })
        .collect();
    let mut cells = String::new();
    for bar in month_bars(&core, at.max(1), DETAIL_HISTORY_DAYS) {
        let _ = write!(
            cells,
            "<i title=\"{}\">{:.1}%</i>",
            vigil_core::time_label(bar.day),
            bar.pct
        );
    }
    let lat = series(db, id, DETAIL_LATENCY_SAMPLES).await?;
    let vals: Vec<i64> = lat.iter().rev().map(|r| r.ms).collect();
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
        "<nav class=crumb><a href=\"/\">&lsaquo; all monitors</a></nav>\
        <section class=detail-hero><h2>{}</h2>\
        <p class=status-line><span class=status-word data-up=\"{flag}\">{word}</span> \
        <span class=tnum>{ms}</span></p>{notes}</section>\
        <h3 class=sect>90 days</h3><div class=bars90>{cells}</div>\
        <h3 class=sect>Latency</h3><p class=tnum>p50 {p50}ms &middot; p95 {p95}ms &middot; {n} samples</p>{spark} \
        <h3 class=sect>Incidents</h3><table class=inc><thead><tr><th>From</th><th>To</th><th>Reason</th></tr></thead><tbody>{rows}</tbody></table>",
        esc(&monitor.name),
        word = if is_up { "UP" } else { "DOWN" },
        flag = if is_up { "1" } else { "0" },
        ms = current.map_or_else(|| "no probes yet".to_owned(), |r| format!("{}ms last probe", r.ms)),
        p50 = percentile(&vals, 50.0),
        p95 = percentile(&vals, 95.0),
        n = vals.len(),
        spark = sparkline(&vals, 600, 60),
    );
    let links = links_html(&site.links);
    Response::from_html(shell(&monitor.name, &links, &counts, at, &body))
}
