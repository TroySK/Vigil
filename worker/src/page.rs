//! Status page: grouped monitor rows, history bars, latency sparklines.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::auth::esc;
use crate::db::Db;
use crate::range::{bars_for, Range};
use crate::reads::{
    clock, daily_series_by_monitor, gaps_window_by_monitor, latest_by_monitor, series_by_monitor,
    SampleOut,
};
use vigil_core::config::{Monitor, Probe, Site};
use vigil_core::rollup::{latency_chart, percentile_pair};
use vigil_core::state::Gap;
use worker::{Response, Result};

/// Latency chart points per sparkline on the index page: each monitor's
/// selected-range window spreads at most this many dots (see
/// `series_by_monitor`), strided server-side so long ranges stay cheap.
pub const INDEX_CHART_POINTS: i64 = 24;

/// Long ranges read the daily rollup; short ranges keep raw samples.
fn uses_daily_rollup(range: Range) -> bool {
    range.uses_rollup()
}

/// Render the full status page.
pub async fn render(
    db: &Db,
    site: &Site,
    favicons: &HashMap<String, String>,
    range: Range,
) -> Result<Response> {
    // One batched `kv` read for the tick clock, counts, and probe clock
    // (was: three sequential round trips). The refresh poller reloads on
    // the probe clock, not the tick clock, so skip-only ticks cost a
    // cheap poll.
    let (at, counts, probed) = clock(db).await?;
    let counts = counts.unwrap_or_else(default_counts);
    let (up, down) = parse_counts(&counts);
    let hero = hero_html(site, up, down, at);
    let mut body = hero;
    body.push_str(&sections(db, site, favicons, at, None, range).await?);
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
        "<section class=\"hero-stat\" data-up=\"{state}\">\
        <p class=\"figure tnum\" data-count=\"{figure}\">{figure}</p>\
        <p class=\"qualifier\">{qualifier}</p>\
        <div class=\"stat-strip tnum\">\
        <div><b>{up}</b><span>up</span></div>\
        <div><b>{down}</b><span>down</span></div>\
        <div><b>{total}</b><span>monitors</span></div>\
        </div>\
        <p class=\"site-sub\">{sub}</p>\
        <p class=\"sr-only\" role=status aria-live=polite aria-atomic=true data-live></p></section>",
        sub = esc(&site.title),
    )
}

