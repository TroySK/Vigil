//! HTTP routing: auth gate, then dispatch to pages or JSON APIs.

use crate::api;
use crate::auth::{authed, deny};
use crate::db::Db;
use worker::{Env, Request, Response, Result};

/// Handle every incoming `fetch`: gate, migrate, route, then harden and
/// cache-tag the response on the single exit path.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    // Borrow-only gate check: `site()` clones the whole config per call,
    // pure allocation overhead for a boolean (and brand assets below never
    // load the config at all — `route` returns before `site()`).
    let gated = vigil::gated();
    let mut resp = route(req, env, &url).await?;
    for (name, value) in security_headers() {
        resp.headers_mut().set(name, value)?;
    }
    if gated {
        resp.headers_mut().set("vary", "Authorization")?;
    }
    if let Some(policy) = cache_control(url.path(), gated) {
        resp.headers_mut().set("cache-control", policy)?;
    }
    Ok(resp)
}

/// Gate, migrate, then route by path.
async fn route(req: Request, env: Env, url: &url::Url) -> Result<Response> {
    // Brand assets stay public so browsers can fetch the tab icon and the
    // header logo without credentials.
    match url.path() {
        "/favicon.ico" => return crate::icons::favicon(),
        "/logo.svg" => return crate::icons::logo(),
        _ => {}
    }
    let site = vigil::site();
    if !authed(&req, &site, &env) {
        return deny();
    }
    let db = Db::of(&env)?;
    // No schema setup here: DDL on the request path fired an idempotent
    // migration on every request landing in the hourly window (a thundering
    // herd under traffic). CI migrations plus the cron tick's self-heal own
    // schema; requests just read.
    match url.path() {
        "/" | "/index.html" => {
            let range = crate::range::parse_range(range_param(url).as_deref());
            crate::page::render(&db, &site, range).await
        }
        "/monitor" => match detail_id(url) {
            Some(id) => {
                let range = crate::range::parse_range(range_param(url).as_deref());
                crate::detail::render(&db, &site, &id, range).await
            }
            None => Response::error("missing id", 400),
        },
        "/api/health" => api::health(&db, &site).await,
        "/api/badge" => api::badge(&db, &site, url).await,
        "/api/history" => api::history(&db, url).await,
        "/api/incidents" => api::incidents(&db, url).await,
        "/api/latency" => api::latency(&db, url).await,
        _ => Response::error("not found", 404),
    }
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

/// Shared cache policy per path: tick-granular pages and read APIs tolerate
/// 30s of staleness (the auto-refresh polls on the same interval), which
/// caps D1 fan-out cost per visitor. `s-maxage` + `stale-while-revalidate`
/// let the Cloudflare edge absorb the tail (D1 variance is the P90) while
/// browsers still refresh promptly; icons set their own immutable policy
/// and the badge (polled by external embeds) tolerates 60s. When the auth
/// gate is on (`gated`), responses are `private` so a shared cache never
/// serves one visitor's page to another (see the `Vary` in `handle`).
fn cache_control(path: &str, gated: bool) -> Option<&'static str> {
    if gated {
        return match path {
            "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history" | "/api/incidents"
            | "/api/latency" => Some("private, max-age=30"),
            "/api/badge" => Some("private, max-age=60"),
            _ => None,
        };
    }
    match path {
        "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history" | "/api/incidents"
        | "/api/latency" => Some("public, max-age=30, s-maxage=30, stale-while-revalidate=60"),
        "/api/badge" => Some("public, max-age=60, s-maxage=60, stale-while-revalidate=120"),
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
    use super::{cache_control, security_headers};

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
    fn tick_granular_routes_share_short_cache() {
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
                Some("public, max-age=30, s-maxage=30, stale-while-revalidate=60"),
                "{path} must be cheap to serve"
            );
        }
    }

    #[test]
    fn badge_caches_briefly_and_errors_stay_uncached() {
        // Badge embeds poll constantly; 60s of staleness caps D1 reads.
        assert_eq!(
            cache_control("/api/badge", false),
            Some("public, max-age=60, s-maxage=60, stale-while-revalidate=120")
        );
        assert_eq!(cache_control("/nope", false), None);
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
