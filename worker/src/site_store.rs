//! Site config loading: raw YAML from D1 `kv`, parsed at request time.
//!
//! The YAML is stored under [`vigil::SITE_KV_KEY`] by the config editor
//! (see [`crate::editor`]). Nothing is compiled in: a missing key means
//! the operator has not seeded the config yet.

use crate::db::Db;
use vigil_core::config::Site;
use worker::{Date, Env, Result};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// How long an isolate trusts its cached raw YAML. Config changes flow
/// through the editor + redeploy path (which starts a fresh isolate), but a
/// short TTL also picks up direct D1 edits without a request-per-read cost.
pub const RAW_TTL_SECS: i64 = 60;

/// `true` when the worker runs in config-editor mode (`EDITOR=true`).
///
/// In editor mode the status site does not serve: every route renders the
/// editor instead, and the cron tick skips probing. Remove the var and
/// redeploy to restore normal operation.
#[must_use]
pub fn is_editor(env: &Env) -> bool {
    env.var("EDITOR").is_ok_and(|v| v.to_string() == "true")
}

/// Raw site YAML from D1, if the editor has saved one.
pub async fn load_raw(db: &Db) -> Result<Option<String>> {
    db.get(vigil::SITE_KV_KEY).await
}

/// Failure to resolve the site config: missing YAML or invalid content.
#[derive(Debug, Clone)]
pub enum SiteError {
    /// No YAML has been saved yet (fresh database).
    Missing,
    /// Saved YAML fails to parse.
    Parse(String),
    /// Saved YAML parses but fails [`Site::check`].
    Invalid(Vec<String>),
}

impl std::fmt::Display for SiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing"),
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Invalid(errors) => write!(f, "invalid: {}", errors.join("; ")),
        }
    }
}

/// Load and validate the site config from D1, memoized per raw bytes.
///
/// The raw YAML is cached in the isolate for [`RAW_TTL_SECS`], so the
/// steady-state request path costs no D1 round trip for config at all.
/// YAML parsing plus [`Site::check`] runs only when the stored bytes
/// change: isolates are reused across requests/ticks, so re-parsing an
/// unchanged multi-KB config every 30s poll would burn the Worker's CPU
/// budget for no new information. Failures are memoized too — a bad save
/// keeps reporting its error instead of serving stale config.
pub async fn load(db: &Db) -> Result<std::result::Result<Loaded, SiteError>> {
    let now = now_secs();
    if let Some(raw) = global().cached_raw(now) {
        return Ok(match raw {
            Some(raw) => global().get(&raw),
            None => Err(SiteError::Missing),
        });
    }
    let raw = load_raw(db).await?;
    global().remember_raw(raw.clone(), now);
    Ok(match raw {
        Some(raw) => global().get(&raw),
        None => Err(SiteError::Missing),
    })
}

/// Drop both the raw and parsed caches (config editor save).
pub fn invalidate() {
    global().invalidate();
}

/// Current time truncated to whole seconds.
#[allow(clippy::cast_possible_wrap)] // epoch millis fit in i64 for millennia
fn now_secs() -> i64 {
    (Date::now().as_millis() / 1_000) as i64
}

/// Site config plus parse-time derivatives, memoized per raw bytes by
/// [`SiteCache`]: one D1 read yields everything a request needs without
/// re-parsing or re-deriving per row.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// Validated site config.
    pub site: Arc<Site>,
    /// Monitor id → https favicon origin, derived once at parse time so
    /// renders never pay `Url::parse` per row per request. Monitors
    /// without a web origin (TCP) are absent; lookup misses fall back to
    /// a live parse so config races stay correct.
    pub favicons: Arc<HashMap<String, String>>,
}

/// One memoized parse: the raw bytes plus their outcome.
#[derive(Debug)]
struct Cached {
    raw: String,
    result: std::result::Result<Loaded, SiteError>,
}

/// One memoized raw read: the value (or absence) plus when it expires.
#[derive(Debug)]
struct RawCached {
    value: Option<String>,
    expires_at: i64,
}

/// Byte-keyed memo for [`load`]: cheap to consult on every request, exact
/// on every save. A dedicated struct (rather than bare statics) keeps the
/// policy unit-testable without global state.
#[derive(Debug, Default)]
pub struct SiteCache {
    inner: Mutex<Option<Cached>>,
    raw: Mutex<Option<RawCached>>,
}

impl SiteCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cached raw YAML, if it has not expired. `Some(None)` means the
    /// database was recently known to have no config.
    #[must_use]
    pub fn cached_raw(&self, now: i64) -> Option<Option<String>> {
        let guard = self
            .raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .filter(|cached| now < cached.expires_at)
            .map(|cached| cached.value.clone())
    }

    /// Remember a raw read (or its absence) for [`RAW_TTL_SECS`].
    pub fn remember_raw(&self, value: Option<String>, now: i64) {
        let mut guard = self
            .raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(RawCached {
            value,
            expires_at: now.saturating_add(RAW_TTL_SECS),
        });
    }

    /// Forget everything; the next [`load`] reads D1 again.
    pub fn invalidate(&self) {
        *self
            .raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Parse `raw` unless these exact bytes were seen before.
    pub fn get(&self, raw: &str) -> std::result::Result<Loaded, SiteError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = guard.as_ref() {
            if cached.raw == raw {
                return cached.result.clone();
            }
        }
        let result = parse_raw(raw);
        *guard = Some(Cached {
            raw: raw.to_owned(),
            result: result.clone(),
        });
        result
    }
}

