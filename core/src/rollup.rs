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

/// Two nearest-rank percentiles with a single sort.
///
/// Hot pages need P50 + P95 per monitor; two [`percentile`] calls sort the
/// same samples twice. Returns `(0, 0)` when empty.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile_pair(values: &[i64], p1: f64, p2: f64) -> (i64, i64) {
    if values.is_empty() {
        return (0, 0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    let rank = |p: f64| {
        ((p.clamp(0.0, 100.0) / 100.0 * len as f64).ceil() as usize).clamp(1, len) - 1
    };
    (sorted[rank(p1)], sorted[rank(p2)])
}

/// Three nearest-rank percentiles with a single sort (latency API).
/// Returns `(0, 0, 0)` when empty.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile_triple(values: &[i64], p1: f64, p2: f64, p3: f64) -> (i64, i64, i64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    let rank = |p: f64| {
        ((p.clamp(0.0, 100.0) / 100.0 * len as f64).ceil() as usize).clamp(1, len) - 1
    };
    (sorted[rank(p1)], sorted[rank(p2)], sorted[rank(p3)])
}

/// Smooth SVG path through pixel points: monotone cubic (Fritsch–Carlson)
/// segments, so latency lines read as trends instead of zigzags while
/// passing through every input point — per-sample dots sit on the line.
/// Monotonicity preservation keeps spiky samples from overshooting into
/// sharp `^` peaks. Empty input yields an empty string; a single point
/// yields a bare moveto (nothing visible).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn smooth_path(points: &[(u32, u32)]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let first = points[0];
    if points.len() == 1 {
        return format!("M{},{}", first.0, first.1);
    }
    if points.len() == 2 {
        let last = points[1];
        return format!("M{},{} L{},{}", first.0, first.1, last.0, last.1);
    }
    let n = points.len();
    let xf: Vec<f64> = points.iter().map(|&(x, _)| f64::from(x)).collect();
    let yf: Vec<f64> = points.iter().map(|&(_, y)| f64::from(y)).collect();
    // Segment widths and slopes. Zero-width segments are vertical jumps
    // (stacked samples in one pixel column): drawn as straight lines
    // with flattened neighbor tangents.
    let mut h = vec![0.0_f64; n - 1];
    let mut delta = vec![0.0_f64; n - 1];
    for i in 0..n - 1 {
        h[i] = xf[i + 1] - xf[i];
        delta[i] = if h[i] > 0.0 {
            (yf[i + 1] - yf[i]) / h[i]
        } else {
            0.0
        };
    }
    // Fritsch–Carlson tangents: zero at direction changes, slope-weighted
    // harmonic mean elsewhere.
    let mut m = vec![0.0_f64; n];
    m[0] = delta[0];
    m[n - 1] = delta[n - 2];
    for i in 1..n - 1 {
        if delta[i - 1] == 0.0
            || delta[i] == 0.0
            || delta[i - 1].signum() != delta[i].signum()
        {
            m[i] = 0.0;
        } else {
            let w1 = 2.0 * h[i] + h[i - 1];
            let w2 = h[i] + 2.0 * h[i - 1];
            m[i] = (w1 + w2) / (w1 / delta[i - 1] + w2 / delta[i]);
        }
    }
    // Clamp tangents into the overshoot-free rectangle: segments with a
    // flat slope pin both tangents, others scale to alpha²+β² ≤ 9.
    for i in 0..n - 1 {
        if delta[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let alpha = m[i] / delta[i];
            let beta = m[i + 1] / delta[i];
            let over = alpha * alpha + beta * beta;
            if over > 9.0 {
                let scale = 3.0 / over.sqrt();
                m[i] *= scale;
                m[i + 1] *= scale;
            }
        }
    }
    // Pixel space has no negatives: clamp rounded control points at zero.
    let round = |v: f64| v.round().max(0.0) as u32;
    let mut d = format!("M{},{}", first.0, first.1);
    for i in 0..n - 1 {
        let (x2, y2) = points[i + 1];
        if h[i] <= 0.0 {
            let _ = write!(d, " L{x2},{y2}");
            continue;
        }
        let _ = write!(
            d,
            " C{},{} {},{} {x2},{y2}",
            round(xf[i] + h[i] / 3.0),
            round(yf[i] + m[i] * h[i] / 3.0),
            round(xf[i + 1] - h[i] / 3.0),
            round(yf[i + 1] - m[i + 1] * h[i] / 3.0),
        );
    }
    d
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
    let mut marks = String::with_capacity(points.len() * 32);
    let mut dots = String::with_capacity(points.len() * 64);
    for (i, &(at, ms)) in points.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (ms.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        pixels.push((x, y));
        let _ = write!(marks, "<circle class=\"dot\" cx=\"{x}\" cy=\"{y}\" r=\"2\"/>");
        let _ = write!(
            dots,
            "<circle class=\"hit\" cx=\"{x}\" cy=\"{y}\" r=\"7\" aria-hidden=true data-ts=\"{at}\" data-tk=\"tip\" data-ms=\"{ms}\" data-tip=\"{} UTC: {ms}ms\"/>",
            crate::tip_stamp(at),
        );
    }
    format!(
        "<svg class=\"spark\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Latency sparkline\">\
<title>Latency sparkline</title>\
<path d=\"{}\" class=\"line\"/>{marks}{dots}</svg>",
        smooth_path(&pixels)
    )
}

