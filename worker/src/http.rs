//! HTTP routing: auth gate, then dispatch to pages or JSON APIs.

use crate::api;
use crate::auth::{authed, deny};
use crate::db::Db;
use worker::{Env, Request, Response, Result};

/// Handle every incoming `fetch`: gate, migrate, then route by path.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
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
    db.init().await?;
    let mut resp = match url.path() {
        "/" | "/index.html" => crate::page::render(&db, &site).await,
        "/monitor" => match detail_id(&url) {
            Some(id) => crate::detail::render(&db, &site, &id).await,
            None => Response::error("missing id", 400),
        },
        "/api/health" => api::health(&db, &site).await,
        "/api/badge" => api::badge(&db, &site, &url).await,
        "/api/history" => api::history(&db, &url).await,
        "/api/incidents" => api::incidents(&db, &url).await,
        "/api/latency" => api::latency(&db, &url).await,
        _ => Response::error("not found", 404),
    }?;
    if let Some(policy) = cache_control(url.path()) {
        resp.headers_mut().set("cache-control", policy)?;
    }
    Ok(resp)
}

/// Extract `?id=` from a detail/API URL.
fn detail_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "id")
        .map(|(_, value)| value.into_owned())
}

/// Shared cache policy per path: tick-granular pages and read APIs tolerate
/// 30s of staleness (the auto-refresh polls on the same interval), which
/// caps D1 fan-out cost per visitor. Badge and icons set their own policy.
fn cache_control(path: &str) -> Option<&'static str> {
    match path {
        "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history" | "/api/incidents"
        | "/api/latency" => Some("public, max-age=30"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::cache_control;

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
                cache_control(path),
                Some("public, max-age=30"),
                "{path} must be cheap to serve"
            );
        }
    }

    #[test]
    fn badge_and_errors_stay_uncached() {
        assert_eq!(cache_control("/api/badge"), None);
        assert_eq!(cache_control("/nope"), None);
    }
}