/// Process-wide cache backing [`load`].
fn global() -> &'static SiteCache {
    static CACHE: OnceLock<SiteCache> = OnceLock::new();
    CACHE.get_or_init(SiteCache::new)
}

/// Parse plus validate one YAML document, deriving favicon origins.
fn parse_raw(raw: &str) -> std::result::Result<Loaded, SiteError> {
    let site: Site = serde_yaml::from_str(raw).map_err(|e| SiteError::Parse(e.to_string()))?;
    if let Err(errors) = site.check() {
        return Err(SiteError::Invalid(
            errors.iter().map(ToString::to_string).collect(),
        ));
    }
    let favicons = site
        .monitors
        .iter()
        .filter_map(|monitor| {
            crate::page::favicon_origin(monitor).map(|origin| (monitor.id.clone(), origin))
        })
        .collect();
    Ok(Loaded {
        site: Arc::new(site),
        favicons: Arc::new(favicons),
    })
}

#[cfg(test)]
mod tests {
    use super::SiteCache;
    use super::SiteError;
    use std::sync::Arc;

    const GOOD_YAML: &str = "title: T\nmonitors:\n  - id: m\n    name: M\n    check:\n      via: tcp\n      host: example.com\n      port: 80\n";

    #[test]
    fn cache_returns_same_site_without_reparsing() {
        let cache = SiteCache::new();
        let first = cache.get(GOOD_YAML).expect("valid yaml parses");
        let second = cache.get(GOOD_YAML).expect("valid yaml parses");
        assert!(Arc::ptr_eq(&first.site, &second.site));
        assert_eq!(first.site.monitors.len(), 1);
    }

    #[test]
    fn cache_reparses_when_raw_changes_and_recovers_after_invalid() {
        let cache = SiteCache::new();
        let first = cache.get(GOOD_YAML).expect("valid yaml parses");
        let other = cache
            .get(GOOD_YAML.replace("title: T", "title: Other").as_str())
            .expect("valid yaml parses");
        assert_eq!(other.site.title, "Other");
        assert!(!Arc::ptr_eq(&first.site, &other.site));
        // Invalid YAML surfaces (and is remembered) instead of serving stale.
        let bad = cache.get("title: [unclosed").expect_err("must fail");
        assert!(matches!(bad, SiteError::Parse(_)));
        let bad_again = cache.get("title: [unclosed").expect_err("must fail");
        assert_eq!(bad.to_string(), bad_again.to_string());
        // Parses but fails validation: Invalid, not Parse.
        let empty = cache
            .get("title: Empty\nmonitors: []\n")
            .expect_err("must fail");
        assert!(matches!(empty, SiteError::Invalid(_)));
        // Back to valid: parses again instead of poisoned by the failure.
        let recovered = cache.get(GOOD_YAML).expect("valid yaml parses");
        assert_eq!(recovered.site.title, "T");
    }

    #[test]
    fn cache_derives_favicon_origins_at_parse_time() {
        let cache = SiteCache::new();
        let yaml = "title: T\nmonitors:\n  - id: m\n    name: M\n    check:\n      via: http\n      url: https://example.com/some/page\n  - id: t\n    name: Tcp\n    check:\n      via: tcp\n      host: example.com\n      port: 80\n";
        let loaded = cache.get(yaml).expect("valid yaml parses");
        assert_eq!(
            loaded.favicons.get("m").map(String::as_str),
            Some("https://example.com")
        );
        assert!(!loaded.favicons.contains_key("t"));
        assert_eq!(loaded.site.monitors.len(), 2);
    }

    #[test]
    fn raw_cache_serves_within_ttl_and_expires() {
        let cache = SiteCache::new();
        assert_eq!(cache.cached_raw(100), None);
        cache.remember_raw(Some(GOOD_YAML.to_owned()), 100);
        assert_eq!(
            cache.cached_raw(100 + super::RAW_TTL_SECS - 1),
            Some(Some(GOOD_YAML.to_owned()))
        );
        assert_eq!(cache.cached_raw(100 + super::RAW_TTL_SECS), None);
    }

    #[test]
    fn raw_cache_remembers_missing_config_and_invalidates() {
        let cache = SiteCache::new();
        cache.remember_raw(None, 100);
        assert_eq!(cache.cached_raw(101), Some(None));
        cache.remember_raw(Some(GOOD_YAML.to_owned()), 101);
        assert_eq!(cache.cached_raw(102), Some(Some(GOOD_YAML.to_owned())));
        cache.invalidate();
        assert_eq!(cache.cached_raw(103), None);
    }
}
