//! Verdicts: turn a raw probe observation into an up/down statement.

use crate::config::Monitor;

/// A raw probe observation plus how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// Elapsed milliseconds for the probe attempt.
    pub ms: u64,
    /// What the probe observed.
    pub what: What,
}

/// The raw outcome of a single probe attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// HTTP completed: status code plus a truncated body sample.
    Http {
        /// HTTP status code.
        code: u16,
        /// Truncated response body sample for keyword matching.
        body: String,
    },
    /// TCP connect attempt result.
    Tcp {
        /// Whether the TCP connect succeeded.
        open: bool,
    },
    /// The probe never got an answer (DNS, timeout, socket error, …).
    Dead {
        /// Human-readable failure reason.
        why: String,
    },
}

/// The judged verdict for a monitor at one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Say {
    /// Whether the monitor counts as up.
    pub up: bool,
    /// Short human-readable reason (safe to store and display).
    pub why: String,
}

/// Judge one observation against a monitor's expectations.
///
///HTTP status allow-list (`expect`, default 2xx) is checked first, then
///the `has` / `lacks` keyword rules.
#[must_use]
pub fn judge(m: &Monitor, s: &Seen) -> Say {
    match &s.what {
        What::Dead { why } => Say {
            up: false,
            why: clip(why),
        },
        What::Tcp { open } => {
            if *open {
                Say {
                    up: true,
                    why: "OK".into(),
                }
            } else {
                Say {
                    up: false,
                    why: "TCP refused".into(),
                }
            }
        }
        What::Http { code, body } => {
            let ok = if m.expect.is_empty() {
                (200..300).contains(code)
            } else {
                m.expect.contains(code)
            };
            if !ok {
                let want = if m.expect.is_empty() {
                    "2xx".to_owned()
                } else {
                    format!("{:?}", m.expect)
                };
                return Say {
                    up: false,
                    why: format!("want {want} got {code}"),
                };
            }
            if let Some(needle) = m.has.as_deref().filter(|n| !n.is_empty()) {
                if !body.contains(needle) {
                    return Say {
                        up: false,
                        why: "missing keyword".into(),
                    };
                }
            }
            if let Some(banned) = m.lacks.as_deref().filter(|b| !b.is_empty()) {
                if body.contains(banned) {
                    return Say {
                        up: false,
                        why: "forbidden keyword".into(),
                    };
                }
            }
            Say {
                up: true,
                why: "OK".into(),
            }
        }
    }
}

/// Collapse whitespace and truncate to a display-safe length.
///
/// Truncation always lands on a `char` boundary so non-ASCII reasons
/// can never panic the cron tick.
fn clip(s: &str) -> String {
    const MAX_CHARS: usize = 160;
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "fetch failed".into();
    }
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let end = flat
        .char_indices()
        .nth(MAX_CHARS)
        .map_or(flat.len(), |(idx, _)| idx);
    format!("{}..", &flat[..end])
}

/// Normalize a colo identifier for storage (`place` column).
///
/// Blank / missing values fall back to `"CRON"` so every sample row
/// has a non-empty origin.
#[must_use]
pub fn place(colo: Option<&str>) -> String {
    match colo.map(str::trim) {
        Some(c) if !c.is_empty() => c.to_uppercase(),
        _ => "CRON".into(),
    }
}
