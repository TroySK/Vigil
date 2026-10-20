//! HTTP routing: auth gate, then dispatch to pages or JSON APIs.

use crate::api;
use crate::auth::{authed, deny};
use crate::db::Db;
use worker::{Env, Request, Response, Result};

/// Handle every incoming `fetch`: editor mode, auth gate, migrate, route,
/// then harden and cache-tag the response on the single exit path.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let editor = crate::site_store::is_editor(&env);
    // Editor mode short-circuits everything (except brand assets): the
    // status site is offline until `EDITOR=true` is removed + redeployed.
    let (mut resp, gated) = route(req, env, &url, editor).await?;
    for (name, value) in security_headers() {
        resp.headers_mut().set(name, value)?;
    }
    if editor && !is_static_asset(url.path()) {
        // Never cache the unauthenticated editor pages (favicon/logo keep
        // their handler-set immutable cache; style.css/app.js use a short
        // revalidating cache — see icons.rs — even in editor mode).
        resp.headers_mut().set("cache-control", "no-store")?;
        return Ok(resp);
    }
    if gated {
        resp.headers_mut().set("vary", "Authorization")?;
    }
    // Error pages (notably the missing-config 500) must never sit in a
    // shared cache: the next deploy or save may resolve them.
    if resp.status_code() >= 500 {
        resp.headers_mut().set("cache-control", "no-store")?;
    } else if let Some(policy) = cache_control(url.path(), gated) {
        resp.headers_mut().set("cache-control", policy)?;
    }
    Ok(resp)
}

/// Gate, migrate, then route by path. Returns the response plus whether
/// the auth gate is on (for `Vary` / private-cache handling in [`handle`]).
async fn route(
    mut req: Request,
    env: Env,
    url: &url::Url,
    editor: bool,
) -> Result<(Response, bool)> {
    // Brand assets stay public so browsers can fetch the tab icon, the
    // header logo, and the shared stylesheet/script without credentials
    // (pages link them, including the unauthenticated editor).
    match url.path() {
        "/favicon.ico" => return Ok((crate::icons::favicon()?, false)),
        "/logo.svg" => return Ok((crate::icons::logo()?, false)),
        "/style.css" => return Ok((crate::icons::style()?, false)),
        "/app.js" => return Ok((crate::icons::app_js()?, false)),
        _ => {}
    }
    let db = Db::of(&env)?;
    if editor {
        // Editor mode: the status site is offline. Every route renders the
        // editor; saving validates before persisting. No auth gate here —
        // there may be no valid config to gate against yet — so keep this
        // window short (the page and README say so).
        // Only POST /editor saves: accepting a save on every path lets a
        // stray POST (pinger, prefetch) with a YAML-shaped body overwrite
        // the site config.
        if req.method() == worker::Method::Post && url.path() == "/editor" {
            return Ok((crate::editor::save(&mut req, &db).await?, false));
        }
        return Ok((crate::editor::render(&db, None, &[], false).await?, false));
    }
    if url.path() == "/editor" {
        // The editor exists only in editor mode.
        return Ok((Response::error("not found", 404)?, false));
    }
    let loaded = match crate::site_store::load(&db).await? {
        Ok(loaded) => loaded,
        Err(crate::site_store::SiteError::Missing) => {
            return Ok((
                crate::editor::missing_config_page(
                    "No config has been saved yet. Fresh database.",
                )?,
                false,
            ));
        }
        Err(crate::site_store::SiteError::Parse(error)) => {
            return Ok((
                crate::editor::missing_config_page(&format!("Saved YAML fails to parse: {error}"))?,
                false,
            ));
        }
        Err(crate::site_store::SiteError::Invalid(errors)) => {
            return Ok((
                crate::editor::missing_config_page(&format!(
                    "Saved config is invalid: {}",
                    errors.join("; ")
                ))?,
                false,
            ));
        }
    };
    let site = &loaded.site;
    if !authed(&req, site, &env) {
        return Ok((deny()?, site.auth.is_some()));
    }
    // No schema setup here: DDL on the request path fired an idempotent
    // migration on every request landing in the hourly window (a thundering
    // herd under traffic). CI migrations plus the cron tick's self-heal own
    // schema; requests just read.
    let gated = site.auth.is_some();
    let resp = match url.path() {
        "/" | "/index.html" => {
            let range = crate::range::parse_range(range_param(url).as_deref());
            crate::page::render(&db, site, &loaded.favicons, range).await?
        }
        "/monitor" => match detail_id(url) {
            Some(id) => {
                let range = crate::range::parse_range(range_param(url).as_deref());
                crate::detail::render(&db, site, &loaded.favicons, &id, range).await?
            }
            None => Response::error("missing id", 400)?,
        },
        "/api/health" => api::health(&db, site).await?,
        "/api/badge" => api::badge(&db, site, url).await?,
        "/api/history" => api::history(&db, url).await?,
        "/api/incidents" => api::incidents(&db, url).await?,
        "/api/latency" => api::latency(&db, url).await?,
        _ => Response::error("not found", 404)?,
    };
    Ok((resp, gated))
}