/// Render upkeep banners plus monitor sections (optionally one group only).
pub async fn sections(
    db: &Db,
    site: &Site,
    favicons: &HashMap<String, String>,
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
    // Four batched D1 round trips cover every monitor (was: four per
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
    // `quiet_chart` monitors skip the sparkline query entirely: their
    // series were fetched and then discarded.
    let chart_ids: Vec<&str> = site
        .monitors
        .iter()
        .filter(|monitor| !monitor.quiet_chart)
        .map(|monitor| monitor.id.as_str())
        .collect();
    // History-bar cutoffs come from the `first:` kv stamps the tick
    // maintains — no MIN(at) scan over the retention window (see
    // `effective_since`).
    let first_keys: Vec<String> = ids.iter().map(|id| format!("first:{id}")).collect();
    // 30d/90d sparklines read one rollup row per monitor-day; 24h/7d keep
    // raw samples (cheap at those spans and more detailed).
    let series_for_range = async {
        if uses_daily_rollup(range) {
            daily_series_by_monitor(db, &chart_ids, cutoff, INDEX_CHART_POINTS).await
        } else {
            series_by_monitor(db, &chart_ids, cutoff, INDEX_CHART_POINTS).await
        }
    };
    let (latest, gap_lists, series, firsts) = futures_util::try_join!(
        latest_by_monitor(db, &ids),
        gaps_window_by_monitor(db, &ids, cutoff, now),
        series_for_range,
        db.kv_map(&first_keys),
    )?;
    let since = |id: &str| {
        // A `first:` stamp with zero retained samples (removed-then-readded
        // id, or stamp older than pruned data) must not paint phantom 100%:
        // old code passed MIN(at) of retained samples (None when empty).
        let has_samples =
            latest.contains_key(id) || gap_lists.get(id).is_some_and(|g| !g.is_empty());
        if !has_samples {
            return None;
        }
        crate::reads::effective_since(
            firsts
                .get(&format!("first:{id}"))
                .and_then(|s| s.parse().ok()),
            now,
        )
    };
    // Rows assemble synchronously in file order; groups slice them below.
    // Favicon origins come from the parse-time map (a lookup miss falls
    // back to a live parse so config races stay correct).
    let rows: BTreeMap<String, String> = site
        .monitors
        .iter()
        .map(|monitor| {
            let live;
            let favicon = if let Some(origin) = favicons.get(monitor.id.as_str()) {
                Some(origin.as_str())
            } else {
                live = favicon_origin(monitor);
                live.as_deref()
            };
            (
                monitor.id.clone(),
                one_row(
                    monitor,
                    range,
                    at,
                    latest.get(&monitor.id),
                    gap_lists.get(&monitor.id).map_or(&[], Vec::as_slice),
                    since(&monitor.id),
                    series.get(&monitor.id).map_or(&[], Vec::as_slice),
                    favicon,
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
/// its chart samples, `since` the monitor's first sample (if any),
/// `favicon` the parse-time origin (`None` falls back to a live parse).
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_precision_loss
)]
fn one_row(
    monitor: &Monitor,
    range: Range,
    at: i64,
    current: Option<&SampleOut>,
    gaps: &[Gap],
    since: Option<i64>,
    lat: &[SampleOut],
    favicon: Option<&str>,
) -> String {
    // Status from gaps (open gap = down), not the hourly sample: split
    // cadence probes uptime every 5m but samples hourly, so the sample's
    // `up` flag can lag reality by ~1h. Latency text still rides the
    // latest sample (hourly-stale by design).
    let open = gaps.iter().any(|g| g.to.is_none());
    let is_up = !open && current.map_or(true, |r| r.up == 1);
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
            cells.push_str(&crate::range::bar_cell(
                bar,
                strip.kind,
                &label,
                "",
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
        // One reverse pass builds chart points; latencies derive from
        // them (was: two reverse passes plus a third copy inside the
        // percentile sort).
        let points: Vec<(i64, i64, &str)> = lat
            .iter()
            .rev()
            .map(|r| (r.at, r.ms, r.place.as_str()))
            .collect();
        let vals: Vec<i64> = points.iter().map(|&(_, ms, _)| ms).collect();
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
    let name = monitor_name(monitor, favicon);
    format!(
        "<article class=r data-up=\"{flag}\"{tip}>\
        <div class=r-head><span class=d aria-hidden=true></span>\
        <b>{name}</b><div class=r-meta><s>{detail}</s><i>{word}</i><a class=det href=\"/monitor?id={}&range={}\">details &rsaquo;</a></div></div>\
        {extra}</article>",
        encode_monitor_id(&monitor.id),
        range.param
    )
}

/// Percent-encode a monitor id for a query string (`detail_id` decodes via
/// `query_pairs`): unreserved chars pass through, everything else becomes
/// `%XX` so `"`/`&`/spaces cannot break the attribute or the link.
fn encode_monitor_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
        }
    }
    out
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
/// image when none exists. The origin arrives precomputed per render
/// (`None` falls back to a live parse).
pub(crate) fn favicon_img(origin: Option<&str>, size: u32) -> String {
    origin.map_or_else(String::new, |origin| {
        // `origin` is parser-constrained, but attribute context demands
        // escaping regardless of what today's parser guarantees.
        let src = esc(origin);
        format!(
            "<img class=mlogo src=\"{src}/favicon.ico\" data-origin=\"{src}\" data-fb=0 alt=\"\" width={size} height={size} loading=lazy onerror='nextFavicon(this)'>",
        )
    })
}

/// Monitor display name with favicon, linked when `href` is set (new tab,
/// no opener). Logo-first per favicon-before-title convention. The origin
/// arrives precomputed per render (`None` falls back to a live parse).
fn monitor_name(monitor: &Monitor, favicon: Option<&str>) -> String {
    let label = match &monitor.href {
        Some(href) => format!(
            "<a href=\"{}\" target=_blank rel=\"noopener noreferrer\">{}</a>",
            esc(href),
            esc(&monitor.name)
        ),
        None => esc(&monitor.name),
    };
    let live;
    let origin = if let Some(origin) = favicon {
        Some(origin)
    } else {
        live = favicon_origin(monitor);
        live.as_deref()
    };
    format!("{}{label}", favicon_img(origin, 16))
}

/// Footer links (`label -> url`) joined with middots. External links open
/// in a new tab without opener access.
#[must_use]
pub fn links_html(links: &indexmap::IndexMap<String, String>) -> String {
    let mut out = String::new();
    for (i, (label, url)) in links.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(
            out,
            "<a href=\"{}\" target=_blank rel=\"noopener noreferrer\">{}</a>",
            esc(url),
            esc(label)
        );
    }
    out
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
/// `pattern` on hover intent (`moderate` eagerness): Chromium turns likely
/// next-page hovers into instant navigations; other browsers ignore the
/// tag. Targets are display-only GETs, so a wasted prerender costs a few
/// cached reads — and prerendered pages stay hidden, where the pollers and
/// rel-time interval already stand down.
#[must_use]
pub(crate) fn speculation_rule(pattern: &str) -> String {
    format!(
        "<script type=\"speculationrules\">{{\"prerender\":[{{\"source\":\"document\",\"where\":{{\"href_matches\":\"{pattern}\"}},\"eagerness\":\"moderate\"}}]}}</script>"
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
        document.documentElement.setAttribute('data-theme',t);}})();\
        var FB=[\"/favicon.ico\",\"/favicon.png\",\"/favicon.svg\"];\
        function nextFavicon(el){{var i=parseInt(el.getAttribute('data-fb')||'0',10)+1;\
        if(i<FB.length){{el.setAttribute('data-fb',String(i));el.src=el.getAttribute('data-origin')+FB[i];}}else{{el.remove();}}}}</script>\
        <title>{}</title><link rel=stylesheet href=\"/style.css\">\
        <script>window.vigilRefresh={refresh_ms}</script><script src=\"/app.js\" defer></script>\
        {spec}</head><body><a class=skip href=\"#main\">Skip to status</a><div class=wrap>\
        <header class=nav-edge><a class=wordmark href=\"{home}\" aria-label=\"Vigil home\"><span class=brand><img src=\"/logo.svg\" alt=\"\" width=32 height=32>{}</span></a><span class=head-right><span class=pill data-up=\"{pill}\" title=\"{uptip}\" role=status>{uptotal}</span><button class=tz-toggle id=tz-toggle type=button aria-label=\"Toggle time zone\" aria-pressed=\"false\" title=\"Times shown in UTC\">UTC</button>{menu}<button class=theme-toggle id=theme-toggle type=button aria-label=\"Toggle color theme\" aria-pressed=\"false\"><svg class=icon-moon viewBox=\"0 0 24 24\" stroke-width=2 stroke-linecap=round stroke-linejoin=round aria-hidden=true><path d=\"M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z\"/></svg><svg class=icon-sun viewBox=\"0 0 24 24\" stroke-width=2 stroke-linecap=round aria-hidden=true><circle cx=12 cy=12 r=\"4\" /><path d=\"M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4\"/></svg></button></span></header>\
        <nav class=links aria-label=\"Site\"><span class=links-label>Quick links:</span>{links}</nav>\
        <main id=main data-probed=\"{probed_at}\" tabindex=-1>{body}</main>\
        <footer class=foot-mast><span class=wordmark>{}</span><span class=meta>Checked {time}</span></footer>\
        </div></body></html>",
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
    use super::{favicon_origin, hero_html, links_html, one_row, parse_counts, shell, speculation_rule, time_html, uses_daily_rollup};
    use crate::range::{RANGE_24H, RANGE_30D, RANGE_7D, RANGE_90D};
    use crate::reads::SampleOut;
    use std::collections::BTreeMap;
    use vigil_core::config::{Monitor, Probe};
    use vigil_core::state::Gap;

    #[test]
    fn hero_keeps_live_announcements_off_the_animated_figure() {
        let html = hero_html(&vigil_core::config::Site::default(), 2, 1, 1_000);
        assert_eq!(
            html.matches("aria-live").count(),
            1,
            "only the hidden status node may be live"
        );
        let hidden = html.find("class=\"sr-only\"").expect("hidden live node");
        assert!(html.contains("role=status"));
        assert!(html.contains("aria-live=polite"));
        assert!(
            !html[..hidden].contains("aria-live"),
            "the counting figure must not be live"
        );
        assert!(html.contains("data-live"));
        assert!(hidden > html.find("data-count").expect("figure"));
    }

    #[test]
    fn counts_parse_and_reject_garbage() {
        assert_eq!(parse_counts("30/0"), (30, 0));
        assert_eq!(parse_counts("0/0"), (0, 0));
        assert_eq!(parse_counts("nope"), (0, 0));
        assert_eq!(parse_counts(""), (0, 0));
    }

    #[test]
    fn long_ranges_read_daily_rollups() {
        // 24h/7d stay on raw samples; 30d/90d must not scan every sample
        // just to draw 24 dots.
        assert!(!uses_daily_rollup(RANGE_24H));
        assert!(!uses_daily_rollup(RANGE_7D));
        assert!(uses_daily_rollup(RANGE_30D));
        assert!(uses_daily_rollup(RANGE_90D));
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
        let html = one_row(&m, RANGE_24H, 10_000, Some(&tried(9_900, 54, 1)), &[], None, &lat, None);
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
            None,
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
            None,
        );
        assert!(down.contains("data-up=\"0\""));
        assert!(down.contains("DOWN"));
        assert!(down.contains("Check failing"));
        let fresh = one_row(&m, RANGE_24H, 10_000, None, &[], None, &[], None);
        assert!(fresh.contains("No data yet"));
    }

    #[test]
    fn row_status_comes_from_open_gap_not_stale_sample() {
        let m = http_monitor("https://example.com");
        let current = tried(9_900, 54, 1);
        let open = Gap {
            at: 9_500,
            to: None,
            why: "refused".into(),
        };
        // Split cadence: the newest sample can still say up for ~1h after
        // a monitor fails, but the open gap is written every 5 minutes.
        let down = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&current),
            &[open],
            None,
            &[],
            None,
        );
        assert!(down.contains("data-up=\"0\""));
        assert!(down.contains("<i>DOWN</i>"));
        assert!(down.contains("Check failing"));

        // A closed gap is history, not current status.
        let closed = Gap {
            at: 9_000,
            to: Some(9_500),
            why: "refused".into(),
        };
        let up = one_row(
            &m,
            RANGE_24H,
            10_000,
            Some(&current),
            &[closed],
            None,
            &[],
            None,
        );
        assert!(up.contains("data-up=\"1\""));
        assert!(up.contains("<i>UP</i>"));
        assert!(up.contains("54ms"));
    }

    #[test]
    fn row_sparkline_labels_follow_selected_range() {
        let m = http_monitor("https://example.com");
        let current = tried(2_000, 60, 1);
        let lat = [tried(1_000, 54, 1), current.clone()];
        let daily = one_row(&m, RANGE_24H, 10_000, Some(&current), &[], None, &lat, None);
        assert!(daily.contains("data-tk=\"hour\""));
        assert!(!daily.contains("data-tk=\"day\""));

        let weekly = one_row(&m, RANGE_7D, 10_000, Some(&current), &[], None, &lat, None);
        assert!(weekly.contains("data-tk=\"day\""));
        assert!(!weekly.contains("data-tk=\"hour\""));
    }

    #[test]
    fn row_paints_24_hour_cells() {
        let m = http_monitor("https://example.com");
        let html = one_row(&m, RANGE_24H, 200_000, None, &[], Some(1), &[], None);
        assert_eq!(html.matches("<i ").count(), 24);
    }

    #[test]
    fn row_paints_daily_cells_for_week_range() {
        let m = http_monitor("https://example.com");
        let html = one_row(&m, RANGE_7D, 30 * 86_400, None, &[], Some(1), &[], None);
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
            None,
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
            None,
        );
        assert!(!quiet.contains("class=lat"));
        assert!(!quiet.contains("<svg"));
    }

    #[test]
    fn row_escapes_monitor_name() {
        let mut m = http_monitor("https://example.com");
        m.name = "<img src=x>".into();
        let html = one_row(&m, RANGE_24H, 10_000, None, &[], None, &[], None);
        assert!(!html.contains("<img src=x>"));
    }

    #[test]
    fn detail_link_encodes_monitor_id() {
        use super::encode_monitor_id;
        assert_eq!(encode_monitor_id("m"), "m");
        assert_eq!(encode_monitor_id("a b&c"), "a%20b%26c");
        let mut m = http_monitor("https://example.com");
        m.id = "a\"b".into();
        let html = one_row(&m, RANGE_24H, 10_000, None, &[], None, &[], None);
        assert!(!html.contains("id=a\"b"), "raw quote must not break the attribute");
        assert!(html.contains("id=a%22b"), "id must be percent-encoded");
    }

    #[test]
    fn row_uses_precomputed_favicon_origin() {
        let m = http_monitor("https://example.com/some/page?x=1");
        let row = |favicon: Option<&str>| {
            one_row(&m, RANGE_24H, 10_000, None, &[], None, &[], favicon)
        };
        let precomputed = row(Some("https://example.com"));
        assert!(precomputed.contains("https://example.com/favicon.ico"));
        // Equivalence: a precomputed origin renders exactly like a live
        // parse (None falls back to parsing the monitor URL).
        assert_eq!(precomputed, row(favicon_origin(&m).as_deref()));
        assert_eq!(precomputed, row(None));
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
    fn shell_hoists_favicon_fallback_into_the_blocking_head() {
        // The inline onerror handler can fire before the deferred app.js
        // runs, so the fallback must be defined in the parser-blocking head.
        let html = shell("T", "B", "", "1/1", 1_000, 30_000, "", "", "", 1_000, "");
        let head_end = html.find("</head>").expect("head closes");
        let body = html.find("<body>").expect("body opens");
        let fallback = html.find("function nextFavicon").expect("fallback is inline");
        assert!(fallback < head_end, "fallback must run during head parse");
        assert!(head_end < body);
        assert!(html.contains("var FB=[\"/favicon.ico\",\"/favicon.png\",\"/favicon.svg\"];"));
    }

    #[test]
    fn shell_links_cacheable_assets_with_inline_refresh() {
        // App CSS/JS live in /style.css + /app.js (short revalidating cache,
        // unversioned URLs) instead of ~30KB inline per page; only the
        // per-page refresh interval stays inline for the shared poller.
        let html = shell("T", "B", "", "1/1", 1_000, 30_000, "", "", "", 1_000, "");
        assert!(html.contains("href=\"/style.css\""));
        assert!(html.contains("src=\"/app.js\""));
        assert!(html.contains("window.vigilRefresh=30000"));
        assert!(
            !html.contains("hero-stat{{"),
            "shell must not inline the extracted stylesheet"
        );
    }

    #[test]
    fn speculation_rule_prerenders_on_hover_intent() {
        // Moderate eagerness fires on hover (conservative waits for
        // mousedown/pointerdown, i.e. click start).
        let tag = speculation_rule("/monitor*");
        assert!(tag.starts_with("<script type=\"speculationrules\">"));
        let json: serde_json::Value =
            serde_json::from_str(tag.trim_start_matches("<script type=\"speculationrules\">").trim_end_matches("</script>"))
                .unwrap();
        assert_eq!(json["prerender"][0]["source"], "document");
        assert_eq!(json["prerender"][0]["where"]["href_matches"], "/monitor*");
        assert_eq!(json["prerender"][0]["eagerness"], "moderate");
    }

    #[test]
    fn shell_leaves_transitions_to_css() {
        // Slide in/out is pure CSS (`::view-transition-old/new(root)` in
        // style.css): no JS direction tagging. Types set in the old
        // document's `pageswap` never reach the new document, and
        // prerender activations leave `navigation.activation.from` empty —
        // either failure silently collapses the slide to a fade.
        let html = shell("T", "B", "", "1/1", 1_000, 30_000, "", "", "", 1_000, "");
        assert!(!html.contains("pageswap"), "no JS transition tagging in shell");
        assert!(!html.contains("pagereveal"), "no JS transition tagging in shell");
        assert!(!html.contains("vtDir"), "no JS transition tagging in shell");
        assert!(!html.contains("'fade'"), "no fade type either");
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
            uptime_every_min: None,
            sample_every_min: None,
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
        use super::{favicon_img, favicon_origin};
        let m = http_monitor("https://example.com/some/page");
        let img = favicon_img(favicon_origin(&m).as_deref(), 16);
        // src and data-origin derive from the same escaped origin, and the
        // client-side fallback chain is wired.
        assert!(img.contains("src=\"https://example.com/favicon.ico\""));
        assert!(img.contains("data-origin=\"https://example.com\""));
        assert!(img.contains("onerror='nextFavicon(this)'"));
        assert!(favicon_img(None, 16).is_empty());
    }
}
