//! Vigil core: pure, host-testable monitoring logic.
//!
//! This crate has no I/O or platform dependencies so it can be unit
//! tested on any host. The `worker` crate owns fetching, storage, and
//! scheduling and calls into these pure functions.

pub mod checks;
pub mod clock;
pub mod config;
pub mod rollup;
pub mod site;
pub mod state;
pub mod upkeep;
pub mod views;

/// Seconds since the Unix epoch rendered as `MM-DD HH:MM` (chart axes).
#[must_use]
pub fn short_label(sec: i64) -> String {
    clock::short(sec)
}

/// Seconds since the Unix epoch rendered as `YYYY-MM-DD HH:MM:SS UTC`.
#[must_use]
pub fn time_label(sec: i64) -> String {
    clock::utc(sec)
}

/// Chart axis label as `DD-Month` (`12-June`); day ranges only.
#[must_use]
pub fn axis_day(sec: i64) -> String {
    clock::day_month(sec)
}

/// Chart axis label as `HH:MM`; the 24-hour view only.
#[must_use]
pub fn axis_hour(sec: i64) -> String {
    clock::hour_min(sec)
}

/// Chart and bar tooltip stamp as `DD-Month @ HH:MM` (`12-June @ 14:30`).
#[must_use]
pub fn tip_stamp(sec: i64) -> String {
    clock::tip_stamp(sec)
}

/// Relative age of `then` as seen at `now`: `just now`, `1 minute ago`,
/// `N minutes ago`, `1 hour ago`, … (floored to whole minutes).
#[must_use]
pub fn rel_label(now: i64, then: i64) -> String {
    clock::rel(now, then)
}

/// Seconds since the Unix epoch rendered as `YYYY-MM-DD` (date only,
/// for day cells where a clock time would be misleading).
#[must_use]
pub fn date_label(sec: i64) -> String {
    clock::date(sec)
}
