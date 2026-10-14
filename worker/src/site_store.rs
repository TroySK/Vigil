//! Site config loading: raw YAML from D1 `kv`, parsed at request time.
//!
//! The YAML is stored under [`vigil::SITE_KV_KEY`] by the config editor
//! (see [`crate::editor`]). Nothing is compiled in: a missing key means
//! the operator has not seeded the config yet.

use crate::db::Db;
use vigil_core::config::Site;
use worker::{Env, Result};

use std::sync::{Arc, Mutex, OnceLock};

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
/// YAML parsing plus [`Site::check`] runs only when the stored bytes
/// change: isolates are reused across requests/ticks, so re-parsing an
/// unchanged multi-KB config every 30s poll would burn the Worker's CPU
/// budget for no new information. Failures are memoized too — a bad save
/// keeps reporting its error instead of serving stale config.
pub async fn load(db: &Db) -> Result<std::result::Result<Arc<Site>, SiteError>> {
    let Some(raw) = load_raw(db).await? else {
        return Ok(Err(SiteError::Missing));
    };
    Ok(global().get(&raw))
}

/// One memoized parse: the raw bytes plus their outcome.
#[derive(Debug)]
struct Cached {
    raw: String,
    result: std::result::Result<Arc<Site>, SiteError>,
}

/// Byte-keyed memo for [`load`]: cheap to consult on every request, exact
/// on every save. A dedicated struct (rather than bare statics) keeps the
/// policy unit-testable without global state.
#[derive(Debug, Default)]
pub struct SiteCache {
    inner: Mutex<Option<Cached>>,
}

impl SiteCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `raw` unless these exact bytes were seen before.
    pub fn get(&self, raw: &str) -> std::result::Result<Arc<Site>, SiteError> {
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

/// Parse plus validate one YAML document.
fn parse_raw(raw: &str) -> std::result::Result<Arc<Site>, SiteError> {
    let site: Site = serde_yaml::from_str(raw).map_err(|e| SiteError::Parse(e.to_string()))?;
    if let Err(errors) = site.check() {
        return Err(SiteError::Invalid(
            errors.iter().map(ToString::to_string).collect(),
        ));
    }
    Ok(Arc::new(site))
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
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.monitors.len(), 1);
    }

    #[test]
    fn cache_reparses_when_raw_changes_and_recovers_after_invalid() {
        let cache = SiteCache::new();
        let first = cache.get(GOOD_YAML).expect("valid yaml parses");
        let other = cache
            .get(GOOD_YAML.replace("title: T", "title: Other").as_str())
            .expect("valid yaml parses");
        assert_eq!(other.title, "Other");
        assert!(!Arc::ptr_eq(&first, &other));
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
        assert_eq!(recovered.title, "T");
    }
}
