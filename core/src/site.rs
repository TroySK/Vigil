//! Config validation: fail fast on typos before the cron tick stores junk.

use crate::config::{Gate, Monitor, Probe, Site, MIN_CADENCE_MIN, DEFAULT_SAMPLE_MIN};
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
    /// timeout, sub-5-minute uptime cadence, bad URL/host, or zero monitors).
    pub fn check(&self) -> Result<(), Vec<ConfigError>> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for m in &self.monitors {
            check_monitor(m, &mut seen, &mut problems);
        }
        if self.sample_every_min != 0
            && self.sample_every_min < MIN_CADENCE_MIN
            && self.sample_every_min < DEFAULT_SAMPLE_MIN
        {
            // Global sample cadence below the uptime floor is a typo
            // (samples ride on probes); per-monitor values clamp instead.
            problems.push(ConfigError("bad sample_every_min".into()));
        }
        if self.monitors.is_empty() {
            problems.push(ConfigError("no monitors".into()));
        }
        for (label, url) in &self.links {
            if !valid_link(url, true) {
                problems.push(ConfigError(format!("bad link {label}")));
            }
        }
        for hook in &self.notify.hooks {
            if !valid_link(&hook.url, false) {
                problems.push(ConfigError("bad hook url".into()));
            }
        }
        if let Some(gate) = &self.auth {
            check_auth(gate, &mut problems);
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
    if m.uptime_every_min().is_some_and(|every| every < MIN_CADENCE_MIN) {
        problems.push(ConfigError(format!("bad every_min {}", m.id)));
    }
    if m.href.as_deref().is_some_and(|href| !valid_link(href, false)) {
        problems.push(ConfigError(format!("bad href {}", m.id)));
    }
    match &m.check {
        Probe::Http { url, .. } => {
            if !valid_http_url(url) {
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

/// `true` for an absolute `http(s)` URL with a host.
fn valid_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some())
}

/// `true` for a renderable link: a same-origin root-relative path, an
/// absolute `http(s)` URL, or (footer links only) `mailto:`. Control
/// characters are rejected so `java\nscript:` cannot slip through.
fn valid_link(value: &str, allow_mailto: bool) -> bool {
    if value.chars().any(char::is_control) {
        return false;
    }
    if value.starts_with('/') {
        // `//evil.example` is protocol-relative, not same-origin.
        return !value.starts_with("//");
    }
    url::Url::parse(value).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            || (allow_mailto && parsed.scheme() == "mailto")
    })
}

/// Validate the optional Basic-auth gate. A gate with no resolvable
/// password would otherwise authenticate `user:` (empty password), so
/// ambiguous or empty credentials are configuration errors.
fn check_auth(gate: &Gate, problems: &mut Vec<ConfigError>) {
    if gate.user.trim().is_empty() {
        problems.push(ConfigError("empty auth user".into()));
    }
    if gate.user.contains(':') {
        problems.push(ConfigError("auth user must not contain a colon".into()));
    }
    match (&gate.pass, &gate.pass_env) {
        (None, None) => problems.push(ConfigError(
            "auth needs a password (set pass or pass_env)".into(),
        )),
        (Some(pass), None) if pass.is_empty() => {
            problems.push(ConfigError("empty auth password".into()));
        }
        (None, Some(key)) if key.is_empty() => {
            problems.push(ConfigError("empty auth password".into()));
        }
        (Some(_), Some(_)) => problems.push(ConfigError("auth sets both pass and pass_env".into())),
        _ => {}
    }
}
