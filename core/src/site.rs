//! Config validation: fail fast on typos before the cron tick stores junk.

use crate::config::{Monitor, Probe, Site};
use std::collections::BTreeSet;
use std::fmt;

/// Maximum accepted probe timeout (Workers subrequest ceiling is lower;
/// 60s keeps cron ticks bounded).
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// One config problem. Returned in bulk by [`Site::check`] so a single
/// run reports every typo instead of one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Site {
    /// Validate ids, timeouts, cadences, and probe targets.
    ///
    /// # Errors
    /// Returns every problem found (empty id, duplicate id, bad
    /// timeout, zero `every_min`, bad URL/host, or zero monitors).
    pub fn check(&self) -> Result<(), Vec<ConfigError>> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for m in &self.monitors {
            check_monitor(m, &mut seen, &mut problems);
        }
        if self.monitors.is_empty() {
            problems.push(ConfigError("no monitors".into()));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Find a monitor by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.id == id)
    }
}

fn check_monitor(m: &Monitor, seen: &mut BTreeSet<String>, problems: &mut Vec<ConfigError>) {
    if m.id.trim().is_empty() {
        problems.push(ConfigError("empty monitor id".into()));
    }
    if !seen.insert(m.id.clone()) {
        problems.push(ConfigError(format!("dup id {}", m.id)));
    }
    if m.timeout_ms == 0 || m.timeout_ms > MAX_TIMEOUT_MS {
        problems.push(ConfigError(format!("bad timeout {}", m.id)));
    }
    if m.every_min.is_some_and(|every| every == 0) {
        problems.push(ConfigError(format!("bad every_min {}", m.id)));
    }
    match &m.check {
        Probe::Http { url, .. } => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                problems.push(ConfigError(format!("bad url {}", m.id)));
            }
        }
        Probe::Tcp { host, port } => {
            if host.trim().is_empty() || *port == 0 {
                problems.push(ConfigError(format!("bad tcp {}", m.id)));
            }
        }
    }
}
