//! Status page: grouped monitor rows, history bars, latency sparklines.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::range::{bars_for, Range};
use crate::reads::{
    data_at_of, gaps_window_by_monitor, latest_and_first_by_monitor, series_by_monitor,
    SampleOut,
};
use vigil_core::config::{Monitor, Probe, Site};
use vigil_core::rollup::{bar_color, latency_chart, percentile_pair};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Latency chart points per sparkline on the index page: each monitor's
/// window spreads at most this many dots (see `series_by_monitor`).
pub const INDEX_CHART_POINTS: i64 = 30;

/// Render the full status page.
pub async fn render(db: &Db, site: &Site, range: Range) -> Result<Response> {
    let at = last_tick_at(db).await?;
    let counts = db.get("counts").await?.unwrap_or_else(default_counts);
    // Last tick that probed something: the refresh poller reloads on this,
    // not on the tick clock, so skip-only ticks cost a cheap poll.
    let probed = data_at_of(db.get("data_at").await?, at);
    let (up, down) = parse_counts(&counts);
    let hero = hero_html(site, up, down, at);
    let mut body = hero;
    body.push_str(&sections(db, site, at, None, range).await?);
    let links = links_html(&site.links);
    let menu = crate::range::menu(range, |option| format!("/?range={}", option.param));
    let home = "/";
    let speculation = speculation_rule("/monitor*");
    Response::from_html(shell(
        &site.title,
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
                "Monitors up out of {total} checked. Last probe {stamp}.",
                stamp = time_html(at.max(1), "rel", &vigil_core::time_label(at.max(1)))
            ),
        )
    } else {
        (
            down.to_string(),
            format!(
                "Monitors down out of {total} checked. Last probe {stamp}.",
                stamp = time_html(at.max(1), "rel", &vigil_core::time_label(at.max(1)))
            ),
        )
    };
    let state = if total == 0 {
        "2"
    } else if down > 0 {
        "0"
    } else {
        "1"
    };
    format!(
        "<section class=\"hero-stat\" data-up=\"{state}\" aria-live=polite>\
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
pub async fn sections(
    db: &Db,
    site: &Site,
    at: i64,
    only_group: Option<&str>,
    range: Range,
) -> Result<String> {
    let mut out = String::new();
    for upkeep in &site.upkeep {
        if let Some((kind, text)) = vigil_core::upkeep::upkeep_banner(upkeep, at) {
            let _ = write!(out, "<div class=\"note {kind}\">{}</div>", esc(&text));
        }
    }
    // Three batched D1 round trips cover every monitor (was: four per
    // monitor, ~120 trips at 30 monitors). The homepage is the
    // highest-traffic page; per-monitor fan-out risked the same CPU
    // exhaustion that killed the tick.
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let now = at.max(1);
    let cutoff = now.saturating_sub(range.secs());
    // Sparklines always pulse the last 24 hours while bars follow the
    // range: a range-wide sparkline would scan the full window per monitor
    // per load (~777k rows at 90 days), burning the D1 rows-read quota for
    // a glanceable trend. Bars carry the range history instead.
    // `quiet_chart` monitors skip the sparkline query entirely: their
    // series were fetched and then discarded.
    let pulse_cutoff = now.saturating_sub(86_400);
    let chart_ids: Vec<&str> = site
        .monitors
        .iter()
        .filter(|monitor| !monitor.quiet_chart)
        .map(|monitor| monitor.id.as_str())
        .collect();
    let (latest_first, gap_lists, series) = futures_util::try_join!(
        latest_and_first_by_monitor(db, &ids),
        gaps_window_by_monitor(db, &ids, cutoff, now),
        series_by_monitor(db, &chart_ids, pulse_cutoff, INDEX_CHART_POINTS),
    )?;
    let (latest, since_map) = latest_first;
    // Rows assemble synchronously in file order; groups slice them below.
    let rows: BTreeMap<String, String> = site
        .monitors
        .iter()
        .map(|monitor| {
            (
                monitor.id.clone(),
                one_row(
                    monitor,
                    range,
                    at,
                    latest.get(&monitor.id),
                    gap_lists.get(&monitor.id).map_or(&[], Vec::as_slice),
                    since_map.get(&monitor.id).copied(),
                    series.get(&monitor.id).map_or(&[], Vec::as_slice),
                ),
            )
        })
        .collect();
    let row = |id: &str| rows.get(id).cloned().unwrap_or_default();
    if site.groups.is_empty() {
        for monitor in &site.monitors {
            out.push_str(&row(&monitor.id));
        }
        return Ok(out);
    }
    let mut covered = BTreeSet::new();
    for (group_name, ids) in &site.groups {
        if only_group.is_some_and(|only| only != group_name) {
            continue;
        }
        let mut group_rows = String::new();
        for id in ids {
            if site.get(id).is_some() {
                covered.insert(id.clone());
                group_rows.push_str(&row(id));
            }
        }
        let _ = write!(out, "<h2 class=group>{}</h2>{group_rows}", esc(group_name));
    }
    if only_group.is_none() {
        let mut rest = String::new();
        for monitor in &site.monitors {
            if !covered.contains(&monitor.id) {
                rest.push_str(&row(&monitor.id));
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
///
/// Pure: all reads arrive batched from [`sections`], so row rendering costs
/// no I/O and is unit-testable. `gaps` covers the selected window, `lat`
/// its chart samples, `since` the monitor's first sample (if any).
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn one_row(
    monitor: &Monitor,
    range: Range,
    at: i64,
    current: Option<&SampleOut>,
    gaps: &[Gap],
    since: Option<i64>,
    lat: &[SampleOut],
) -> String {
    let is_up = current.map_or(true, |r| r.up == 1);
    let word = if is_up { "UP" } else { "DOWN" };
    let detail = match current {
        Some(sample) if is_up => {
            format!(
                "{}ms &middot; {} &middot; {}",
                sample.ms,
                time_html(sample.at, "rel", &vigil_core::time_label(sample.at)),
                esc(&sample.place)
            )
        }
        Some(_) => "Check failing".to_owned(),
        None => "No data yet".to_owned(),
    };
    let strip = bars_for(gaps, at.max(1), range, since);
    let mut cells = String::new();
    let mut covered = 0_usize;
    let mut sum = 0.0_f64;
    let label = strip.label;
    for bar in &strip.cells {
        if bar.has_data {
            covered += 1;
            sum += bar.pct;
            let v = format!("{:.1}%", bar.pct);
            let _ = write!(
                cells,
                "<i aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"{kind}\" data-v=\"{v}\" style=\"background:{}\"></i>",
                bar_color(bar.pct),
                day = bar.day,
                kind = strip.kind,
                tip = format!("{} UTC: {:.1}%", label(bar.day), bar.pct),
            );
        } else {
            let _ = write!(
                cells,
                "<i class=nodata aria-hidden=true title=\"{tip}\" data-tip=\"{tip}\" data-ts=\"{day}\" data-tk=\"{kind}\"></i>",
                day = bar.day,
                kind = strip.kind,
                tip = format!("{} UTC: no data", label(bar.day)),
            );
        }
    }
    let bars_label = if covered == 0 {
        format!("Last {}: no data yet", range.label)
    } else {
        format!(
            "Last {}: {:.1}% average uptime over {} {} with data. {} is {}.",
            range.label,
            sum / covered as f64,
            covered,
            strip.period,
            monitor.name,
            word
        )
    };
    let mut extra = format!(
        "<div class={} role=img aria-label=\"{}\" tabindex=0>{cells}</div>",
        strip.class,
        esc(&bars_label)
    );
    if !monitor.quiet_chart {
        let vals: Vec<i64> = lat.iter().rev().map(|r| r.ms).collect();
        let points: Vec<(i64, i64, &str)> =
            lat.iter().rev().map(|r| (r.at, r.ms, r.place.as_str())).collect();
        // One sort for both percentiles (was: one sort each).
        let (p50, p95) = percentile_pair(&vals, 50.0, 95.0);
        let spark = latency_chart(&points, 560, 120, range.hourly());
        let chart = if spark.is_empty() {
            "<s>No samples yet</s>".to_owned()
        } else {
            format!(
                "<span role=img aria-label=\"Latency over last {} samples: P50 {}ms, P95 {}ms.\">{spark}</span>",
                vals.len(),
                p50,
                p95
            )
        };
        let _ = write!(
            extra,
            "<div class=lat>{chart} <s>P50 {p50}ms &middot; P95 {p95}ms</s></div>",
        );
    }
    let tip = monitor
        .tip
        .as_deref()
        .map_or_else(String::new, |t| format!(" title=\"{}\"", esc(t)));
    let flag = if is_up { "1" } else { "0" };
    let name = monitor_name(monitor);
    format!(
        "<article class=r data-up=\"{flag}\"{tip}>\
        <div class=r-head><span class=d aria-hidden=true></span>\
        <b>{name}</b><div class=r-meta><s>{detail}</s><i>{word}</i><a class=det href=\"/monitor?id={}&range={}\">details &rsaquo;</a></div></div>\
        {extra}</article>",
        monitor.id,
        range.param
    )
}

/// Web origin for a monitor's site icons, if it has an origin.
/// Always `https`: icons are decorative third-party loads, so they use TLS
/// even when the probe itself targets plain HTTP (avoids mixed content).
/// TCP monitors have no web origin and yield `None`.
#[must_use]
pub fn favicon_origin(monitor: &Monitor) -> Option<String> {
    let url = match &monitor.check {
        Probe::Http { url, .. } => url,
        Probe::Tcp { .. } => return None,
    };
    let parsed = url::Url::parse(url).ok()?;
    Some(format!("https://{}", parsed.host_str()?))
}

/// `<img>` for a monitor's favicon, logo-first per favicon-before-title
/// convention. Sites keep icons at different paths (`favicon.ico`,
/// `favicon.png`, …), so the tag carries the origin and a global
/// `nextFavicon` script walks the candidates on error, removing the
/// image when none exists.
pub(crate) fn favicon_img(monitor: &Monitor, size: u32) -> String {
    favicon_origin(monitor).map_or_else(String::new, |origin| {
        // `origin` is parser-constrained, but attribute context demands
        // escaping regardless of what today's parser guarantees.
        let src = esc(&origin);
        format!(
            "<img class=mlogo src=\"{src}/favicon.ico\" data-origin=\"{src}\" data-fb=0 alt=\"\" width={size} height={size} loading=lazy onerror='nextFavicon(this)'>",
        )
    })
}

/// Monitor display name with favicon, linked when `href` is set (new tab,
/// no opener). Logo-first per favicon-before-title convention.
fn monitor_name(monitor: &Monitor) -> String {
    let label = match &monitor.href {
        Some(href) => format!(
            "<a href=\"{}\" target=_blank rel=\"noopener noreferrer\">{}</a>",
            esc(href),
            esc(&monitor.name)
        ),
        None => esc(&monitor.name),
    };
    format!("{}{label}", favicon_img(monitor, 16))
}

/// Footer links (`label -> url`) joined with middots. External links open
/// in a new tab without opener access.
#[must_use]
pub fn links_html(links: &indexmap::IndexMap<String, String>) -> String {
    links
        .iter()
        .map(|(label, url)| {
            format!(
                "<a href=\"{}\" target=_blank rel=\"noopener noreferrer\">{}</a>",
                esc(url),
                esc(label)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
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

/// Inline `<time>` carrying its unix timestamp so the client can re-render
/// it without refetching. `kind` is `full`, `short` or `date` (absolute UTC
/// formats matching `text`), or `rel`: the server bakes the UTC `text` as a
/// no-JS fallback and the toggle script replaces it with a live relative age.
#[must_use]
pub fn time_html(at: i64, kind: &str, text: &str) -> String {
    format!("<time data-ts=\"{at}\" data-tk=\"{kind}\">{text}</time>")
}

/// Speculation-rules tag prerendering same-origin navigations matching
/// `pattern` on conservative intent (field visit, not hover): Chromium turns
/// likely next-page navigations into instant ones; other browsers ignore the
/// tag. Moderate eagerness prerendered on hover and doubled D1 load for
/// navigations that never happened. Targets are display-only GETs, so a
/// wasted prerender costs a few cached reads — and prerendered pages stay
/// hidden, where the pollers and rel-time interval already stand down.
#[must_use]
pub(crate) fn speculation_rule(pattern: &str) -> String {
    format!(
        "<script type=\"speculationrules\">{{\"prerender\":[{{\"source\":\"document\",\"where\":{{\"href_matches\":\"{pattern}\"}},\"eagerness\":\"conservative\"}}]}}</script>"
    )
}

/// Full HTML shell: head, Cobalt tokens, N9 header, body, Ft2 footer.
///
/// `page_title` names the browser tab; `brand` (always the site title) names
/// the header wordmark and footer so every page shares one header.
/// Theme: light by default, dark via toggle or OS preference, persisted.
///
/// Stamp: Hallmark genre modern-minimal · macrostructure Stat-Led ·
/// theme Cobalt · nav N9 · footer Ft2 · enrichment none.
#[must_use]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn shell(
    page_title: &str,
    brand: &str,
    links: &str,
    counts: &str,
    at: i64,
    refresh_ms: u64,
    range_menu: &str,
    home_url: &str,
    speculation: &str,
    probed_at: i64,
    body: &str,
) -> String {
    let (cup, cdown) = parse_counts(counts);
    let total = cup.saturating_add(cdown);
    let pill = if total == 0 {
        "2"
    } else if cdown == 0 {
        "1"
    } else {
        "0"
    };
    let uptotal = format!("{cup}/{total}");
    let uptip = format!("{cup} up, {cdown} down");
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
        <meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
        <meta name=color-scheme content=\"light dark\">\
        <meta name=description content=\"Vigil status page: live uptime, latency, and incident history.\">\
        <link rel=icon href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">\
        <link rel=preconnect href=\"https://fonts.googleapis.com\">\
        <link rel=preconnect href=\"https://fonts.gstatic.com\" crossorigin>\
        <link rel=stylesheet href=\"https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600&display=swap\" media=print onload=\"this.media='all'\">\
        <script>(function(){{var t=null;try{{t=localStorage.getItem('vigil-theme');}}catch(e){{}}\
        if(!t)t=(window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches)?'dark':'light';\
        document.documentElement.setAttribute('data-theme',t);}})();</script>\
        <title>{}</title><style>\
        /* Hallmark · genre: atmospheric · macrostructure: Catalogue + Long Document · design-system: design.md · designed-as-app */\
        :root{{--color-paper:oklch(97% .008 270);--color-paper-2:oklch(94% .01 270);\
        --color-rule:oklch(87% .01 270);--color-neutral:oklch(55% .012 270);\
        --color-muted:oklch(44% .011 270);--color-ink:oklch(21% .012 270);\
        --color-accent:oklch(55% .2 260);--color-focus:oklch(55% .2 260);\
        --color-ok:oklch(60% .16 150);--color-bad:oklch(55% .2 25);\
        --font-display:\"Geist Sans\",ui-sans-serif,system-ui,sans-serif;\
        --font-body:\"Geist Sans\",ui-sans-serif,system-ui,sans-serif;\
        --font-mono:\"JetBrains Mono\",ui-monospace,monospace;\
        --space-3xs:.125rem;--space-2xs:.25rem;--space-xs:.5rem;--space-sm:.75rem;\
        --space-md:1rem;--space-lg:1.5rem;--space-xl:2.5rem;--space-2xl:4rem;--space-3xl:6rem;\
        --text-2xs:.7rem;--text-xs:.8rem;--text-sm:.875rem;--text-base:1rem;--text-md:1.25rem;\
        --text-lg:1.5625rem;--text-xl:1.9531rem;\
        --text-display:clamp(3rem,6vw + 1rem,6rem);\
        --ease-out:cubic-bezier(.16,1,.3,1);--ease-in:cubic-bezier(.7,0,.84,0);\
        --ease-in-out:cubic-bezier(.65,0,.35,1);\
        --dur-micro:120ms;--dur-short:220ms;--dur-long:420ms;\
        --radius-s:6px;--radius-m:12px;--radius-pill:999px;--z-base:1;--z-sticky:200;\
        --shadow:0 8px 32px oklch(20% .02 270 / .12);}}\
        [data-theme=\"dark\"]{{--color-paper:oklch(14% .012 270);--color-paper-2:oklch(18% .014 270);\
        --color-rule:oklch(30% .012 270);--color-neutral:oklch(62% .01 270);\
        --color-muted:oklch(74% .008 270);--color-ink:oklch(93% .008 270);\
        --color-accent:oklch(70% .16 250);--color-focus:oklch(70% .16 250);\
        --color-ok:oklch(72% .14 150);--color-bad:oklch(70% .17 25);\
        --shadow:0 10px 40px oklch(0% 0 0 / .45);}}\
        html{{overflow-x:clip;scroll-padding-top:5rem}}\
        body{{overflow-x:clip;background:var(--color-paper);color:var(--color-ink);\
        font-family:var(--font-body);font-size:var(--text-base);line-height:1.6;\
        margin:0}}\
        .skip{{position:absolute;left:1rem;top:-4rem;z-index:calc(var(--z-sticky) + 10);\
        background:var(--color-ink);color:var(--color-paper);padding:var(--space-xs) var(--space-md);\
        border-radius:var(--radius-m);text-decoration:none;transition:top var(--dur-micro) var(--ease-out)}}\
        .skip:focus-visible{{top:1rem;color:var(--color-paper)}}\
        .sr-only{{position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;\
        clip:rect(0 0 0 0);white-space:nowrap;border:0}}\
        body::before{{content:\"\";position:fixed;inset:0;z-index:0;pointer-events:none;\
        background:radial-gradient(38rem 30rem at 12% -6%,oklch(70% .12 190 / .14),transparent 70%),\
        radial-gradient(42rem 32rem at 88% 2%,oklch(66% .13 300 / .12),transparent 70%);}}\
        [data-theme=\"dark\"] body::before{{background:radial-gradient(40rem 32rem at 10% -8%,oklch(70% .16 190 / .22),transparent 70%),\
        radial-gradient(44rem 34rem at 90% 0%,oklch(65% .17 305 / .2),transparent 70%);}}\
        .wrap{{position:relative;z-index:1;max-width:56rem;margin:0 auto;padding-inline:clamp(1rem,4vw,1.5rem);\
        padding-block:var(--space-lg) var(--space-2xl)}}\
        a{{color:var(--color-accent);text-underline-offset:2px}}\
        a:hover{{color:var(--color-ink)}}\
        :focus-visible{{outline:2px solid var(--color-focus);outline-offset:2px}}\
        .tnum{{font-variant-numeric:tabular-nums}}\
        .nav-edge{{position:sticky;top:var(--space-md);z-index:var(--z-sticky);\
        display:flex;justify-content:space-between;align-items:center;gap:var(--space-md);\
        max-width:44rem;margin-inline:auto;padding:var(--space-sm) var(--space-md);\
        border:1px solid var(--color-rule);border-radius:var(--radius-pill);\
        background:color-mix(in oklch,var(--color-paper) 78%,transparent);\
        backdrop-filter:blur(12px) saturate(1.3);box-shadow:var(--shadow)}}\
        .brand{{display:flex;gap:var(--space-sm);align-items:center;min-width:0}}\
        .brand img{{width:32px;height:32px;flex:none}}\
        .wordmark{{font-family:var(--font-display);font-weight:600;font-size:var(--text-md);\
        letter-spacing:-.03em;color:var(--color-ink);text-decoration:none;white-space:nowrap;\
        overflow-wrap:anywhere;min-width:0}}\
        .pill{{font-family:var(--font-body);font-size:var(--text-xs);white-space:nowrap;\
        display:inline-flex;align-items:center;min-height:2rem;height:2rem;box-sizing:border-box;\
        border:1px solid var(--color-rule);border-radius:999px;padding:var(--space-2xs) var(--space-sm);\
        color:var(--color-muted);background:var(--color-paper-2)}}\
        .pill[data-up=\"1\"]{{color:var(--color-ink);border-color:var(--color-ok)}}\
        .pill[data-up=\"0\"]{{color:var(--color-ink);border-color:var(--color-bad)}}\
        .pill[data-up=\"2\"]{{color:var(--color-muted);border-style:dashed}}\
        @media (pointer:coarse){{\
        .pill,.theme-toggle,.tz-toggle,.range-menu summary,.range-pop a,.links a,.det{{min-height:44px}}}}\
        .theme-toggle{{font-family:var(--font-body);font-size:var(--text-sm);white-space:nowrap;\
        display:inline-flex;align-items:center;justify-content:center;min-width:2.75rem;min-height:2rem;height:2rem;box-sizing:border-box;\
        border:1px solid var(--color-rule);border-radius:999px;padding:var(--space-2xs) var(--space-sm);\
        color:var(--color-ink);background:transparent;cursor:pointer}}\
        .theme-toggle:hover{{color:var(--color-ink);border-color:var(--color-neutral)}}\
        .theme-toggle:active{{color:var(--color-ink)}}\
        .tz-toggle{{font-family:var(--font-mono);font-weight:600;font-size:var(--text-xs);\
        letter-spacing:.08em;white-space:nowrap;font-variant-numeric:tabular-nums;\
        display:inline-flex;align-items:center;justify-content:center;min-width:3.5rem;min-height:2rem;height:2rem;box-sizing:border-box;\
        border:1px solid var(--color-rule);border-radius:999px;padding:var(--space-2xs) var(--space-sm);\
        color:var(--color-muted);background:transparent;cursor:pointer}}\
        .tz-toggle:hover{{color:var(--color-ink);border-color:var(--color-neutral)}}\
        .tz-toggle[aria-pressed=\"true\"]{{color:var(--color-ink);border-color:var(--color-accent)}}\
        .range-menu{{position:relative;flex:none}}\
        .range-menu summary{{list-style:none;cursor:pointer;font-family:var(--font-mono);\
        font-weight:600;font-size:var(--text-xs);letter-spacing:.08em;white-space:nowrap;\
        font-variant-numeric:tabular-nums;display:inline-flex;align-items:center;gap:var(--space-2xs);\
        min-width:3.5rem;min-height:2rem;height:2rem;box-sizing:border-box;border:1px solid var(--color-rule);\
        border-radius:999px;padding:var(--space-2xs) var(--space-sm);\
        color:var(--color-muted);background:transparent}}\
        .range-menu summary::-webkit-details-marker{{display:none}}\
        .range-menu summary::after{{content:\"\";border:solid var(--color-muted);\
        border-width:0 1.5px 1.5px 0;padding:2.5px;transform:rotate(45deg) translateY(-1px)}}\
        .range-menu summary:hover{{color:var(--color-ink);border-color:var(--color-neutral)}}\
        .range-menu summary:hover::after{{border-color:var(--color-ink)}}\
        .range-menu[open] summary{{color:var(--color-ink);border-color:var(--color-accent)}}\
        .range-pop{{position:absolute;right:0;top:calc(100% + var(--space-2xs));\
        display:flex;flex-direction:column;gap:2px;min-width:7rem;z-index:var(--z-sticky);\
        background:var(--color-paper-2);border:1px solid var(--color-rule);\
        border-radius:var(--radius-m);padding:var(--space-2xs);box-shadow:var(--shadow)}}\
        .range-pop a{{font-family:var(--font-mono);font-weight:600;font-size:var(--text-xs);\
        letter-spacing:.08em;white-space:nowrap;text-decoration:none;display:flex;\
        align-items:center;min-height:2rem;border-radius:var(--radius-s);\
        padding:var(--space-3xs) var(--space-sm);color:var(--color-muted)}}\
        .range-pop a:hover{{color:var(--color-ink);background:color-mix(in oklch,var(--color-paper) 60%,transparent)}}\
        .range-pop a[aria-current]{{color:var(--color-ink)}}\
        .theme-toggle svg{{width:16px;height:16px;display:block;color:var(--color-ink);stroke:var(--color-ink);fill:none}}.theme-toggle svg *{{stroke:inherit;fill:inherit}}\
        .theme-toggle .icon-sun{{display:none}}\
        [data-theme=\"dark\"] .theme-toggle .icon-sun{{display:block}}\
        [data-theme=\"dark\"] .theme-toggle .icon-moon{{display:none}}\
        .head-right{{display:flex;gap:var(--space-sm);align-items:center;flex:none}}\
        .links{{display:flex;flex-wrap:wrap;justify-content:center;gap:var(--space-sm);\
        margin:var(--space-md) 0 var(--space-xl);padding:0;border:0}}\
        .links a{{white-space:nowrap;font-size:var(--text-sm);color:var(--color-muted);\
        text-decoration:none;border:1px solid var(--color-rule);\
        border-radius:var(--radius-pill);padding:var(--space-3xs) var(--space-sm);\
        background:color-mix(in oklch,var(--color-paper-2) 72%,transparent);\
        transition:color var(--dur-short) var(--ease-out),border-color var(--dur-short) var(--ease-out)}}\
        .links a:hover{{color:var(--color-ink);border-color:var(--color-accent)}}\
        .links a:active{{color:var(--color-ink)}}\
        .links-label{{font-family:var(--font-mono);font-size:var(--text-xs);\
        text-transform:uppercase;letter-spacing:.08em;color:var(--color-neutral);\
        white-space:nowrap;align-self:center}}\
        .hero-stat{{padding-block:var(--space-md) var(--space-xl);text-align:center}}\
        .figure{{font-family:var(--font-display);font-weight:700;font-style:normal;\
        font-size:clamp(4rem,12vw,7rem);line-height:1;letter-spacing:-.03em;margin:0;\
        font-variant-numeric:tabular-nums;overflow-wrap:anywhere;min-width:0}}\
        .hero-stat[data-up=\"1\"] .figure{{color:var(--color-ink)}}\
        .hero-stat[data-up=\"0\"] .figure{{color:var(--color-bad)}}\
        .hero-stat[data-up=\"2\"] .figure{{color:var(--color-muted)}}\
        .qualifier{{font-size:var(--text-md);color:var(--color-muted);max-width:65ch;margin:var(--space-sm) auto 0}}\
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
        .r{{background:var(--color-paper-2);border:1px solid var(--color-rule);\
        border-radius:var(--radius-m);padding:var(--space-lg);margin-bottom:var(--space-xl)}}\
        .r-head{{display:grid;grid-template-columns:minmax(0,1fr);gap:var(--space-2xs) var(--space-md);\
        align-items:baseline}}\
        @media (min-width:40rem){{\
        .r-head{{grid-template-columns:12px minmax(0,1.4fr) minmax(0,1fr);align-items:center}}}}\
        .d{{width:10px;height:10px;border-radius:50%;background:var(--color-neutral)}}\
        .r[data-up=\"1\"] .d{{background:var(--color-ok)}}\
        .r[data-up=\"0\"] .d{{background:var(--color-bad)}}\
        .r b{{font-family:var(--font-display);font-weight:700;letter-spacing:-.01em;\
        overflow-wrap:anywhere;min-width:0}}\
        .r b a{{color:var(--color-ink);text-decoration:none}}\
        .r b a:hover{{color:var(--color-accent)}}\
        .r b .mlogo{{width:16px;height:16px;vertical-align:-3px;margin-right:var(--space-2xs);border-radius:3px}}\
        .detail-hero h2 .mlogo{{height:1em;width:auto;vertical-align:-0.12em;margin-right:var(--space-xs);border-radius:5px}}\
        .r i{{font-style:normal;font-family:var(--font-mono);font-size:var(--text-xs);\
        font-weight:600;letter-spacing:.08em}}\
        .r[data-up=\"1\"] i{{color:var(--color-ok)}}\
        .r[data-up=\"0\"] i{{color:var(--color-bad)}}\
        .r s{{color:var(--color-muted);text-decoration:none;font-size:var(--text-sm);\
        font-variant-numeric:tabular-nums;white-space:nowrap;overflow:hidden;\
        text-overflow:ellipsis;min-width:0}}\
        @media (min-width:40rem){{.r s{{text-align:right}}}}\
        .det{{font-family:var(--font-display);font-weight:700;font-size:var(--text-xs);\
        text-transform:uppercase;letter-spacing:.08em;\
        color:var(--color-ink);text-decoration:none;white-space:nowrap;\
        display:inline-flex;align-items:center;min-height:2rem;\
        border:1px solid var(--color-rule);border-radius:var(--radius-pill);\
        padding:var(--space-3xs) var(--space-sm);background:transparent;\
        transition:color var(--dur-short) var(--ease-out),border-color var(--dur-short) var(--ease-out)}}\
        .det:hover{{color:var(--color-ink);border-color:var(--color-accent)}}\
        .det:active{{color:var(--color-ink)}}\
        .r-meta{{display:flex;align-items:center;flex-wrap:nowrap;\
        min-width:0;justify-content:flex-start}}\
        @media (min-width:40rem){{.r-meta{{justify-content:flex-end}}}}\
        .r-meta s{{flex:1 1 auto;min-width:100%}}\
        .r-meta>i,.r-meta>.det{{flex:none;white-space:nowrap}}\
        .r-meta>i::before,.r-meta>i::after{{content:\"·\";color:var(--color-neutral);\
        margin-inline:var(--space-sm)}}\
        .bars{{display:flex;gap:var(--space-3xs);margin:var(--space-sm) 0 var(--space-xs)}}\
        .bars i,.bars90 i{{position:relative;box-sizing:border-box}}\
        .bars i.nodata,.bars90 i.nodata{{background:var(--color-paper-2);\
        border:1px dashed var(--color-rule)}}\
        .bars i::after,.bars90 i::after{{content:attr(data-tip);position:absolute;\
        bottom:calc(100% + var(--space-xs));left:50%;transform:translateX(-50%);\
        font-family:var(--font-body);font-size:var(--text-sm);font-weight:600;line-height:1.4;\
        color:var(--color-ink);background:var(--color-paper-2);\
        border:1px solid var(--color-rule);border-radius:var(--radius-m);\
        padding:var(--space-xs) var(--space-sm);white-space:nowrap;\
        opacity:0;pointer-events:none;z-index:var(--z-sticky);\
        transition:opacity var(--dur-micro) var(--ease-out)}}\
        .bars i:first-child::after,.bars90 i:first-child::after{{left:0;transform:none}}\
        .bars i:last-child::after,.bars90 i:last-child::after{{left:auto;right:0;transform:none}}\
        .bars i:hover::after,.bars i:active::after,.bars i:focus-visible::after,\
        .bars90 i:hover::after,.bars90 i:active::after,.bars90 i:focus-visible::after{{opacity:1}}\
        .bars:focus-visible,.bars90:focus-visible{{outline:2px solid var(--color-focus);outline-offset:4px}}\
        .bars i{{flex:1 1 0;min-width:3px;height:28px;border-radius:2px;display:block}}\
        .bars90{{display:flex;flex-wrap:wrap;gap:var(--space-3xs);margin:var(--space-sm) 0}}\
        .bars90 i{{flex:1 1 0;min-width:4px;height:34px;border-radius:2px;display:block;\
        font-size:0;line-height:0}}\
        .lat{{color:var(--color-muted);font-size:var(--text-xs);\
        font-variant-numeric:tabular-nums;margin:0}}\
        .lat s{{text-decoration:none}}\
        .spark .line,.chart .line{{fill:none;stroke:var(--color-accent);stroke-width:1.5}}\
        .spark circle.hit,.chart circle.hit{{fill:transparent;pointer-events:all;cursor:crosshair}}\
        .spark circle.dot,.chart circle.dot{{fill:var(--color-accent);pointer-events:none}}\
        .chart{{width:100%;height:auto;display:block;margin-top:var(--space-sm)}}\
        .chart text{{font-family:var(--font-body);font-size:var(--text-2xs);fill:var(--color-muted)}}\
        .chart .grid{{stroke:var(--color-rule);stroke-width:1}}\
        .chart .axis{{stroke:var(--color-neutral);stroke-width:1}}\
        .svg-tip{{position:fixed;left:0;top:0;z-index:var(--z-sticky);display:none;\
        font-family:var(--font-body);font-size:var(--text-sm);font-weight:600;line-height:1.4;\
        color:var(--color-ink);background:var(--color-paper-2);\
        border:1px solid var(--color-rule);border-radius:var(--radius-m);\
        padding:var(--space-xs) var(--space-sm);white-space:nowrap;pointer-events:none}}\
        .svg-tip.on{{display:block}}\
        .lat s{{text-decoration:none;white-space:nowrap}}\
        .r,.lat{{transition:transform var(--dur-short) var(--ease-out),box-shadow var(--dur-short) var(--ease-out)}}\
        @media (hover:hover) and (pointer:fine){{\
        .r:hover{{transform:translateY(-1px);box-shadow:var(--shadow)}}}}\
        .r,.chart{{scroll-margin-top:6rem}}\
        main{{animation:rise var(--dur-short) var(--ease-out)}}\
        @keyframes rise{{from{{opacity:0}}to{{opacity:1}}}}\
        @view-transition{{navigation:auto}}\
        :active-view-transition-type(forward)::view-transition-old(root){{animation:vt-out-left var(--dur-short) var(--ease-out)}}\
        :active-view-transition-type(forward)::view-transition-new(root){{animation:vt-in-right var(--dur-short) var(--ease-out)}}\
        :active-view-transition-type(back)::view-transition-old(root){{animation:vt-out-right var(--dur-short) var(--ease-out)}}\
        :active-view-transition-type(back)::view-transition-new(root){{animation:vt-in-left var(--dur-short) var(--ease-out)}}\
        :active-view-transition-type(fade)::view-transition-old(root),\
        :active-view-transition-type(fade)::view-transition-new(root){{animation:vt-fade var(--dur-short) var(--ease-out)}}\
        @keyframes vt-in-right{{from{{transform:translateX(2rem);opacity:0}}to{{transform:none;opacity:1}}}}\
        @keyframes vt-out-left{{from{{transform:none;opacity:1}}to{{transform:translateX(-2rem);opacity:0}}}}\
        @keyframes vt-in-left{{from{{transform:translateX(-2rem);opacity:0}}to{{transform:none;opacity:1}}}}\
        @keyframes vt-out-right{{from{{transform:none;opacity:1}}to{{transform:translateX(2rem);opacity:0}}}}\
        @keyframes vt-fade{{from{{opacity:0}}to{{opacity:1}}}}\
        .note{{padding:var(--space-sm) var(--space-md);border-radius:var(--radius-m);\
        margin:var(--space-md) 0;border:1px solid var(--color-rule);\
        background:var(--color-paper-2);font-size:var(--text-sm)}}\
        .empty{{color:var(--color-muted);font-size:var(--text-sm)}}\
        .active{{border-color:var(--color-bad)}}\
        .upcoming{{border-color:var(--color-rule)}}\
        .detail-hero{{padding-block:var(--space-lg) 0}}\
        .crumb{{margin:var(--space-lg) 0 var(--space-md)}}\
        .crumb a{{font-family:var(--font-mono);font-size:var(--text-xs);\
        text-transform:uppercase;letter-spacing:.08em;color:var(--color-muted);\
        text-decoration:none;white-space:nowrap}}\
        .crumb a:hover{{color:var(--color-accent)}}\
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
        table.inc th{{font-family:var(--font-mono);font-weight:600;font-size:var(--text-xs);text-transform:uppercase;\
        letter-spacing:.08em;color:var(--color-muted);text-align:left;\
        border-bottom:1px solid var(--color-rule);padding:var(--space-xs) var(--space-xs) var(--space-xs) 0}}\
        table.inc td{{border-bottom:1px solid var(--color-rule);padding:var(--space-xs) var(--space-xs) var(--space-xs) 0;\
        vertical-align:top;overflow-wrap:anywhere}}\
        table.inc td .spark{{max-width:100%;height:auto;display:block}}\
        @media (max-width:40rem){{\
        table.inc thead{{position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0}}\
        table.inc tr{{display:block;border-top:1px solid var(--color-rule);padding-block:var(--space-xs)}}\
        table.inc td{{display:block;border:0;padding:var(--space-3xs) 0}}\
        table.inc td::before{{content:attr(data-label);display:block;font-family:var(--font-mono);font-weight:600;\
        font-size:var(--text-xs);text-transform:uppercase;letter-spacing:.08em;color:var(--color-muted)}}}}\
        .foot-mast{{border-top:1px solid var(--color-rule);margin-top:var(--space-2xl);\
        padding-top:var(--space-lg);display:flex;flex-wrap:wrap;align-items:baseline;\
        gap:var(--space-2xs) var(--space-md)}}\
        .foot-mast .wordmark{{font-family:var(--font-display);font-weight:600;font-size:var(--text-lg);\
        letter-spacing:-.03em;color:var(--color-ink);white-space:nowrap}}\
        .foot-mast .meta{{margin-left:auto;font-size:var(--text-sm);color:var(--color-muted)}}\
        a:active{{color:var(--color-ink)}}\
        @media (prefers-reduced-motion:reduce){{\
        @view-transition{{navigation:none}}\
        *,*::before,*::after{{animation-duration:150ms !important;transition-duration:150ms !important}}\
        .r:hover{{transform:none}}}}\
        </style>{spec}</head><body><a class=skip href=\"#main\">Skip to status</a><div class=wrap>\
        <header class=nav-edge><a class=wordmark href=\"{home}\" aria-label=\"Vigil home\"><span class=brand><img src=\"/logo.svg\" alt=\"\" width=32 height=32>{}</span></a><span class=head-right><span class=pill data-up=\"{pill}\" title=\"{uptip}\" role=status>{uptotal}</span><button class=tz-toggle id=tz-toggle type=button aria-label=\"Toggle time zone\" aria-pressed=\"false\" title=\"Times shown in UTC\">UTC</button>{menu}<button class=theme-toggle id=theme-toggle type=button aria-label=\"Toggle color theme\" aria-pressed=\"false\"><svg class=icon-moon viewBox=\"0 0 24 24\" stroke-width=2 stroke-linecap=round stroke-linejoin=round aria-hidden=true><path d=\"M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z\"/></svg><svg class=icon-sun viewBox=\"0 0 24 24\" stroke-width=2 stroke-linecap=round aria-hidden=true><circle cx=12 cy=12 r=\"4\" /><path d=\"M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4\"/></svg></button></span></header>\
        <nav class=links aria-label=\"Site\"><span class=links-label>Quick links:</span>{links}</nav>\
        <main id=main data-probed=\"{probed_at}\" tabindex=-1>{body}</main>\
        <footer class=foot-mast><span class=wordmark>{}</span><span class=meta>Checked {time}</span></footer>\
        </div><script>try{{window.addEventListener('pageswap',function(e){{try{{\
        var act=e.activation;if(!act||!e.viewTransition)return;\
        if(act.navigationType==='reload')return;\
        var to=new URL(act.entry.url).pathname;\
        var here=new URL(window.location.href).pathname;\
        var t=(to===here)?'fade':((to.indexOf('/monitor')===0)?'forward':((to==='/')?'back':null));\
        if(t)e.viewTransition.types.add(t);}}catch(err){{}}}});}}catch(err){{}}</script><script>(function(){{var el=document.querySelector('[data-count]');\
        if(el){{var raw=el.getAttribute('data-count');var target=parseInt(raw,10);\
        if(isFinite(target)&&!(window.matchMedia&&window.matchMedia('(prefers-reduced-motion: reduce)').matches)){{\
        var t0=null,dur=600;function step(t){{if(!t0)t0=t;var p=Math.min(1,(t-t0)/dur);\
        var e=1-Math.pow(1-p,3);el.textContent=String(Math.round(target*e));\
        if(p<1)requestAnimationFrame(step);}}requestAnimationFrame(step);}}}}\
        var tip=document.createElement('div');tip.className='svg-tip';tip.setAttribute('role','status');\
        document.body.appendChild(tip);var hideT=null;\
        function dotAt(t){{return t&&t.closest?t.closest('circle[data-tip]'):null;}}\
        function show(c,x,y){{tip.textContent=c.getAttribute('data-tip');tip.classList.add('on');\
        var w=tip.offsetWidth;var lx=x+14;if(lx+w>window.innerWidth-8)lx=x-w-14;\
        tip.style.left=lx+'px';tip.style.top=(y+18)+'px';}}\
        function hideSoon(ms){{if(hideT)clearTimeout(hideT);hideT=setTimeout(function(){{tip.classList.remove('on');}},ms);}}\
        document.addEventListener('mouseover',function(e){{var c=dotAt(e.target);\
        if(c){{if(hideT){{clearTimeout(hideT);hideT=null;}}show(c,e.clientX,e.clientY);}}}});\
        document.addEventListener('mousemove',function(e){{if(tip.classList.contains('on')){{var c=dotAt(e.target);\
        if(c)show(c,e.clientX,e.clientY);}}}});\
        document.addEventListener('mouseout',function(e){{if(dotAt(e.target))hideSoon(120);}});\
        document.addEventListener('click',function(e){{var c=dotAt(e.target);\
        if(c){{show(c,e.clientX,e.clientY);hideSoon(2500);}}}});\
        var tb=document.getElementById('theme-toggle');\
        function eff(){{var a=document.documentElement.getAttribute('data-theme');\
        if(a)return a;\
        return (window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches)?'dark':'light';}}\
        function paintBtn(){{if(tb)tb.setAttribute('aria-pressed',(eff()==='dark')?'true':'false');}}\
        if(tb){{paintBtn();tb.addEventListener('click',function(){{var n=(eff()==='dark')?'light':'dark';\
        document.documentElement.setAttribute('data-theme',n);\
        try{{localStorage.setItem('vigil-theme',n);}}catch(e){{}}paintBtn();}});}}\
        var tzMode='utc';try{{tzMode=localStorage.getItem('vigil-tz')||'utc';}}catch(e){{}}if(tzMode!=='local')tzMode='utc';\
        function tzName(){{try{{var o=Intl.DateTimeFormat().resolvedOptions();if(o&&o.timeZone)return o.timeZone;}}catch(e){{}}return 'local time';}}\
        function tzAbbr(){{try{{var ps=new Intl.DateTimeFormat('en-US',{{timeZoneName:'short'}}).formatToParts(new Date());for(var i=0;i<ps.length;i++){{if(ps[i].type==='timeZoneName')return ps[i].value;}}}}catch(e){{}}return '';}}\
        function p2(n){{return (n<10?'0':'')+n;}}\
        var MON=['January','February','March','April','May','June','July','August','September','October','November','December'];\
        function relTz(ts){{var s=Math.floor(Date.now()/1000)-ts;if(s<60)return 'just now';\
        var m=Math.floor(s/60);if(m<60)return m+(m===1?' minute ago':' minutes ago');\
        var h=Math.floor(m/60);if(h<24)return h+(h===1?' hour ago':' hours ago');\
        var d=Math.floor(h/24);return d+(d===1?' day ago':' days ago');}}\
        function fmtTz(ts,kind,local){{if(kind==='rel')return relTz(ts);\
        if(kind==='Rel'){{var r=relTz(ts);return r?r.charAt(0).toUpperCase()+r.slice(1):r;}}\
        var d=new Date(ts*1000);var Y,M,D,h,mi,s;\
        if(local){{Y=d.getFullYear();M=d.getMonth()+1;D=d.getDate();h=d.getHours();mi=d.getMinutes();s=d.getSeconds();}}\
        else{{Y=d.getUTCFullYear();M=d.getUTCMonth()+1;D=d.getUTCDate();h=d.getUTCHours();mi=d.getUTCMinutes();s=d.getUTCSeconds();}}\
        var date=Y+'-'+p2(M)+'-'+p2(D);\
        if(kind==='hour')return p2(h)+':'+p2(mi);\
        if(kind==='day')return p2(D)+'-'+MON[M-1];\
        if(kind==='tip'){{var ab=local?tzAbbr():'';return p2(D)+'-'+MON[M-1]+' @ '+p2(h)+':'+p2(mi)+' '+(ab||(local?'local':'UTC'));}}\
        if(kind==='short')return p2(M)+'-'+p2(D)+' '+p2(h)+':'+p2(mi);\
        if(kind==='date')return date;\
        var ab=local?tzAbbr():'';return date+' '+p2(h)+':'+p2(mi)+':'+p2(s)+' '+(ab||(local?'local':'UTC'));}}\
        function tipTz(el,local){{var ts=parseInt(el.getAttribute('data-ts'),10)||0;\
        var t=fmtTz(ts,el.getAttribute('data-tk')||'full',local);\
        if(el.hasAttribute('data-v')){{var v=el.getAttribute('data-v');return t+': '+(v||'no data');}}\
        if(el.hasAttribute('data-ms')){{var ms=el.getAttribute('data-ms');var pl=el.getAttribute('data-place');\
        return pl?t+' · '+pl+' · '+ms+'ms':t+': '+ms+'ms';}}return t;}}\
        function applyTz(){{var local=tzMode==='local';\
        var texts=document.querySelectorAll('time[data-ts],text[data-ts]');\
        for(var i=0;i<texts.length;i++){{var n=texts[i];\
        n.textContent=fmtTz(parseInt(n.getAttribute('data-ts'),10)||0,n.getAttribute('data-tk')||'full',local);}}\
        var tips=document.querySelectorAll('[data-tip][data-ts]');\
        for(var j=0;j<tips.length;j++){{var c=tips[j];var v=tipTz(c,local);\
        c.setAttribute('data-tip',v);if(c.hasAttribute('title'))c.setAttribute('title',v);}}\
        var zb=document.getElementById('tz-toggle');\
        if(zb){{zb.textContent=local?(tzAbbr()||'Local'):'UTC';\
        zb.setAttribute('aria-pressed',local?'true':'false');\
        zb.title=local?'Times in local time ('+tzName()+'). Switch to UTC.':'Times in UTC. Switch to local time.';}}}}\
        var zb=document.getElementById('tz-toggle');\
        if(zb){{zb.addEventListener('click',function(){{tzMode=(tzMode==='local')?'utc':'local';\
        try{{localStorage.setItem('vigil-tz',tzMode);}}catch(e){{}}applyTz();}});}}\
        applyTz();\
        if(document.querySelector('time[data-tk=\"rel\"]')){{\
        setInterval(function(){{if(document.hidden)return;applyTz();}},30000);}}\
        var mainEl=document.querySelector('main[data-probed]');\
        if(mainEl&&{refresh_ms}>0){{var seen=parseInt(mainEl.getAttribute('data-probed'),10)||0;\
        setInterval(function(){{if(document.hidden)return;\
        fetch('/api/health',{{headers:{{'accept':'application/json'}}}})\
        .then(function(r){{return r.ok?r.json():null;}})\
        .then(function(j){{var nxt=(j&&j.dataAt!=null)?j.dataAt:((j&&j.at)||0);if(nxt>seen)location.reload();}})\
        .catch(function(){{}});}},{refresh_ms});}}}})();</script><script>var FB=[\"/favicon.ico\",\"/favicon.png\",\"/favicon.svg\",\"/icon.svg\",\"/icon.png\",\"/apple-touch-icon.png\"];function nextFavicon(el){{var i=parseInt(el.getAttribute('data-fb')||'0',10)+1;if(i<FB.length){{el.setAttribute('data-fb',String(i));el.src=el.getAttribute('data-origin')+FB[i];}}else{{el.remove();}}}}</script></body></html>",
        esc(page_title),
        esc(brand),
        esc(brand),
        home = home_url,
        time = time_html(at.max(0), "rel", &vigil_core::time_label(at.max(0))),
        spec = speculation,
        menu = range_menu
    )
}

#[cfg(test)]
mod tests {
    use super::{favicon_origin, links_html, one_row, parse_counts, shell, speculation_rule, time_html};
    use crate::range::{RANGE_24H, RANGE_7D};
    use crate::reads::SampleOut;
    use std::collections::BTreeMap;
    use vigil_core::config::{Monitor, Probe};

    #[test]
    fn counts_parse_and_reject_garbage() {
        assert_eq!(parse_counts("30/0"), (30, 0));
        assert_eq!(parse_counts("0/0"), (0, 0));
        assert_eq!(parse_counts("nope"), (0, 0));
        assert_eq!(parse_counts(""), (0, 0));
    }

    #[test]
    fn time_html_carries_timestamp_for_toggle() {
        assert_eq!(
            time_html(1_000, "full", "1970-01-01 00:16:40 UTC"),
            "<time data-ts=\"1000\" data-tk=\"full\">1970-01-01 00:16:40 UTC</time>"
        );
    }

    #[test]
    fn links_open_new_tab_without_opener() {
        let html = links_html(
            &indexmap::IndexMap::from([("Home".to_owned(), "https://example.com".to_owned())]),
        );
        assert!(html.contains("target=_blank"));
        assert!(html.contains("noopener noreferrer"));
        let evil = links_html(&indexmap::IndexMap::from([(
            "<x>".to_owned(),
            "https://example.com/?a=\"b\"".to_owned(),
        )]));
        assert!(!evil.contains("<x>"));
    }

#[test]
    fn links_keep_file_order() {        let html = links_html(
            &indexmap::IndexMap::from([
                ("Home".to_owned(), "https://www.troysk.com".to_owned()),
                ("Blog".to_owned(), "https://blog.troysk.com".to_owned()),
            ]),
        );
        assert!(html.find("Home").unwrap() < html.find("Blog").unwrap());
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
    fn row_latency_uses_uppercase_percentile_labels() {
        let m = http_monitor("https://example.com");
        let lat = [tried(9_900, 54, 1), tried(9_800, 60, 1)];
        let html = one_row(&m, RANGE_24H, 10_000, Some(&tried(9_900, 54, 1)), &[], None, &lat);
        assert!(html.contains("P50"), "labels read P50, not p50");
        assert!(html.contains("P95"), "labels read P95, not p95");
        assert!(!html.contains("p50"), "no lowercase percentile labels");
        assert!(!html.contains("p95"), "no lowercase percentile labels");
    }

    #[test]
    fn row_states_carry_dot_word_and_detail() {
        let m = http_monitor("https://example.com");
        let up = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&tried(9_900, 54, 1)),
            &[],
            None,
            &[],
        );
        assert!(up.contains("data-up=\"1\""));
        assert!(up.contains("<i>UP</i>"));
        assert!(up.contains("54ms"));
        assert!(up.contains("BOM"));
        assert!(up.contains("No samples yet"));
        // Range syncs across pages: the details link carries it.
        assert!(up.contains("/monitor?id=m&range=24h"));
        let down = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&tried(9_900, 0, 0)),
            &[],
            None,
            &[],
        );
        assert!(down.contains("data-up=\"0\""));
        assert!(down.contains("DOWN"));
        assert!(down.contains("Check failing"));
        let fresh = one_row(&m, RANGE_24H, 10_000, None, &[], None, &[]);
        assert!(fresh.contains("No data yet"));
    }

    #[test]
    fn row_paints_24_hour_cells() {
        let m = http_monitor("https://example.com");
        let html = one_row(&m, RANGE_24H, 200_000, None, &[], Some(1), &[]);
        assert_eq!(html.matches("<i ").count(), 24);
    }

    #[test]
    fn row_paints_daily_cells_for_week_range() {
        let m = http_monitor("https://example.com");
        let html = one_row(&m, RANGE_7D, 30 * 86_400, None, &[], Some(1), &[]);
        assert_eq!(html.matches("<i ").count(), 7);
        assert!(html.contains("class=bars90"));
        assert!(html.contains("Last 7 days"));
    }

    #[test]
    fn row_charts_latency_unless_quiet() {
        let mut m = http_monitor("https://example.com");
        let lat = vec![tried(9_900, 54, 1), tried(9_800, 60, 1)];
        let html = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&tried(9_900, 54, 1)),
            &[],
            None,
            &lat,
        );
        assert!(html.contains("<svg"));
        assert!(html.contains("class=lat"));
        m.quiet_chart = true;
        let quiet = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&tried(9_900, 54, 1)),
            &[],
            None,
            &lat,
        );
        assert!(!quiet.contains("class=lat"));
        assert!(!quiet.contains("<svg"));
    }

    #[test]
    fn row_escapes_monitor_name() {
        let mut m = http_monitor("https://example.com");
        m.name = "<img src=x>".into();
        let html = one_row(&m, RANGE_24H, 10_000, None, &[], None, &[]);
        assert!(!html.contains("<img src=x>"));
    }

    #[test]
    fn shell_loads_fonts_without_blocking_render() {
        // The JetBrains Mono stylesheet is render-blocking by default: every
        // first paint waits on Google Fonts (client-side P90). media=print
        // + onload swap keeps the font without blocking first paint.
        let html = shell("T", "B", "", "1/1", 1_000, 30_000, "", "", "", 1_000, "");
        assert!(html.contains("fonts.googleapis.com"));
        assert!(
            html.contains("media=print"),
            "font stylesheet must not render-block first paint"
        );
    }

    #[test]
    fn speculation_rule_prerenders_conservatively() {
        // Conservative eagerness: moderate prerendered detail pages on mere
        // hover, doubling D1 load for navigations that never happen (P90).
        let tag = speculation_rule("/monitor*");
        assert!(tag.starts_with("<script type=\"speculationrules\">"));
        let json: serde_json::Value =
            serde_json::from_str(tag.trim_start_matches("<script type=\"speculationrules\">").trim_end_matches("</script>"))
                .unwrap();
        assert_eq!(json["prerender"][0]["source"], "document");
        assert_eq!(json["prerender"][0]["where"]["href_matches"], "/monitor*");
        assert_eq!(json["prerender"][0]["eagerness"], "conservative");
    }

    fn http_monitor(url: &str) -> Monitor {
        Monitor {
            id: "m".into(),
            name: "M".into(),
            check: Probe::Http {
                url: url.into(),
                method: "GET".into(),
                headers: BTreeMap::new(),
                body: None,
            },
            expect: vec![],
            timeout_ms: 10_000,
            has: None,
            lacks: None,
            tip: None,
            href: None,
            quiet_chart: false,
            every_min: None,
        }
    }

    #[test]
    fn favicon_uses_probe_origin() {
        assert_eq!(
            favicon_origin(&http_monitor("https://example.com/some/page?x=1")).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(favicon_origin(&http_monitor("not a url")), None);
    }

    #[test]
    fn favicon_origin_upgrades_plain_http() {
        // Icons are decorative third-party loads: always fetch them over
        // TLS even when the probe itself targets plain HTTP.
        assert_eq!(
            favicon_origin(&http_monitor("http://example.com/status")).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn tcp_monitors_have_no_favicon() {
        let mut m = http_monitor("https://example.com");
        m.check = Probe::Tcp {
            host: "1.1.1.1".into(),
            port: 53,
        };
        assert_eq!(favicon_origin(&m), None);
    }

    #[test]
    fn favicon_markup_agrees_on_origin_and_fallback() {
        use super::favicon_img;
        let m = http_monitor("https://example.com/some/page");
        let img = favicon_img(&m, 16);
        // src and data-origin derive from the same escaped origin, and the
        // client-side fallback chain is wired.
        assert!(img.contains("src=\"https://example.com/favicon.ico\""));
        assert!(img.contains("data-origin=\"https://example.com\""));
        assert!(img.contains("onerror='nextFavicon(this)'"));
    }
}
