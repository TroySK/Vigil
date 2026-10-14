//! Site config loading: raw YAML from D1 `kv`, parsed at request time.
//!
//! The YAML is stored under [`vigil::SITE_KV_KEY`] by the config editor
//! (see [`crate::editor`]). Nothing is compiled in: a missing key means
//! the operator has not seeded the config yet.

use crate::db::Db;
use vigil_core::config::Site;
use worker::{Env, Result};

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
#[derive(Debug)]
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

/// Load and validate the site config from D1.
pub async fn load(db: &Db) -> Result<std::result::Result<Site, SiteError>> {
    let Some(raw) = load_raw(db).await? else {
        return Ok(Err(SiteError::Missing));
    };
    let site: Site = match serde_yaml::from_str(&raw) {
        Ok(site) => site,
        Err(e) => return Ok(Err(SiteError::Parse(e.to_string()))),
    };
    if let Err(errors) = site.check() {
        return Ok(Err(SiteError::Invalid(
            errors.iter().map(ToString::to_string).collect(),
        )));
    }
    Ok(Ok(site))
}
