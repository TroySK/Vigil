//! Shared time-range machinery: the 24h/7d/30d/90d [`Range`] model, its
//! pills, and the hourly/daily history-strip builder behind the index and
//! detail pages.

use std::fmt::Write as _;

use vigil_core::rollup::{hour_bars, month_bars, DayBar};
use vigil_core::state::GapSpan;

/// Page time range: every ranged section covers exactly this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    /// Calendar days covered (1 selects the 24-hour view).
    pub(crate) days: i64,
    /// Human span used in headings ("24 hours", "7 days", …).
    pub(crate) label: &'static str,
    /// `?range=` value selecting this range.
    pub(crate) param: &'static str,
    /// Short pill text.
    pub(crate) short: &'static str,
}

impl Range {
    /// Seconds covered.
    pub(crate) fn secs(self) -> i64 {
        self.days.saturating_mul(86_400)
    }

    /// The 24-hour view renders hourly cells; day ranges render daily cells.
    pub(crate) fn hourly(self) -> bool {
        self.days <= 1
    }

    /// Long ranges read the daily rollups; short ranges keep raw samples
    /// (cheap at those spans and more detailed).
    pub(crate) fn uses_rollup(self) -> bool {
        self.days >= crate::reads::ROLLUP_MIN_DAYS
    }
}

/// Range pills in display order.
pub(crate) const RANGES: [Range; 4] = [RANGE_24H, RANGE_7D, RANGE_30D, RANGE_90D];
pub(crate) const RANGE_24H: Range = Range {
    days: 1,
    label: "24 hours",
    param: "24h",
    short: "24H",
};
pub(crate) const RANGE_7D: Range = Range {
    days: 7,
    label: "7 days",
    param: "7d",
    short: "7D",
};
pub(crate) const RANGE_30D: Range = Range {
    days: 30,
    label: "30 days",
    param: "30d",
    short: "30D",
};
pub(crate) const RANGE_90D: Range = Range {
    days: 90,
    label: "90 days",
    param: "90d",
    short: "90D",
};
/// Default range: a missing `?range=` falls back here. Unknown values are
/// rejected by [`exact_range`] instead of being masked by this default.
pub(crate) const DEFAULT_RANGE: Range = RANGE_90D;

/// Exact `?range=` match (case-insensitive). `None` for unknown values so
/// callers can return 400 instead of silently rendering the default and
/// caching a cache-busting URL.
#[must_use]
pub(crate) fn exact_range(raw: &str) -> Option<Range> {
    match raw.to_ascii_lowercase().as_str() {
        "24h" => Some(RANGE_24H),
        "7d" => Some(RANGE_7D),
        "30d" => Some(RANGE_30D),
        "90d" => Some(RANGE_90D),
        _ => None,
    }
}

/// One history strip: cells plus how to paint, label, and describe them.
pub(crate) struct Bars {
    pub(crate) cells: Vec<DayBar>,
    pub(crate) class: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) label: fn(i64) -> String,
    pub(crate) period: &'static str,
}

/// One history-bar `<i>` cell. The tooltip rides on `title` alone: it is
/// the no-JS fallback, and the client script localizes it into `data-tip`
/// on load (see `applyTz` in app.js), so the server never ships the same
/// string twice. `content` is the visually-hidden daily percentage
/// (hourly and no-data cells stay empty); `value` is the `data-v`
/// percentage carried only by cells with data.
pub(crate) fn bar_cell(
    bar: &DayBar,
    kind: &str,
    label: &dyn Fn(i64) -> String,
    content: &str,
    value: Option<&str>,
) -> String {
    use vigil_core::rollup::bar_color;
    if let Some(v) = value {
        let tip = format!("{} UTC: {:.1}%", label(bar.day), bar.pct);
        format!(
            "<i aria-hidden=true title=\"{tip}\" data-ts=\"{}\" data-tk=\"{kind}\" data-v=\"{v}\" style=\"background:{}\">{content}</i>",
            bar.day,
            bar_color(bar.pct),
        )
    } else {
        let tip = format!("{} UTC: no data", label(bar.day));
        format!(
            "<i class=nodata aria-hidden=true title=\"{tip}\" data-ts=\"{}\" data-tk=\"{kind}\"></i>",
            bar.day,
        )
    }
}