/// Long-lived public assets: favicon/logo served with handler-set
/// immutable caches that survive even editor-mode `no-store` (see [`handle`]).
/// style.css/app.js use short revalidating caches (unversioned URLs).
fn is_static_asset(path: &str) -> bool {
    matches!(
        path,
        "/favicon.ico" | "/logo.svg" | "/style.css" | "/app.js"
    )
}

/// Extract `?id=` from a detail/API URL.
fn detail_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "id")
        .map(|(_, value)| value.into_owned())
}

/// Extract `?range=` from a detail URL.
fn range_param(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "range")
        .map(|(_, value)| value.into_owned())
}

/// Shared cache policy per path: probes run every 5 minutes but latency
/// charts stay hourly, so pages and read APIs tolerate 5 minutes of
/// staleness (the auto-refresh polls on the same interval), which caps
/// D1 fan-out cost per visitor. `s-maxage` + `stale-while-revalidate`
/// let the Cloudflare edge absorb the tail (D1 variance is the P90)
/// while browsers still refresh promptly; icons set their own immutable
/// policy and the badge (polled by external embeds) tolerates 5 minutes.
/// When the auth gate is on (`gated`), responses are `private` so a
/// shared cache never serves one visitor's page to another (see the
/// `Vary` in `handle`).
fn cache_control(path: &str, gated: bool) -> Option<&'static str> {
    if gated {
        return match path {
            "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history"
            | "/api/incidents" | "/api/latency" => Some("private, max-age=300"),
            "/api/badge" => Some("private, max-age=300"),
            _ => None,
        };
    }
    match path {
        "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history" | "/api/incidents"
        | "/api/latency" => Some("public, max-age=300, s-maxage=300, stale-while-revalidate=60"),
        "/api/badge" => Some("public, max-age=300, s-maxage=300, stale-while-revalidate=60"),
        _ => None,
    }
}

/// Baseline hardening headers applied to every response leaving `handle`.
/// No CSP here: scripts and styles are inline, so a strict policy would
/// need a nonce refactor first.
fn security_headers() -> [(&'static str, &'static str); 3] {
    [
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ]
}

#[cfg(test)]
mod tests {
    use super::{cache_control, is_static_asset, security_headers};

    #[test]
    fn every_response_carries_hardening_headers() {
        let headers = security_headers();
        let get = |name: &str| headers.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        // No referrer leaks to third-party favicon hosts.
        assert_eq!(get("referrer-policy"), Some("no-referrer"));
        // No MIME sniffing of API/JSON bodies.
        assert_eq!(get("x-content-type-options"), Some("nosniff"));
        // Status pages embed as data, never as framed documents.
        assert_eq!(get("x-frame-options"), Some("DENY"));
    }

    #[test]
    fn hourly_granular_routes_share_five_minute_cache() {
        for path in [
            "/",
            "/index.html",
            "/monitor",
            "/api/health",
            "/api/history",
            "/api/incidents",
            "/api/latency",
        ] {
            assert_eq!(
                cache_control(path, false),
                Some("public, max-age=300, s-maxage=300, stale-while-revalidate=60"),
                "{path} must be cheap to serve"
            );
        }
    }

    #[test]
    fn badge_caches_briefly_and_errors_stay_uncached() {
        // Badge embeds poll constantly; 5 minutes of staleness caps D1 reads
        // while staying fresh with the probe cadence.
        assert_eq!(
            cache_control("/api/badge", false),
            Some("public, max-age=300, s-maxage=300, stale-while-revalidate=60")
        );
        assert_eq!(cache_control("/nope", false), None);
    }

    #[test]
    fn static_assets_keep_handler_cache_policy() {
        // /style.css + /app.js set their own immutable cache in icons.rs;
        // the shared policy must stay out of the way (None = untouched).
        // `is_static_asset` gates the editor-mode no-store override.
        for path in ["/favicon.ico", "/logo.svg", "/style.css", "/app.js"] {
            assert_eq!(cache_control(path, false), None);
            assert_eq!(cache_control(path, true), None);
            assert!(is_static_asset(path), "{path} must be static");
        }
        assert!(!is_static_asset("/"));
        assert!(!is_static_asset("/editor"));
    }

    #[test]
    fn gated_sites_never_share_edge_cache() {
        // With an auth gate, responses vary by credentials: a shared
        // edge cache must not serve one visitor's page to another.
        for path in ["/", "/monitor", "/api/health", "/api/badge"] {
            let policy = cache_control(path, true).expect("{path} stays cached");
            assert!(
                policy.starts_with("private,"),
                "{path} gated policy must be private, got {policy}"
            );
            assert!(!policy.contains("s-maxage"), "{path} must not set s-maxage");
        }
        assert_eq!(cache_control("/nope", true), None);
    }
}
