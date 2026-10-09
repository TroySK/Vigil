//! Incident tracking: open/close downtime gaps per monitor.

use serde::{Deserialize, Serialize};

/// Seconds in one day; the retention and rollup granularity.
pub const SECS_PER_DAY: i64 = 86_400;
/// Hard cap on stored gaps per monitor (bounds D1 rows + memory).
pub const MAX_GAPS: usize = 2_000;

/// One contiguous downtime window. `to == None` means still ongoing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gap {
    /// Start time (unix seconds).
    pub at: i64,
    /// End time (unix seconds), or `None` while the outage continues.
    pub to: Option<i64>,
    /// Human-readable cause; appended with `" | "` on reason changes.
    pub why: String,
}

/// In-memory view of a monitor's gaps with open/close/trim helpers.
#[derive(Debug, Clone, Default)]
pub struct Track {
    /// Gaps in chronological order (oldest first) when built from storage.
    pub gaps: Vec<Gap>,
}

impl Track {
    /// The currently-open (ongoing) gap, if any.
    #[must_use]
    pub fn open(&self) -> Option<&Gap> {
        self.gaps.iter().rev().find(|g| g.to.is_none())
    }

    /// `true` when no gap is currently open.
    #[must_use]
    pub fn shut(&self) -> bool {
        self.open().is_none()
    }

    /// Record a down observation. Returns `true` when storage changed.
    ///
    /// Opens a new gap when shut; when a gap is already open and the
    /// reason changed, the new reason is appended (`"old | new"`).
    pub fn note_down(&mut self, now: i64, why: &str) -> bool {
        if let Some(open) = self.gaps.iter_mut().rev().find(|g| g.to.is_none()) {
            if open.why != why {
                open.why = format!("{} | {why}", open.why);
                return true;
            }
            return false;
        }
        self.gaps.push(Gap {
            at: now,
            to: None,
            why: why.into(),
        });
        true
    }

    /// Close the open gap at `now`. Returns `true` when one was closed.
    pub fn note_up(&mut self, now: i64) -> bool {
        match self.gaps.iter_mut().rev().find(|g| g.to.is_none()) {
            Some(open) => {
                open.to = Some(now);
                true
            }
            None => false,
        }
    }

    /// Drop gaps fully outside the retention window and enforce [`MAX_GAPS`].
    pub fn trim(&mut self, now: i64, keep_days: i64) {
        let cutoff = now.saturating_sub(keep_days.max(0).saturating_mul(SECS_PER_DAY));
        // `Option::is_none_or` needs Rust 1.82; `map_or` keeps MSRV 1.75.
        self.gaps.retain(|g| g.to.map_or(true, |end| end >= cutoff));
        if self.gaps.len() > MAX_GAPS {
            let excess = self.gaps.len() - MAX_GAPS;
            self.gaps.drain(..excess);
        }
    }

    /// Percentage of `[now - window, now]` covered by *no* gap.
    ///
    /// Unrecorded time counts as up (documented on the status page).
    /// Returns `100.0` for non-positive windows.
    #[must_use]
    pub fn uptime_pct(&self, now: i64, window: i64) -> f64 {
        if window <= 0 {
            return 100.0;
        }
        let from = now.saturating_sub(window);
        let mut down = 0_i64;
        for g in &self.gaps {
            let start = g.at.max(from);
            let end = g.to.unwrap_or(now).min(now);
            if end > start {
                down = down.saturating_add(end - start);
            }
        }
        let total = (now - from).max(1);
        #[allow(clippy::cast_precision_loss)]
        let pct = (total - down).max(0) as f64 / total as f64 * 100.0;
        // Round to one decimal place without pulling in a float-format dep.
        (pct * 10.0).round() / 10.0
    }
}
