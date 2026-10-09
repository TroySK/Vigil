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
            "<tr><td>{}</td><td>{to}</td><td>{}</td></tr>",
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
        "<p><a href=\"/\">&lsaquo; all</a></p><h2>{}</h2>\
<p>{} &middot; {}</p>{notes}\
<h3>90 days</h3><div class=bars90>{cells}</div>\
<h3>Latency</h3><p>p50 {}ms &middot; p95 {}ms</p>{} \
<h3>Incidents</h3><table><tr><th>From</th><th>To</th><th>Reason</th></tr>{rows}</table>",
        esc(&monitor.name),
        if is_up { "UP" } else { "DOWN" },
        current.map_or_else(|| "—".to_owned(), |r| format!("{}ms", r.ms)),
        percentile(&vals, 50.0),
        percentile(&vals, 95.0),
        sparkline(&vals, 600, 60),
    );
    let links = links_html(&site.links);
    Response::from_html(shell(&monitor.name, &links, &counts, at, &body))
}
