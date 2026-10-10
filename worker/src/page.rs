//! Status page: grouped monitor rows, history bars, latency sparklines.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::reads::{first_sample_at, latest, recent_gaps, series};
use vigil_core::config::{Monitor, Site};
use vigil_core::rollup::{bar_color, month_bars, percentile, sparkline_points};
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
    let (up, down) = parse_counts(&counts);
    let hero = hero_html(site, up, down, at);
    let mut body = hero;
    body.push_str(&sections(db, site, at, None).await?);
    let links = links_html(&site.links);
    Response::from_html(shell(&site.title, &links, &counts, at, &body))
}

/// Parse an `up/down` counts string; malformed input yields `(0, 0)`.
fn parse_counts(counts: &str) -> (u32, u32) {
    counts.split_once('/').map_or((0, 0), |(up, down)| {
        (up.parse().unwrap_or(0), down.parse().unwrap_or(0))
    })
}

/// Stat-Led hero (H4 + T4): one focal figure plus a three-stat strip.
/// All numbers are live counts — never invented.
fn hero_html(site: &Site, up: u32, down: u32, at: i64) -> String {
    let total = up.saturating_add(down);
    let (figure, qualifier) = if total == 0 {
        (
            "—".to_owned(),
            "No probes recorded yet. The next cron tick fills this in.".to_owned(),
        )
    } else if down == 0 {
        (
            up.to_string(),
            format!(
                "monitors up out of {total} checked. Last probe {stamp}.",
                stamp = vigil_core::time_label(at.max(1))
            ),
        )
    } else {
        (
            down.to_string(),
            format!(
                "monitors down out of {total} checked. Last probe {stamp}.",
                stamp = vigil_core::time_label(at.max(1))
            ),
        )
    };
    let state = if total == 0 || down > 0 { "0" } else { "1" };
    format!(
        "<section class=\"hero-stat\" data-up=\"{state}\">\
        <p class=\"figure tnum\" data-count=\"{figure}\">{figure}</p>\
        <p class=\"qualifier\">{qualifier}</p>\
        <div class=\"stat-strip tnum\">\
        <div><b>{up}</b><span>up</span></div>\
        <div><b>{down}</b><span>down</span></div>\
        <div><b>{total}</b><span>monitors</span></div>\
        </div>\
        <p class=\"site-sub\">{sub}</p></section>",
        sub = esc(&site.title),
    )
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
        let _ = write!(out, "<h2 class=group>{}</h2>{rows}", esc(group_name));
    }
    if only_group.is_none() {
        let mut rest = String::new();
        for monitor in &site.monitors {
            if !covered.contains(&monitor.id) {
                rest.push_str(&one(db, monitor, at).await?);
            }
        }
        if !rest.is_empty() {
            let _ = write!(out, "<h2 class=group>Other</h2>{rest}");
        }
    }
    Ok(out)
}