/// Latency chart with labeled X (probe time) and Y (milliseconds) axes.
///
/// X labels read `HH:MM` on the 24-hour view (`hourly`) and `DD-Month`
/// otherwise. Points are `(sample time, milliseconds, origin colo)`; each
/// sample gets an invisible hover target whose tooltip names all three,
/// since probes in one chart can come from different colos. Empty input
/// yields an empty string.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn latency_chart(points: &[(i64, i64, &str)], w: u32, h: u32, hourly: bool) -> String {
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
    let mut marks = String::with_capacity(points.len() * 48);
    let mut dots = String::with_capacity(points.len() * 96);
    for (i, &(at, ms, place)) in points.iter().enumerate() {
        pixels.push((x(i), y(ms)));
        let origin = crate::views::escape_html(place);
        let _ = write!(marks, "<circle class=\"dot\" cx=\"{}\" cy=\"{}\" r=\"2\"/>", x(i), y(ms));
        let _ = write!(
            dots,
            "<circle class=\"hit\" cx=\"{}\" cy=\"{}\" r=\"7\" aria-hidden=true data-ts=\"{at}\" data-tk=\"tip\" data-place=\"{origin}\" data-ms=\"{ms}\" data-tip=\"{} UTC &middot; {origin} &middot; {ms}ms\"/>",
            x(i),
            y(ms),
            crate::tip_stamp(at),
        );
    }
    let mid = max_ms / 2;
    let base = pad_t + plot_h;
    let mid_i = points.len() / 2;
    let (axis_kind, axis_label): (&str, fn(i64) -> String) = if hourly {
        ("hour", crate::axis_hour)
    } else {
        ("day", crate::axis_day)
    };
    format!(
        "<svg class=\"chart\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Latency chart\">\
<title>Latency over time</title>\
<line class=\"grid\" x1=\"{pad_l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>\
<line class=\"grid\" x1=\"{pad_l}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>\
<line class=\"axis\" x1=\"{pad_l}\" y1=\"{base}\" x2=\"{}\" y2=\"{base}\"/>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{max_ms}ms</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{mid}ms</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\">0</text>\
<text x=\"{pad_l}\" y=\"{}\" text-anchor=\"start\" data-ts=\"{}\" data-tk=\"{}\">{}</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" data-ts=\"{}\" data-tk=\"{}\">{}</text>\
<text x=\"{}\" y=\"{}\" text-anchor=\"end\" data-ts=\"{}\" data-tk=\"{}\">{}</text>\
<path d=\"{}\" class=\"line\"/>{marks}{dots}</svg>",
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
        axis_kind,
        axis_label(points[0].0),
        pad_l + plot_w / 2,
        h - 6,
        points[mid_i].0,
        axis_kind,
        axis_label(points[mid_i].0),
        pad_l + plot_w,
        h - 6,
        points[points.len() - 1].0,
        axis_kind,
        axis_label(points[points.len() - 1].0),
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
    let mut marks = String::with_capacity(vals.len() * 32);
    for (i, &v) in vals.iter().enumerate() {
        let x = (i as f64 * step).round() as u32;
        let scaled = (v.max(0) as f64 / max) * f64::from(h.saturating_sub(4));
        let y = h.saturating_sub(scaled.round() as u32 + 2);
        pixels.push((x, y));
        let _ = write!(marks, "<circle cx=\"{x}\" cy=\"{y}\" r=\"2\" fill=\"#0969da\"/>");
    }
    format!(
        "<svg width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-hidden=\"true\">\
<path d=\"{}\" fill=\"none\" stroke=\"#0969da\" stroke-width=\"1.5\"/>{marks}</svg>",
        smooth_path(&pixels)
    )
}
