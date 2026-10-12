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
