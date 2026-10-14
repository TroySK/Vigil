//! Vigil site config, loaded from [`site.yaml`](crate::SITE_YAML) at build time.
//!
//! Edit `config/site.yaml` to define monitors; keep ids stable — D1 history
//! keys off them. Secrets don't belong in the file: reference worker env
//! vars by name instead (see `pass_env`).

use std::sync::OnceLock;
use vigil_core::config::Site;

/// Raw `site.yaml` source, embedded at compile time.
pub const SITE_YAML: &str = include_str!("../site.yaml");

/// Parsed site config, cached after the first call.
static SITE: OnceLock<Site> = OnceLock::new();

/// Site config parsed from `site.yaml`.
///
/// Panics at first use when the embedded YAML is invalid — that is a
/// build-time authoring error, so failing fast beats serving half a config.
#[must_use]
pub fn site() -> Site {
    get().clone()
}

/// Whether the auth gate is configured, borrowing the cached config.
///
/// Hot paths (every `fetch`, including brand assets) check this without
/// cloning the whole `Site`: `site()` clones monitors, links, and upkeep
/// on each call, which is pure allocation overhead for a boolean answer.
#[must_use]
pub fn gated() -> bool {
    get().auth.is_some()
}

/// Parsed site config, shared by [`site`] and [`gated`].
fn get() -> &'static Site {
    SITE.get_or_init(|| {
        serde_yaml::from_str(SITE_YAML).expect("config/site.yaml must parse as a Site")
    })
}
