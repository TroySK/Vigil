//! Rollups for history bars, latency percentiles, and sparklines.

use crate::state::Gap;
use std::fmt::Write as _;

/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;
/// Uptime thresholds for green / amber / red history cells.
pub const GREEN_PCT: f64 = 99.9;
/// Uptime thresholds for green / amber / red history cells.
pub const AMBER_PCT: f64 = 95.0;

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
/// (oldest → newest). Each sample gets an invisible hover target carrying
/// `<title>{time}: {ms}ms</title>`; empty input yields an empty string.
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
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"7\" fill=\"transparent\">\
            <title>{}: {ms}ms</title></circle>",
            crate::time_label(at),
        );
    }
    format!(
        "<svg width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-hidden=\"true\">\
<polyline points=\"{}\" fill=\"none\" stroke=\"#0969da\" stroke-width=\"1.5\"/>{dots}</svg>",
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
