//! Rollups for history bars, latency percentiles, and sparklines.

use crate::state::Gap;
use std::fmt::Write as _;

/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;
/// Uptime thresholds for green / amber / red history cells.
pub const GREEN_PCT: f64 = 99.9;
/// Uptime thresholds for green / amber / red history cells.
pub const AMBER_PCT: f64 = 95.0;

/// Seconds in an hour.
pub const SECS_PER_HOUR: i64 = 3_600;

/// One calendar day cell for history bars: percent of day up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayBar {
    /// Midnight UTC starting this cell (unix seconds).
    pub day: i64,
    /// Uptime percent in `[0.0, 100.0]`, rounded to one decimal.
    /// Meaningless when [`DayBar::has_data`] is false.
    pub pct: f64,
    /// Whether any probe data covers this day. Days before the first
    /// recorded sample report `false` so callers render "no data"
    /// instead of a misleading 100%.
    pub has_data: bool,
}

/// Build `days` daily cells ending today from gap records.
///
/// `since` is the earliest recorded sample time (unix seconds), or `None`
/// when the monitor has no samples at all. Days ending before `since`
/// are marked [`DayBar::has_data`] `= false`; time not covered by a
/// recorded gap still counts as up, but only within days that have data.
#[must_use]
pub fn month_bars(gaps: &[Gap], now: i64, days: usize, since: Option<i64>) -> Vec<DayBar> {
    let today = now - now.rem_euclid(SECS_PER_DAY);
    let mut out = Vec::with_capacity(days);
    for back in (0..days).rev() {
        #[allow(clippy::cast_possible_wrap)]
        let start = today - (back as i64).saturating_mul(SECS_PER_DAY);
        let end = start.saturating_add(SECS_PER_DAY);
        let has_data = since.is_some_and(|s| end > s);
        let mut down = 0_i64;
        for g in gaps {
            let s = g.at.max(start);
            let e = g.to.unwrap_or(now).min(end);
            if e > s {
                down = down.saturating_add(e - s);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let up = (SECS_PER_DAY - down).max(0) as f64 / SECS_PER_DAY as f64 * 100.0;
        out.push(DayBar {
            day: start,
            pct: (up * 10.0).round() / 10.0,
            has_data,
        });
    }
    out
}

/// Build `hours` hourly cells ending now from gap records.
///
/// Same contract as [`month_bars`], but each cell covers one hour — used
/// for the last-24-hours strip where daily cells would hide intraday
/// outages. `since` marks the earliest recorded sample; earlier cells
/// report [`DayBar::has_data`] `= false`.
#[must_use]
pub fn hour_bars(gaps: &[Gap], now: i64, hours: usize, since: Option<i64>) -> Vec<DayBar> {
    let mut out = Vec::with_capacity(hours);
    for back in (0..hours).rev() {
        #[allow(clippy::cast_possible_wrap)]
        let start = now.saturating_sub((back as i64).saturating_mul(SECS_PER_HOUR));
        let end = start.saturating_add(SECS_PER_HOUR);
        let has_data = since.is_some_and(|s| end > s);
        let mut down = 0_i64;
        for g in gaps {
            let s = g.at.max(start);
            let e = g.to.unwrap_or(now).min(end);
            if e > s {
                down = down.saturating_add(e - s);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let up = (SECS_PER_HOUR - down).max(0) as f64 / SECS_PER_HOUR as f64 * 100.0;
        out.push(DayBar {
            day: start,
            pct: (up * 10.0).round() / 10.0,
            has_data,
        });
    }
    out
}

/// Cell color for a bar: green / amber / red thresholds.
#[must_use]
pub fn bar_color(pct: f64) -> &'static str {
    if pct >= GREEN_PCT {
        "#2da44e"
    } else if pct >= AMBER_PCT {
        "#bf8700"
    } else {
        "#cf222e"
    }
}

/// Nearest-rank percentile over a slice. Returns 0 when empty.
///
/// `p` is clamped to `[0.0, 100.0]`; the input is copied and sorted so
/// callers keep ownership of their samples.
///
/// Precision-note: sample counts and millisecond values are far below
/// 2⁵³, so the `as f64` conversions below are exact in practice.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile(values: &[i64], p: f64) -> i64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (p.clamp(0.0, 100.0) / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Dependency-free SVG sparkline with per-point time tooltips
/// (oldest → newest). Each sample gets an invisible hover target carrying a
/// `data-tip` attribute (`{timestamp}: {ms}ms`); a small page script turns
/// those into styled bubbles, since native SVG `<title>` tooltips cannot be
/// sized. Empty input yields an empty string.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn sparkline_points(points: &[(i64, i64)], w: u32, h: u32) -> String {
    if points.is_empty() {
        return String::new();
    }
    let max = points
        .iter()
        .map(|&(_, ms)| ms)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let step = if points.len() < 2 {
        0.0
    } else {
        f64::from(w) / (points.len() as f64 - 1.0)
    };
    let mut pts = String::with_capacity(points.len() * 12);
    let mut dots = String::with_capacity(points.len() * 64);
    for (i, &(at, ms)) in points.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (ms.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        // `write!` on a `String` is infallible; ignore the result.
        let _ = write!(pts, "{x},{y} ");
        let _ = write!(
            dots,
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"7\" aria-hidden=true data-ts=\"{at}\" data-tk=\"full\" data-ms=\"{ms}\" data-tip=\"{}: {ms}ms\"/>",
            crate::time_label(at),
        );
    }
    format!(
        "<svg class=\"spark\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Latency sparkline\">\
<title>Latency sparkline</title>\
<polyline points=\"{}\" class=\"line\"/>{dots}</svg>",
        pts.trim_end()
    )
}

/// Latency chart with labeled X (probe time) and Y (milliseconds) axes.
///
/// Points are `(sample time, milliseconds, origin colo)`; each sample gets
/// an invisible hover target whose tooltip names all three, since probes in
/// one chart can come from different colos. Empty input yields an empty
/// string.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn latency_chart(points: &[(i64, i64, &str)], w: u32, h: u32) -> String {
    if points.is_empty() {
        return String::new();
    }
    let pad_l = 46_u32;
    let pad_b = 22_u32;
    let pad_t = 8_u32;
    let pad_r = 8_u32;
    let plot_w = w.saturating_sub(pad_l + pad_r).max(10);
    let plot_h = h.saturating_sub(pad_t + pad_b).max(10);
    let max_ms = points.iter().map(|&(_, ms, _)| ms).max().unwrap_or(0).max(1);
    let max = max_ms as f64;
    let step = if points.len() < 2 {
        0.0
    } else {
        f64::from(plot_w) / (points.len() as f64 - 1.0)
    };
    let x = |i: usize| pad_l + (i as f64 * step).round() as u32;
    let y = |ms: i64| {
        let scaled = (ms.max(0) as f64 / max) * f64::from(plot_h);
        pad_t + plot_h.saturating_sub(scaled.round() as u32)
    };
    let mut pts = String::with_capacity(points.len() * 12);
    let mut dots = String::with_capacity(points.len() * 96);
    for (i, &(at, ms, place)) in points.iter().enumerate() {
        let _ = write!(pts, "{},{} ", x(i), y(ms));
        let origin = crate::views::escape_html(place);
        let _ = write!(
            dots,
            "<circle cx=\"{}\" cy=\"{}\" r=\"7\" aria-hidden=true data-ts=\"{at}\" data-tk=\"full\" data-place=\"{origin}\" data-ms=\"{ms}\" data-tip=\"{} &middot; {origin} &middot; {ms}ms\"/>",
            x(i),
            y(ms),
            crate::time_label(at),
        );
    }
    let mid = max_ms / 2;
    let base = pad_t + plot_h;
    let mid_i = points.len() / 2;
    format!(
        "<svg class=\"chart\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Latency chart\">\
<title>Latency over time</title>\
<line class=\"grid\" x1=\"{pad_l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>\
<line class=\"grid\" x1=\"{pad_l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>\
<line class=\"axis\" x1=\"{pad_l}\" y1=\"{base}\" x2=\"{}\" y2=\"{base}\"/>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{max_ms}ms</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{mid}ms</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">0</text>\
<text x=\"{pad_l}\" y=\"{}\" text-anchor=\"start\" data-ts=\"{}\" data-tk=\"short\">{}</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" data-ts=\"{}\" data-tk=\"short\">{}</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\" data-ts=\"{}\" data-tk=\"short\">{}</text>\
<polyline points=\"{}\" class=\"line\"/>{dots}</svg>",
        y(max_ms),
        pad_l + plot_w,
        y(max_ms),
        y(mid),
        pad_l + plot_w,
        y(mid),
        pad_l + plot_w,
        pad_l - 6,
        y(max_ms) + 4,
        pad_l - 6,
        y(mid) + 4,
        pad_l - 6,
        base + 4,
        h - 6,
        points[0].0,
        crate::short_label(points[0].0),
        pad_l + plot_w / 2,
        h - 6,
        points[mid_i].0,
        crate::short_label(points[mid_i].0),
        pad_l + plot_w,
        h - 6,
        points[points.len() - 1].0,
        crate::short_label(points[points.len() - 1].0),
        pts.trim_end()
    )
}

/// `vals` are milliseconds; empty input yields an empty-string placeholder.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn sparkline(vals: &[i64], w: u32, h: u32) -> String {
    if vals.is_empty() {
        return String::new();
    }
    let max = vals.iter().max().copied().unwrap_or(0).max(1) as f64;
    let step = if vals.len() < 2 {
        0.0
    } else {
        f64::from(w) / (vals.len() as f64 - 1.0)
    };
    let mut pts = String::with_capacity(vals.len() * 12);
    for (i, &v) in vals.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (v.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        // `write!` on a `String` is infallible; ignore the result.
        let _ = write!(pts, "{x},{y} ");
    }
    format!(
        "<svg width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-hidden=\"true\">\
<polyline points=\"{}\" fill=\"none\" stroke=\"#0969da\" stroke-width=\"1.5\"/></svg>",
        pts.trim_end()
    )
}
