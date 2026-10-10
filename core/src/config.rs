//! Config model: the status site, its monitors, notifiers, and upkeep windows.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default dashboard title when `title` is omitted.
pub const DEFAULT_TITLE: &str = "Status";
/// Default per-probe timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Default HTTP method / webhook method / payload mode.
pub const DEFAULT_HTTP_METHOD: &str = "GET";
/// Default HTTP method / webhook method / payload mode.
pub const DEFAULT_HOOK_METHOD: &str = "POST";
/// Default HTTP method / webhook method / payload mode.
pub const DEFAULT_HOOK_MODE: &str = "json";

/// An entire status site: monitors, grouping, notifications, upkeep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Site {
    /// Dashboard title.
    #[serde(default = "default_title")]
    pub title: String,
    /// Footer links (`label -> url`).
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    /// Group name -> monitor ids (unlisted monitors render under "Other").
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
    /// All monitors; ids must be unique and stable (history keys off them).
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    /// Webhook / grace configuration.
    #[serde(default)]
    pub notify: Notify,
    /// Scheduled maintenance windows.
    #[serde(default)]
    pub upkeep: Vec<Upkeep>,
    /// Optional Basic-auth gate for the whole site.
    #[serde(default)]
    pub auth: Option<Gate>,
    /// Default probe cadence in minutes for monitors without an
    /// [`Monitor::every_min`] override (`0`/unset means every tick).
    #[serde(default)]
    pub check_every_min: u64,
}

impl Site {
    /// Probe cadence for a monitor in minutes: its own `every_min` when
    /// set, else the global `check_every_min`; always at least 1.
    #[must_use]
    pub fn cadence_min(&self, monitor: &Monitor) -> u64 {
        monitor.every_min.unwrap_or(self.check_every_min).max(1)
    }
}

fn default_title() -> String {
    DEFAULT_TITLE.into()
}

/// One monitored endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Monitor {
    /// Stable id; used as the D1 history key — never rename casually.
    pub id: String,
    /// Human display name.
    pub name: String,
    /// How to probe it.
    pub check: Probe,
    /// Accepted HTTP status codes; empty means any `2xx`.
    #[serde(default)]
    pub expect: Vec<u16>,
    /// Per-probe timeout in milliseconds (`1..=60_000`).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Body must contain this substring (empty / `None` disables).
    #[serde(default)]
    pub has: Option<String>,
    /// Body must *not* contain this substring.
    #[serde(default)]
    pub lacks: Option<String>,
    /// Hover tooltip on the status page.
    #[serde(default)]
    pub tip: Option<String>,
    /// Link wrapped around the monitor name.
    #[serde(default)]
    pub href: Option<String>,
    /// Hide the latency sparkline for this monitor.
    #[serde(default)]
    pub quiet_chart: bool,
    /// Probe every N minutes instead of every tick (`None` inherits the
    /// global [`Site::check_every_min`]). Explicit `0` is rejected by
    /// [`Site::check`].
    #[serde(default)]
    pub every_min: Option<u64>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// How to reach a monitor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "via")]
pub enum Probe {
    /// HTTP(S) request via outbound `fetch`.
    #[serde(rename = "http")]
    Http {
        /// Absolute `http(s)://…` URL.
        url: String,
        /// HTTP method (default `GET`).
        #[serde(default = "default_method")]
        method: String,
        /// Extra request headers.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Optional request body.
        #[serde(default)]
        body: Option<String>,
    },
    /// TCP connect check (Workers sockets; no raw ICMP/UDP).
    #[serde(rename = "tcp")]
    Tcp {
        /// Hostname or IP.
        host: String,
        /// TCP port (non-zero).
        port: u16,
    },
}

fn default_method() -> String {
    DEFAULT_HTTP_METHOD.into()
}

/// Notification / grace configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Notify {
    /// Webhooks paged on transitions (down after grace, recovery always).
    #[serde(default)]
    pub hooks: Vec<Hook>,
    /// Consecutive failing ticks before paging (min 1 enforced at use).
    #[serde(default)]
    pub grace_min: u64,
    /// Monitor ids that never page.
    #[serde(default)]
    pub mute: Vec<String>,
}

/// One outgoing webhook.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hook {
    /// Destination URL.
    pub url: String,
    /// HTTP method (default `POST`).
    #[serde(default = "default_hook_method")]
    pub method: String,
    /// `"json"` (`{"text": …}`) or anything else (form `text=…`).
    #[serde(default = "default_hook_mode")]
    pub mode: String,
    /// `$NAME` / `$STATUS` / `$REASON` / `$TIME` template.
    pub text: String,
}

fn default_hook_method() -> String {
    DEFAULT_HOOK_METHOD.into()
}

fn default_hook_mode() -> String {
    DEFAULT_HOOK_MODE.into()
}

/// A scheduled maintenance window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upkeep {
    /// Short title shown in the banner.
    pub title: String,
    /// Longer description shown while active.
    pub body: String,
    /// Start: unix seconds or RFC-3339.
    pub from: String,
    /// End: unix seconds or RFC-3339; `None` means open-ended.
    #[serde(default)]
    pub until: Option<String>,
    /// Restrict to these monitor ids; empty means site-wide.
    #[serde(default)]
    pub only: Vec<String>,
}

/// Optional Basic-auth gate for the whole site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gate {
    /// Expected username.
    pub user: String,
    /// Password in cleartext (prefer `pass_env` + a secret instead).
    #[serde(default)]
    pub pass: Option<String>,
    /// `Env` var name holding the password (e.g. `STATUS_PASSWORD`).
    #[serde(default)]
    pub pass_env: Option<String>,
}