/// History cells for one range: 24 hourly cells for the 24-hour view, one
/// cell per day otherwise. Both share the `DayBar` shape, so one loop paints
/// them on either page. Generic over gap spans so reason-less rows (API
/// shapes without cloned reason strings) paint directly.
pub(crate) fn bars_for<G: GapSpan>(core: &[G], now: i64, range: Range, since: Option<i64>) -> Bars {
    if range.hourly() {
        Bars {
            cells: hour_bars(core, now, 24, since),
            class: "bars",
            kind: "tip",
            label: vigil_core::tip_stamp,
            period: "hours",
        }
    } else {
        Bars {
            cells: month_bars(core, now, usize::try_from(range.days).unwrap_or(90), since),
            class: "bars90",
            kind: "tip",
            label: vigil_core::tip_stamp,
            period: "days",
        }
    }
}

/// Header range dropdown: `<details>` needs no JS (selecting an option
/// navigates, which reloads the page and closes the menu). `href` builds
/// each option's URL — detail pages scope by monitor, the index by
/// nothing. Replaces the old in-page pills with one header control.
pub(crate) fn menu(active: Range, href: impl Fn(Range) -> String) -> String {
    let mut out = String::from("<details class=range-menu><summary aria-label=\"Time range\">");
    out.push_str(active.short);
    out.push_str("</summary><div class=range-pop role=menu>");
    for option in RANGES {
        if option == active {
            let _ = write!(
                out,
                "<a href=\"{}\" aria-current=\"page\" role=menuitem>{}</a>",
                href(option),
                option.short,
            );
        } else {
            let _ = write!(
                out,
                "<a href=\"{}\" role=menuitem aria-label=\"Show last {}\">{}</a>",
                href(option),
                option.label,
                option.short,
            );
        }
    }
    out.push_str("</div></details>");
    out
}

#[cfg(test)]
mod tests {
    use super::{exact_range, menu, RANGES, RANGE_90D};

    #[test]
    fn range_parses_known_params_and_defaults_to_90_days() {
        assert_eq!(exact_range("24h").map(|r| r.days), Some(1));
        assert_eq!(exact_range("7d").map(|r| r.days), Some(7));
        assert_eq!(exact_range("30d").map(|r| r.days), Some(30));
        assert_eq!(exact_range("90d").map(|r| r.days), Some(90));
        assert_eq!(exact_range("7D").map(|r| r.days), Some(7));
        assert_eq!(super::DEFAULT_RANGE.days, 90);
        assert_eq!(RANGES.len(), 4);
    }

    #[test]
    fn long_ranges_use_the_daily_rollup() {
        use super::{RANGE_24H, RANGE_30D, RANGE_7D, RANGE_90D};
        assert!(!RANGE_24H.uses_rollup());
        assert!(!RANGE_7D.uses_rollup());
        assert!(RANGE_30D.uses_rollup());
        assert!(RANGE_90D.uses_rollup());
    }

    #[test]
    fn exact_range_rejects_unknown_values() {
        assert_eq!(exact_range("24h").map(|r| r.days), Some(1));
        assert_eq!(exact_range("7D").map(|r| r.days), Some(7));
        assert_eq!(exact_range("30d").map(|r| r.days), Some(30));
        assert_eq!(exact_range("90d").map(|r| r.days), Some(90));
        for bad in ["", "bogus", "7", "24h ", " 7D ", "90days"] {
            assert!(exact_range(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn menu_summarizes_the_active_range() {
        let html = menu(RANGE_90D, |option| format!("/?range={}", option.param));
        assert!(html.contains("<details class=range-menu>"));
        assert!(html.contains(">90D</summary>"));
        assert!(html.contains("href=\"/?range=24h\""));
        assert!(html.contains("href=\"/?range=90d\""));
        assert!(html.contains("aria-current=\"page\""));
    }

    #[test]
    fn bar_cells_carry_title_without_prebaked_data_tip() {
        use super::bar_cell;
        use vigil_core::rollup::DayBar;
        let label = |ts: i64| vigil_core::tip_stamp(ts);
        // Data cell: tooltip on title alone (the client derives data-tip
        // on load), percentage in data-v, daily content kept.
        let data = DayBar {
            day: 86_400,
            pct: 50.0,
            has_data: true,
        };
        let html = bar_cell(&data, "tip", &label, "50.0%", Some("50.0%"));
        assert!(html.contains("title="));
        assert!(!html.contains("data-tip="), "tooltip must not ship twice");
        assert!(html.contains("data-v=\"50.0%\""));
        assert!(html.contains(">50.0%</i>"));
        // No-data cell: empty, dashed class, still no data-tip.
        let nodata = DayBar {
            day: 86_400,
            pct: 0.0,
            has_data: false,
        };
        let html = bar_cell(&nodata, "tip", &label, "", None);
        assert!(html.contains("nodata"));
        assert!(!html.contains("data-tip="));
        assert!(html.ends_with("></i>"));
    }
}
