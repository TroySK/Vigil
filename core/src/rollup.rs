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

/// Smooth SVG path through pixel points: Catmull-Rom splines converted to
/// cubic Béziers, so latency lines read as trends instead of zigzags.
/// Empty input yields an empty string; a single point yields a bare
/// moveto (nothing visible, same as the old polyline).
#[must_use]
pub fn smooth_path(points: &[(u32, u32)]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let at = |i: usize| points[i.min(points.len() - 1)];
    let mut d = format!("M{},{}", points[0].0, points[0].1);
    for i in 0..points.len().saturating_sub(1) {
        let (x0, y0) = at(i.saturating_sub(1));
        let (x1, y1) = at(i);
        let (x2, y2) = at(i + 1);
        let (x3, y3) = at(i + 2);
        let _ = write!(
            d,
            "C{},{},{},{},{},{}",
            curve(x1, x0, x2),
            curve(y1, y0, y2),
            curve(x2, x1, x3),
            curve(y2, y1, y3),
            x2,
            y2,
        );
    }
    d
}

/// One Catmull-Rom control coordinate, rounded to whole pixels: `p1` pulled
/// a sixth of the way toward the neighbor span. Integer math keeps the
/// path output compact.
#[allow(clippy::cast_possible_truncation)]
fn curve(p1: u32, p0: u32, p2: u32) -> i64 {
    // Pixel values stay far below 2^53, so the float round-trip is exact;
    // float-to-int `as` saturates rather than wrapping on extremes.
    let c = f64::from(p1) + (f64::from(p2) - f64::from(p0)) / 6.0;
    c.round() as i64
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
    let mut pixels = Vec::with_capacity(points.len());
    let mut dots = String::with_capacity(points.len() * 64);
    for (i, &(at, ms)) in points.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (ms.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        pixels.push((x, y));
        let _ = write!(
            dots,
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"7\" aria-hidden=true data-ts=\"{at}\" data-tk=\"full\" data-ms=\"{ms}\" data-tip=\"{}: {ms}ms\"/>",
            crate::time_label(at),
        );
    }
    format!(
        "<svg class=\"spark\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Latency sparkline\">\
<title>Latency sparkline</title>\
<path d=\"{}\" class=\"line\"/>{dots}</svg>",
        smooth_path(&pixels)
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
    let mut pixels = Vec::with_capacity(points.len());
    let mut dots = String::with_capacity(points.len() * 96);
    for (i, &(at, ms, place)) in points.iter().enumerate() {
        pixels.push((x(i), y(ms)));
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
<path d=\"{}\" class=\"line\"/>{dots}</svg>",
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
        smooth_path(&pixels)
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
    let mut pixels = Vec::with_capacity(vals.len());
    for (i, &v) in vals.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (v.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        pixels.push((x, y));
    }
    format!(
        "<svg width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-hidden=\"true\">\
<path d=\"{}\" fill=\"none\" stroke=\"#0969da\" stroke-width=\"1.5\"/></svg>",
        smooth_path(&pixels)
    )
}
