//! Small view models shared by the status page, badges, and webhooks.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Aggregate snapshot served by `/api/health`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snap {
    /// Monitors currently up.
    pub up: u32,
    /// Monitors currently down.
    pub down: u32,
    /// Unix seconds of the last cron tick that produced this snapshot.
    pub at: i64,
    /// Per-monitor details keyed by monitor id.
    pub items: BTreeMap<String, Item>,
}

/// Per-monitor row inside [`Snap`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    /// Whether the monitor is currently up.
    pub up: bool,
    /// Last probe duration in milliseconds.
    pub ms: u64,
    /// Last probe time rendered as a UTC label.
    pub at: String,
    /// Short human-readable note (from [`crate::checks::Say`]).
    pub note: String,
}

/// Badge word + color for shields (`up`/`down`).
#[must_use]
pub fn badge(up: bool) -> (&'static str, &'static str) {
    if up {
        ("up", "#2da44e")
    } else {
        ("down", "#cf222e")
    }
}

/// Escape text for embedding in HTML or SVG element content/attributes.
#[must_use]
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Minimal shields-style SVG badge. Inputs are HTML-escaped.
#[must_use]
pub fn svg(label: &str, value: &str, color: &str) -> String {
    // Cheap width estimate: padding + ~6px per char (matches prior output).
    let label_w = 34 + label.chars().count() * 6;
    let value_w = 34 + value.chars().count() * 6;
    let width = label_w + value_w;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"20\">\
<rect width=\"{label_w}\" height=\"20\" fill=\"#555\"/>\
<rect x=\"{label_w}\" width=\"{value_w}\" height=\"20\" fill=\"{color}\"/>\
<text x=\"{}\" y=\"14\" fill=\"#fff\" font-size=\"11\">{}</text>\
<text x=\"{}\" y=\"14\" fill=\"#fff\" font-size=\"11\">{}</text></svg>",
        label_w / 2,
        escape_html(label),
        label_w + value_w / 2,
        escape_html(value),
    )
}

/// One-line webhook/alert text: `[DOWN] Name: reason (time)`.
#[must_use]
pub fn alert(name: &str, up: bool, why: &str, at_utc: &str) -> String {
    let state = if up { "RECOVERED" } else { "DOWN" };
    format!("[{state}] {name}: {why} ({at_utc})")
}

/// Fill a `$NAME` / `$STATUS` / `$REASON` / `$TIME` webhook template.
#[must_use]
pub fn fill(template: &str, name: &str, up: bool, why: &str, at: &str) -> String {
    let status = if up { "UP" } else { "DOWN" };
    template
        .replace("$NAME", name)
        .replace("$STATUS", status)
        .replace("$REASON", why)
        .replace("$TIME", at)
}
