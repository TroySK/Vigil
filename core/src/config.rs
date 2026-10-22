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
    /// Footer links (`label -> url`), rendered in file order.
    #[serde(default)]
    pub links: indexmap::IndexMap<String, String>,
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
    /// Default probe cadence in minutes for monitors without a
    /// [`Monitor::uptime_every_min`] override. Unset/`0` means every tick
    /// (the cron fires every 5 minutes); per-monitor overrides can only
    /// slow down, never speed up.
    #[serde(default)]
    pub check_every_min: u64,
    /// Default latency-sample cadence in minutes for monitors without a
    /// [`Monitor::sample_every_min`] override. Unset/`0` means hourly:
    /// probes still run on the uptime cadence for fast paging, but
    /// `samples` rows (charts, percentiles) are written at most once per
    /// hour so D1 sample writes and chart scans stay flat.
    #[serde(default)]
    pub sample_every_min: u64,
    /// Auto-refresh interval in seconds for status pages (`None` means the
    /// hourly default; `Some(0)` disables polling entirely).
    #[serde(default)]
    pub refresh_secs: Option<u64>,
}

/// Minimum uptime-probe cadence in minutes: 5. Faster probing multiplies
/// D1 `kv` reads/writes (every tick does a 4N-key `kv_map` plus cadence
/// stamps) against the 5M rows/day quota, so anything faster is rejected
/// by [`Site::check`] and clamped here.
pub const MIN_CADENCE_MIN: u64 = 5;
/// Default latency-sample cadence in minutes: hourly. Charts and
/// percentiles need one dot per hour, not one per probe — a 5-minute
/// fleet writes 12x the `samples` rows for identical pixels.
pub const DEFAULT_SAMPLE_MIN: u64 = 60;
/// Default page refresh when `refresh_secs` is unset: 5 minutes, matching
/// the probe cadence so the page reloads on fresh uptime data without
/// burning D1 reads between ticks.
pub const DEFAULT_REFRESH_SECS: u64 = 300;
/// Floor for a configured page refresh. Sub-minute polling multiplies
/// health reads per open tab for no visible gain; `0` still disables.
pub const MIN_REFRESH_SECS: u64 = 30;

impl Site {
    /// Uptime-probe cadence for a monitor in minutes: its own
    /// `uptime_every_min` (legacy `every_min`) when set, else the global
    /// `check_every_min`; unset/`0` means every tick, always at least
    /// [`MIN_CADENCE_MIN`] so one fleet can't burn the D1 rows quota.
    #[must_use]
    pub fn cadence_min(&self, monitor: &Monitor) -> u64 {
        monitor
            .uptime_every_min()
            .unwrap_or(self.check_every_min)
            .max(MIN_CADENCE_MIN)
    }

    /// Latency-sample cadence for a monitor in minutes: its own
    /// `sample_every_min` when set, else the global `sample_every_min`;
    /// unset/`0` means [`DEFAULT_SAMPLE_MIN`] (hourly). Always at least
    /// the uptime cadence — samples ride on probes, so sampling faster
    /// than probing is meaningless.
    #[must_use]
    pub fn sample_cadence_min(&self, monitor: &Monitor) -> u64 {
        let sample = monitor
            .sample_every_min
            .unwrap_or(self.sample_every_min);
        let sample = if sample == 0 {
            DEFAULT_SAMPLE_MIN
        } else {
            sample
        };
        sample.max(self.cadence_min(monitor))
    }

    /// Page auto-refresh interval in milliseconds: configured seconds
    /// clamped to [`MIN_REFRESH_SECS`], or the hourly default when unset;
    /// `0` disables polling.
    #[must_use]
    pub fn refresh_ms(&self) -> u64 {
        match self.refresh_secs {
            None => DEFAULT_REFRESH_SECS.saturating_mul(1_000),
            Some(0) => 0,
            Some(secs) => secs.max(MIN_REFRESH_SECS).saturating_mul(1_000),
        }
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
    /// Probe every N minutes instead of hourly (`None` inherits the
    /// global [`Site::check_every_min`]). Values below 5 minutes are
    /// rejected by [`Site::check`] to protect the D1 rows quota.
    ///
    /// Legacy name for [`Monitor::uptime_every_min`]: kept so existing
    /// configs keep working; when both are set, `uptime_every_min` wins.
    #[serde(default)]
    pub every_min: Option<u64>,
    /// Uptime-probe cadence override in minutes (`None` inherits the
    /// global [`Site::check_every_min`], falling back to `every_min` for
    /// legacy configs). Unset/`0` means every tick; values below
    /// [`MIN_CADENCE_MIN`] are rejected by [`Site::check`].
    #[serde(default)]
    pub uptime_every_min: Option<u64>,
    /// Latency-sample cadence override in minutes (`None` inherits the
    /// global [`Site::sample_every_min`], unset = hourly). Samples ride on
    /// probes, so values below the uptime cadence clamp up to it.
    #[serde(default)]
    pub sample_every_min: Option<u64>,
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

impl Monitor {
    /// Effective uptime-probe override: `uptime_every_min` when set, else
    /// legacy `every_min` (`None` inherits the global cadence).
    #[must_use]
    pub fn uptime_every_min(&self) -> Option<u64> {
        self.uptime_every_min.or(self.every_min)
    }
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
