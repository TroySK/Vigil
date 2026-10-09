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

/// Seconds since the Unix epoch rendered as `YYYY-MM-DD HH:MM:SS UTC`.
#[must_use]
pub fn time_label(sec: i64) -> String {
    clock::utc(sec)
}
