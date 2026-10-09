//! Status page: grouped monitor rows, history bars, latency sparklines.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::reads::{latest, recent_gaps, series};
use vigil_core::config::{Monitor, Site};
use vigil_core::rollup::{bar_color, month_bars, percentile, sparkline};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Days of history shown on the index page.
pub const INDEX_HISTORY_DAYS: usize = 30;
/// Latency samples per sparkline on the index page.
pub const INDEX_LATENCY_SAMPLES: i64 = 60;

/// Render the full status page.
pub async fn render(db: &Db, site: &Site) -> Result<Response> {
    let at = last_tick_at(db).await?;
    let counts = db.get("counts").await?.unwrap_or_else(default_counts);
    let body = sections(db, site, at, None).await?;
    let links = links_html(&site.links);
    Response::from_html(shell(&site.title, &links, &counts, at, &body))
}

/// Render upkeep banners plus monitor sections (optionally one group only).
pub async fn sections(db: &Db, site: &Site, at: i64, only_group: Option<&str>) -> Result<String> {
    let mut out = String::new();
    for upkeep in &site.upkeep {
        if let Some((kind, text)) = vigil_core::upkeep::upkeep_banner(upkeep, at) {
            let _ = write!(out, "<div class=\"note {kind}\">{}</div>", esc(&text));
        }
    }
    if site.groups.is_empty() {
        for monitor in &site.monitors {
            out.push_str(&one(db, monitor, at).await?);
        }
        return Ok(out);
    }
    let mut covered = BTreeSet::new();
    for (group_name, ids) in &site.groups {
        if only_group.is_some_and(|only| only != group_name) {
            continue;
        }
        let mut rows = String::new();
        for id in ids {
            if let Some(monitor) = site.get(id) {
                covered.insert(id.clone());
                rows.push_str(&one(db, monitor, at).await?);
            }
        }
        let _ = write!(out, "<h2>{}</h2>{rows}", esc(group_name));
    }
    if only_group.is_none() {
        let mut rest = String::new();
        for monitor in &site.monitors {
            if !covered.contains(&monitor.id) {
                rest.push_str(&one(db, monitor, at).await?);
            }
        }
        if !rest.is_empty() {
            let _ = write!(out, "<h2>Other</h2>{rest}");
        }
    }
    Ok(out)
}

/// One monitor row: status dot, name, history bars, latency sparkline.
async fn one(db: &Db, monitor: &Monitor, at: i64) -> Result<String> {
    let current = latest(db, &monitor.id).await?;
    let is_up = current.as_ref().map_or(true, |r| r.up == 1);
    let color = if is_up { "#2da44e" } else { "#cf222e" };
    let word = if is_up { "UP" } else { "DOWN" };
    let detail = match &current {
        Some(sample) if is_up => {
            format!(
                "{}ms &middot; {}",
                sample.ms,
                vigil_core::time_label(sample.at)
            )
        }
        Some(_) => "check failing".to_owned(),
        None => "no data yet".to_owned(),
    };
    let gaps = recent_gaps(db, &monitor.id, crate::api::GAP_SCAN_ROWS).await?;
    let core: Vec<Gap> = gaps
        .iter()
        .map(|g| Gap {
            at: g.at,
            to: g.to,
            why: g.why.clone(),
        })
        .collect();
    let mut cells = String::new();
    for bar in month_bars(&core, at.max(1), INDEX_HISTORY_DAYS) {
        let _ = write!(
            cells,
            "<i title=\"{}: {}%\" style=\"background:{}\"></i>",
            vigil_core::time_label(bar.day),
            bar.pct,
            bar_color(bar.pct)
        );
    }
    let mut extra = format!("<div class=bars>{cells}</div>");
    if !monitor.quiet_chart {
        let lat = series(db, &monitor.id, INDEX_LATENCY_SAMPLES).await?;
        let vals: Vec<i64> = lat.iter().rev().map(|r| r.ms).collect();
        let _ = write!(
            extra,
            "<div class=lat>{} <s>p50 {}ms &middot; p95 {}ms</s></div>",
            sparkline(&vals, 180, 28),
            percentile(&vals, 50.0),
            percentile(&vals, 95.0)
        );
    }
    let tip = monitor
        .tip
        .as_deref()
        .map_or_else(String::new, |t| format!(" title=\"{}\"", esc(t)));
    let name = match &monitor.href {
        Some(href) => format!("<a href=\"{}\">{}</a>", esc(href), esc(&monitor.name)),
        None => esc(&monitor.name),
    };
    Ok(format!(
        "<div class=r{tip}><span class=d style=\"background:{color}\"></span>\
<b>{name}</b><i>{word}</i><s>{detail}</s></div>\
<div class=more><a href=\"/monitor?id={}\">details &rsaquo;</a></div>{extra}",
        monitor.id
    ))
}

/// Footer links (`label -> url`) joined with middots.
#[must_use]
pub fn links_html(links: &BTreeMap<String, String>) -> String {
    links
        .iter()
        .map(|(label, url)| format!("<a href=\"{}\">{}</a>", esc(url), esc(label)))
        .collect::<Vec<_>>()
        .join(" &middot; ")
}

/// Last cron tick time, `0` when the worker never ticked.
pub async fn last_tick_at(db: &Db) -> Result<i64> {
    Ok(db
        .get("at")
        .await?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// Default `up/down` counts string before the first tick.
#[must_use]
pub fn default_counts() -> String {
    "0/0".to_owned()
}

/// Full HTML shell: head, styles, title, footer, and body.
#[must_use]
pub fn shell(title: &str, links: &str, counts: &str, at: i64, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<link rel=icon href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">\
<title>{}</title><style>body{{font-family:system-ui;max-width:680px;margin:32px auto;padding:0 16px}}\
.brand{{display:flex;gap:10px;align-items:center}}.brand img{{width:32px;height:32px}}\
h1{{font-size:24px;margin:0}}\
.r{{display:flex;gap:8px;padding:8px 0;border-bottom:1px solid #eee}}\
.d{{width:10px;height:10px;border-radius:50%;margin-top:6px}}s{{color:#666;text-decoration:none;margin-left:auto}}\
.bars{{display:flex;gap:2px;margin:4px 0 12px}}.bars i{{width:14px;height:28px;border-radius:3px;display:block}}\
.lat{{color:#666;font-size:12px;margin:0 0 12px;display:flex;gap:8px;align-items:center}}\
.more{{font-size:12px}}.note{{padding:8px 12px;border-radius:8px;margin:12px 0}}\
.active{{background:#fff8c5}}.upcoming{{background:#f6f8fa}}\
table{{border-collapse:collapse;width:100%;font-size:13px}}\
td,th{{border-bottom:1px solid #eee;padding:6px;text-align:left}}</style>\
<header class=brand><img src=\"/logo.svg\" alt=\"Vigil logo\" width=32 height=32><h1>{}</h1></header><p>{links}</p><p>{counts} &middot; {}</p>{body}",
        esc(title),
        esc(title),
        vigil_core::time_label(at)
    )
}