/// One monitor row: status dot, name, history bars, latency sparkline.
/// Rendered as a tabular spec-sheet row (F3): hairline rule, tabular
/// numerics, status carried by dot + word (never color alone).
async fn one(db: &Db, monitor: &Monitor, at: i64) -> Result<String> {
    let current = latest(db, &monitor.id).await?;
    let is_up = current.as_ref().map_or(true, |r| r.up == 1);
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
    let since = first_sample_at(db, &monitor.id).await?;
    for bar in month_bars(&core, at.max(1), INDEX_HISTORY_DAYS, since) {
        if bar.has_data {
            let _ = write!(
                cells,
                "<i title=\"{tip}\" data-tip=\"{tip}\" style=\"background:{}\"></i>",
                bar_color(bar.pct),
                tip = format!(
                    "{}: {:.1}%",
                    vigil_core::date_label(bar.day),
                    bar.pct
                ),
            );
        } else {
            let _ = write!(
                cells,
                "<i class=nodata title=\"{tip}\" data-tip=\"{tip}\"></i>",
                tip = format!("{}: no data", vigil_core::date_label(bar.day)),
            );
        }
    }
    let mut extra = format!("<div class=bars>{cells}</div>");
    if !monitor.quiet_chart {
        let lat = series(db, &monitor.id, INDEX_LATENCY_SAMPLES).await?;
        let vals: Vec<i64> = lat.iter().rev().map(|r| r.ms).collect();
        let points: Vec<(i64, i64)> = lat.iter().rev().map(|r| (r.at, r.ms)).collect();
        let spark = sparkline_points(&points, 180, 28);
        let chart = if spark.is_empty() {
            "<s>no samples yet</s>".to_owned()
        } else {
            spark
        };
        let _ = write!(
            extra,
            "<div class=lat>{chart} <s>p50 {}ms &middot; p95 {}ms</s></div>",
            percentile(&vals, 50.0),
            percentile(&vals, 95.0)
        );
    }
    let tip = monitor
        .tip
        .as_deref()
        .map_or_else(String::new, |t| format!(" title=\"{}\"", esc(t)));
    let flag = if is_up { "1" } else { "0" };
    let name = match &monitor.href {
        Some(href) => format!("<a href=\"{}\">{}</a>", esc(href), esc(&monitor.name)),
        None => esc(&monitor.name),
    };
    Ok(format!(
        "<article class=r data-up=\"{flag}\"{tip}>\
        <div class=r-head><span class=d aria-hidden=true></span>\
        <b>{name}</b><i>{word}</i><s>{detail}</s></div>\
        <div class=more><a href=\"/monitor?id={}\">details &rsaquo;</a></div>{extra}</article>",
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

/// Full HTML shell: head, Cobalt tokens, N9 header, body, Ft2 footer.
///
/// Stamp: Hallmark genre modern-minimal · macrostructure Stat-Led ·
/// theme Cobalt · nav N9 · footer Ft2 · enrichment none.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn shell(title: &str, links: &str, counts: &str, at: i64, body: &str) -> String {
    let down: u32 = counts
        .split_once('/')
        .and_then(|(_, d)| d.parse().ok())
        .unwrap_or(0);
    let pill = if down == 0 { "1" } else { "0" };
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
        <meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
        <link rel=icon href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">\
        <link rel=preconnect href=\"https://fonts.googleapis.com\">\
        <link rel=preconnect href=\"https://fonts.gstatic.com\" crossorigin>\
        <link rel=stylesheet href=\"https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;700&family=IBM+Plex+Sans:wght@400;600&family=JetBrains+Mono:wght@400;600&display=swap\">\
        <title>{}</title><style>\
        /* Hallmark · macrostructure: Stat-Led · tone: technical · anchor hue: cobalt-blue */\
        :root{{--color-paper:oklch(98% .004 250);--color-paper-2:oklch(94.5% .007 250);\
        --color-rule:oklch(87% .009 250);--color-neutral:oklch(55% .012 250);\
        --color-muted:oklch(42% .011 250);--color-ink:oklch(21% .011 250);\
        --color-accent:oklch(55% .22 260);--color-focus:oklch(55% .22 260);\
        --color-ok:oklch(60% .16 150);--color-bad:oklch(55% .2 25);\
        --font-display:\"Space Grotesk\",ui-sans-serif,system-ui,sans-serif;\
        --font-body:\"IBM Plex Sans\",ui-sans-serif,system-ui,sans-serif;\
        --font-mono:\"JetBrains Mono\",ui-monospace,monospace;\
        --space-3xs:.125rem;--space-2xs:.25rem;--space-xs:.5rem;--space-sm:.75rem;\
        --space-md:1rem;--space-lg:1.5rem;--space-xl:2.5rem;--space-2xl:4rem;--space-3xl:6rem;\
        --text-xs:.8rem;--text-sm:.875rem;--text-base:1rem;--text-md:1.25rem;\
        --text-lg:1.5625rem;--text-xl:1.9531rem;\
        --text-display:clamp(2.75rem,5vw + 1rem,5.25rem);\
        --ease-out:cubic-bezier(.16,1,.3,1);--ease-in:cubic-bezier(.7,0,.84,0);\
        --ease-in-out:cubic-bezier(.65,0,.35,1);\
        --dur-micro:120ms;--dur-short:220ms;--dur-long:420ms;\
        --radius-s:6px;--radius-m:10px;--z-base:1;--z-sticky:200;}}\
        html{{overflow-x:clip}}\
        body{{overflow-x:clip;background:var(--color-paper);color:var(--color-ink);\
        font-family:var(--font-body);font-size:var(--text-base);line-height:1.6;\
        margin:0}}\
        .wrap{{max-width:56rem;margin:0 auto;padding-inline:clamp(1rem,4vw,1.5rem);\
        padding-block:var(--space-lg) var(--space-2xl)}}\
        a{{color:var(--color-accent);text-underline-offset:2px}}\
        a:hover{{color:var(--color-ink)}}\
        :focus-visible{{outline:2px solid var(--color-focus);outline-offset:2px}}\
        .nav-edge{{display:flex;justify-content:space-between;align-items:center;gap:var(--space-md);\
        padding-block:var(--space-md)}}\
        .brand{{display:flex;gap:var(--space-sm);align-items:center;min-width:0}}\
        .brand img{{width:32px;height:32px;flex:none}}\
        .wordmark{{font-family:var(--font-display);font-weight:700;font-size:var(--text-md);\
        letter-spacing:-.02em;color:var(--color-ink);text-decoration:none;white-space:nowrap;\
        overflow-wrap:anywhere;min-width:0}}\
        .pill{{font-family:var(--font-body);font-size:var(--text-xs);white-space:nowrap;\
        border:1px solid var(--color-rule);border-radius:999px;padding:var(--space-2xs) var(--space-sm);\
        color:var(--color-muted);background:var(--color-paper-2)}}\
        .pill[data-up=\"1\"]{{color:var(--color-ink);border-color:var(--color-ok)}}\
        .pill[data-up=\"0\"]{{color:var(--color-ink);border-color:var(--color-bad)}}\
        .links{{display:flex;flex-wrap:wrap;gap:var(--space-2xs) var(--space-md);\
        font-size:var(--text-sm);padding-block:var(--space-sm);border-block:1px solid var(--color-rule)}}\
        .links a{{white-space:nowrap}}\
        .hero-stat{{padding-block:var(--space-md) var(--space-xl)}}\
        .figure{{font-family:var(--font-display);font-weight:700;font-style:normal;\
        font-size:clamp(4rem,12vw,7rem);line-height:1;letter-spacing:-.03em;margin:0;\
        font-variant-numeric:tabular-nums;overflow-wrap:anywhere;min-width:0}}\
        .hero-stat[data-up=\"1\"] .figure{{color:var(--color-ink)}}\
        .hero-stat[data-up=\"0\"] .figure{{color:var(--color-bad)}}\
        .qualifier{{font-size:var(--text-md);color:var(--color-muted);max-width:65ch;margin:var(--space-sm) 0 0}}\
        .stat-strip{{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:var(--space-md);\
        margin-top:var(--space-lg);padding-block:var(--space-md);border-block:1px solid var(--color-rule)}}\
        .stat-strip b{{display:block;font-family:var(--font-display);font-weight:700;\
        font-size:var(--text-xl);font-variant-numeric:tabular-nums;line-height:1.1}}\
        .stat-strip span{{font-family:var(--font-body);font-size:var(--text-xs);\
        text-transform:uppercase;letter-spacing:.1em;color:var(--color-muted)}}\
        .site-sub{{font-family:var(--font-body);font-size:var(--text-xs);color:var(--color-muted);\
        text-transform:uppercase;letter-spacing:.1em}}\
        h2.group{{font-family:var(--font-display);font-weight:700;font-style:normal;\
        font-size:var(--text-lg);letter-spacing:-.02em;margin:var(--space-xl) 0 var(--space-xs);\
        overflow-wrap:anywhere;min-width:0}}\
        .r{{border-top:1px solid var(--color-rule);padding-block:var(--space-md)}}\
        .r-head{{display:grid;grid-template-columns:minmax(0,1fr);gap:var(--space-2xs) var(--space-md);\
        align-items:baseline}}\
        @media (min-width:40rem){{\
        .r-head{{grid-template-columns:12px minmax(0,1.4fr) auto minmax(0,1fr);align-items:center}}}}\
        .d{{width:10px;height:10px;border-radius:50%;background:var(--color-neutral)}}\
        .r[data-up=\"1\"] .d{{background:var(--color-ok)}}\
        .r[data-up=\"0\"] .d{{background:var(--color-bad)}}\
        .r b{{font-family:var(--font-display);font-weight:700;letter-spacing:-.01em;\
        overflow-wrap:anywhere;min-width:0}}\
        .r b a{{color:var(--color-ink);text-decoration:none}}\
        .r b a:hover{{color:var(--color-accent)}}\
        .r i{{font-style:normal;font-family:var(--font-mono);font-size:var(--text-xs);\
        font-weight:600;letter-spacing:.08em}}\
        .r[data-up=\"1\"] i{{color:var(--color-muted)}}\
        .r[data-up=\"0\"] i{{color:var(--color-bad)}}\
        .r s{{color:var(--color-muted);text-decoration:none;font-size:var(--text-sm);\
        font-variant-numeric:tabular-nums}}\
        @media (min-width:40rem){{.r s{{text-align:right}}}}\
        .more{{font-size:var(--text-sm);margin-top:var(--space-2xs)}}\
        .more a{{white-space:nowrap}}\
        .bars{{display:flex;gap:var(--space-3xs);margin:var(--space-sm) 0 var(--space-xs)}}\
        .bars i,.bars90 i{{position:relative;box-sizing:border-box}}\
        .bars i.nodata,.bars90 i.nodata{{background:var(--color-paper-2);\
        border:1px dashed var(--color-rule)}}\
        .bars i::after,.bars90 i::after{{content:attr(data-tip);position:absolute;\
        bottom:calc(100% + var(--space-xs));left:50%;transform:translateX(-50%);\
        font-family:var(--font-body);font-size:var(--text-base);font-weight:600;line-height:1.4;\
        color:var(--color-ink);background:var(--color-paper-2);\
        border:1px solid var(--color-rule);border-radius:var(--radius-m);\
        padding:var(--space-xs) var(--space-sm);white-space:nowrap;\
        opacity:0;pointer-events:none;z-index:var(--z-sticky);\
        transition:opacity var(--dur-micro) var(--ease-out)}}\
        .bars i:first-child::after,.bars90 i:first-child::after{{left:0;transform:none}}\
        .bars i:last-child::after,.bars90 i:last-child::after{{left:auto;right:0;transform:none}}\
        .bars i:hover::after,.bars i:active::after,\
        .bars90 i:hover::after,.bars90 i:active::after{{opacity:1}}\
        .bars i{{flex:1 1 0;min-width:3px;height:28px;border-radius:2px;display:block}}\
        .bars90{{display:flex;flex-wrap:wrap;gap:var(--space-3xs);margin:var(--space-sm) 0}}\
        .bars90 i{{flex:1 1 0;min-width:4px;height:34px;border-radius:2px;display:block;\
        font-size:0;line-height:0}}\
        .lat{{display:flex;gap:var(--space-sm);align-items:center;color:var(--color-muted);\
        font-size:var(--text-xs);font-variant-numeric:tabular-nums;margin:0 0 var(--space-md)}}\
        .lat svg{{flex:none;max-width:100%;height:auto}}\
        .lat s{{text-decoration:none;white-space:nowrap}}\
        .r,.more,.lat{{transition:transform var(--dur-short) var(--ease-out)}}\
        @media (hover:hover) and (pointer:fine){{\
        .r:hover{{transform:translateY(-1px)}}}}\
        .note{{padding:var(--space-sm) var(--space-md);border-radius:var(--radius-m);\
        margin:var(--space-md) 0;border:1px solid var(--color-rule);\
        background:var(--color-paper-2);font-size:var(--text-sm)}}\
        .empty{{color:var(--color-muted);font-size:var(--text-sm)}}\
        .active{{border-color:var(--color-bad)}}\
        .upcoming{{border-color:var(--color-rule)}}\
        .detail-hero{{padding-block:var(--space-lg) 0}}\
        .crumb{{font-size:var(--text-sm);white-space:nowrap}}\
        .detail-hero h2{{font-family:var(--font-display);font-weight:700;font-style:normal;\
        font-size:clamp(2rem,4vw + 1rem,3rem);letter-spacing:-.025em;line-height:1.05;\
        margin:var(--space-sm) 0 var(--space-2xs);overflow-wrap:anywhere;min-width:0}}\
        .status-line{{display:flex;flex-wrap:wrap;gap:var(--space-2xs) var(--space-md);\
        align-items:baseline;font-variant-numeric:tabular-nums}}\
        .status-word{{font-family:var(--font-mono);font-weight:600;font-size:var(--text-sm);\
        letter-spacing:.08em}}\
        .status-word[data-up=\"1\"]{{color:var(--color-ok)}}\
        .status-word[data-up=\"0\"]{{color:var(--color-bad)}}\
        h3.sect{{font-family:var(--font-display);font-weight:700;font-style:normal;\
        font-size:var(--text-md);letter-spacing:-.01em;margin:var(--space-xl) 0 var(--space-2xs);\
        overflow-wrap:anywhere;min-width:0}}\
        table.inc{{border-collapse:collapse;width:100%;font-size:var(--text-sm);\
        font-variant-numeric:tabular-nums}}\
        table.inc th{{font-family:var(--font-body);font-size:var(--text-xs);text-transform:uppercase;\
        letter-spacing:.08em;color:var(--color-muted);text-align:left;\
        border-bottom:1px solid var(--color-rule);padding:var(--space-xs) var(--space-xs) var(--space-xs) 0}}\
        table.inc td{{border-bottom:1px solid var(--color-rule);padding:var(--space-xs) var(--space-xs) var(--space-xs) 0;\
        vertical-align:top;overflow-wrap:anywhere}}\
        @media (max-width:40rem){{\
        table.inc thead{{position:absolute;left:-9999px}}\
        table.inc tr{{display:block;border-top:1px solid var(--color-rule);padding-block:var(--space-xs)}}\
        table.inc td{{display:block;border:0;padding:var(--space-3xs) 0}}\
        table.inc td::before{{content:attr(data-label);display:block;font-family:var(--font-body);\
        font-size:var(--text-xs);text-transform:uppercase;letter-spacing:.08em;color:var(--color-muted)}}}}\
        .foot-line{{border-top:1px solid var(--color-rule);margin-top:var(--space-2xl);\
        padding-top:var(--space-md);font-size:var(--text-sm);color:var(--color-muted);\
        display:flex;flex-wrap:wrap;gap:var(--space-2xs) var(--space-md)}}\
        .foot-line a{{white-space:nowrap}}\
        .meta{{font-family:var(--font-body);font-size:var(--text-xs);color:var(--color-muted)}}\
        a:active{{color:var(--color-ink)}}\
        @media (prefers-reduced-motion:reduce){{\
        *,*::before,*::after{{animation-duration:150ms !important;transition-duration:150ms !important}}\
        .r:hover{{transform:none}}}}\
        </style></head><body><div class=wrap>\
        <header class=nav-edge><a class=wordmark href=\"/\"><span class=brand><img src=\"/logo.svg\" alt=\"\" width=32 height=32>{}</span></a><span class=pill data-up=\"{pill}\">{counts}</span></header>\
        <nav class=links aria-label=\"Site\">{links}</nav>\
        <main>{body}</main>\
        <footer class=foot-line><span>{}</span><span class=meta>checked {}</span></footer>\
        </div><script>(function(){{var el=document.querySelector('[data-count]');\
        if(!el)return;var raw=el.getAttribute('data-count');var target=parseInt(raw,10);\
        if(!isFinite(target))return;\
        if(window.matchMedia&&window.matchMedia('(prefers-reduced-motion: reduce)').matches)return;\
        var t0=null,dur=600;function step(t){{if(!t0)t0=t;var p=Math.min(1,(t-t0)/dur);\
        var e=1-Math.pow(1-p,3);el.textContent=String(Math.round(target*e));\
        if(p<1)requestAnimationFrame(step);}}requestAnimationFrame(step);}})();</script></body></html>",
        esc(title),
        esc(title),
        esc(title),
        vigil_core::time_label(at.max(0))
    )
}
