//! Vigil site config parsing (runtime, D1-backed).
//!
//! The YAML lives in the D1 `kv` table (key `site_yaml`), edited through
//! the worker's config editor (deployed with `EDITOR=true`). It is parsed
//! here on every request/tick — no config is compiled into the Worker.
//! Keep monitor `id`s stable: D1 history keys off them. Secrets don't
//! belong in the YAML: reference worker env vars by name (`pass_env`).

use vigil_core::config::Site;

/// D1 `kv` key holding the raw site YAML.
pub const SITE_KV_KEY: &str = "site_yaml";

/// Parse raw YAML into a [`Site`], returning a human-readable error.
pub fn parse(yaml: &str) -> Result<Site, String> {
    serde_yaml::from_str(yaml).map_err(|e| e.to_string())
}

/// Parse and validate: YAML syntax plus [`Site::check`].
pub fn parse_checked(yaml: &str) -> Result<Site, Vec<String>> {
    let site = parse(yaml).map_err(|e| vec![e])?;
    site.check()
        .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    Ok(site)
}
