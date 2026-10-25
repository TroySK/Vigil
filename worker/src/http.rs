//! HTTP routing: auth gate, then dispatch to pages or JSON APIs.

use crate::api;
use crate::auth::{authed, deny};
use crate::db::Db;
use worker::{console_log, Env, Request, Response, Result};

/// Handle every incoming `fetch`: editor mode, auth gate, migrate, route,
/// then harden and cache-tag the response on the single exit path.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let editor = crate::site_store::is_editor(&env);
    // Editor mode short-circuits everything (except brand assets): the
    // status site is offline until `EDITOR=true` is removed + redeployed.
    // A propagated runtime error would otherwise bypass the header loop
    // below, so turn it into a generic, hardened 500 first.
    let (mut resp, gated) = match route(req, env, &url, editor).await {
        Ok(result) => result,
        Err(error) => {
            // The body stays generic; the log is the only way `wrangler tail`
            // can tell a D1 outage from a config or serialization bug.
            console_log!("route {} failed: {error}", url.path());
            (Response::error("internal error", 500)?, false)
        }
    };
    for (name, value) in security_headers() {
        resp.headers_mut().set(name, value)?;
    }
    if editor && !is_static_asset(url.path()) {
        // Never cache or index editor pages (favicon/logo keep their
        // handler-set immutable cache; style.css/app.js use a short
        // revalidating cache — see icons.rs — even in editor mode).
        resp.headers_mut().set("cache-control", "no-store")?;
        resp.headers_mut()
            .set("x-robots-tag", "noindex, nofollow")?;
        return Ok(resp);
    }
    if gated {
        resp.headers_mut().set("vary", "Authorization")?;
    }
    // Error pages (notably the missing-config 500) must never sit in a
    // shared cache: the next deploy or save may resolve them. 401s must
    // not be replayed by a shared browser profile either.
    if never_cache(resp.status_code()) {
        resp.headers_mut().set("cache-control", "no-store")?;
    } else if let Some(policy) = cache_control(url.path(), gated) {
        resp.headers_mut().set("cache-control", policy)?;
    }
    Ok(resp)
}

/// `true` for statuses that must never be stored by any cache.
fn never_cache(status: u16) -> bool {
    status >= 500 || status == 401
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
    // (pages link them, including the editor). The validator
    // lets a revalidating cache skip the body after a deploy.
    let if_none_match = req.headers().get("if-none-match").ok().flatten();
    match url.path() {
        "/favicon.ico" => return Ok((crate::icons::favicon(if_none_match.as_deref())?, false)),
        "/logo.svg" => return Ok((crate::icons::logo(if_none_match.as_deref())?, false)),
        "/style.css" => return Ok((crate::icons::style(if_none_match.as_deref())?, false)),
        "/app.js" => return Ok((crate::icons::app_js(if_none_match.as_deref())?, false)),
        _ => {}
    }
    let db = Db::of(&env)?;
    if editor {
        // Editor mode: the status site is offline. Access is gated by the
        // EDITOR flag alone; saves additionally require same-origin
        // browser metadata (see editor::permitted).
        // Only POST /editor saves: accepting a save on every path lets a
        // stray POST (pinger, prefetch) with a YAML-shaped body overwrite
        // the site config.
        if !crate::editor::permitted(&req, url) {
            return Ok((crate::editor::deny()?, false));
        }
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
    // Path-scoped query allowlist: unknown or duplicated keys would make
    // every variant a distinct cache entry (and force a fresh D1 read).
    let allowed: Option<&[&str]> = match url.path() {
        "/" | "/index.html" => Some(&["range"]),
        "/monitor" => Some(&["id", "range"]),
        "/api/health" => Some(&[]),
        "/api/badge" | "/api/history" | "/api/latency" => Some(&["id"]),
        "/api/incidents" => Some(&["id", "limit"]),
        _ => None,
    };
    if allowed.is_some_and(|allowed| !api::query_ok(url, allowed)) {
        return Ok((Response::error("unknown query parameter", 400)?, gated));
    }
    // Unknown range values are a 400, not a silent default: rendering and
    // caching arbitrary `?range=` spellings would multiply D1 work.
    let range = if matches!(url.path(), "/" | "/index.html" | "/monitor") {
        match range_param(url) {
            Some(raw) => match crate::range::exact_range(&raw) {
                Some(range) => range,
                None => return Ok((Response::error("bad range", 400)?, gated)),
            },
            None => crate::range::DEFAULT_RANGE,
        }
    } else {
        crate::range::DEFAULT_RANGE
    };
    let resp = match url.path() {
        "/" | "/index.html" => crate::page::render(&db, site, &loaded.favicons, range).await?,
        "/monitor" => match detail_id(url) {
            Some(id) => crate::detail::render(&db, site, &loaded.favicons, &id, range).await?,
            None => Response::error("missing id", 400)?,
        },
        "/api/health" => api::health(&db, site).await?,
        "/api/badge" => api::badge(&db, site, url).await?,
        "/api/history" => api::history(&db, site, url).await?,
        "/api/incidents" => api::incidents(&db, site, url).await?,
        "/api/latency" => api::latency(&db, site, url).await?,
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

/// Shared cache policy per path. Pages are cached for one minute (and may
/// serve 30 s stale) so a reload never stacks a full cache window on top of
/// the poll interval; `/api/health` is the poll itself, so it gets the
/// shortest TTL. History APIs change only when a sample lands (hourly) and
/// keep the five-minute edge cache. Icons set their own immutable policy.
/// `stale-while-revalidate` lets the Cloudflare edge absorb the tail (D1
/// variance is the P90) while browsers still refresh promptly. Do NOT add
/// `s-maxage`/`must-revalidate`: Cloudflare documents both as disabling
/// stale-while-revalidate. When the auth gate is on (`gated`), responses
/// are `private, no-store` so a shared cache never serves one visitor's
/// page to another (see the `Vary` in `handle`).
fn cache_control(path: &str, gated: bool) -> Option<&'static str> {
    if gated {
        return match path {
            "/" | "/index.html" | "/monitor" | "/api/health" | "/api/history" | "/api/badge"
            | "/api/incidents" | "/api/latency" => Some("private, no-store"),
            _ => None,
        };
    }
    match path {
        "/" | "/index.html" | "/monitor" => Some("public, max-age=60, stale-while-revalidate=30"),
        "/api/health" => Some("public, max-age=30"),
        "/api/history" | "/api/badge" | "/api/incidents" | "/api/latency" => {
            Some("public, max-age=300, stale-while-revalidate=60")
        }
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
    use super::{cache_control, is_static_asset, never_cache, security_headers};

    #[test]
    fn routing_failures_and_challenges_are_never_cached() {
        assert!(never_cache(500));
        assert!(never_cache(503));
        assert!(never_cache(401));
        assert!(!never_cache(200));
        assert!(!never_cache(404));
    }

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
    fn pages_revalidate_faster_than_the_poll_interval() {
        // A page cached for the full 5-minute poll interval can serve data
        // that is already one probe cycle old on reload. Keep page caching
        // short enough that a reload never adds a second stale window.
        for path in ["/", "/index.html", "/monitor"] {
            assert_eq!(
                cache_control(path, false),
                Some("public, max-age=60, stale-while-revalidate=30"),
                "{path} must revalidate within a poll cycle"
            );
        }
    }

    #[test]
    fn health_is_cached_only_briefly() {
        // The poller calls this every refresh; a 5-minute cache stacked on
        // a 5-minute poll (and a 5-minute page cache) hid transitions for
        // up to 15 minutes. Health must never outlive a poll cycle.
        assert_eq!(
            cache_control("/api/health", false),
            Some("public, max-age=30")
        );
    }

    #[test]
    fn history_apis_keep_the_five_minute_cache() {
        for path in [
            "/api/history",
            "/api/incidents",
            "/api/latency",
            "/api/badge",
        ] {
            assert_eq!(
                cache_control(path, false),
                Some("public, max-age=300, stale-while-revalidate=60"),
                "{path} is cheap to serve and tolerates a probe cycle"
            );
        }
    }

    #[test]
    fn public_cache_policy_keeps_stale_while_revalidate_alive() {
        // Cloudflare documents that `s-maxage` (and must-revalidate)
        // disables stale-while-revalidate. The public policy must not
        // re-add it, or the edge can only serve fresh-or-block.
        for path in ["/", "/api/badge"] {
            let policy = cache_control(path, false).expect("public route is cached");
            assert!(policy.starts_with("public, "), "{path}: {policy}");
            assert!(
                policy.contains("stale-while-revalidate="),
                "{path} must allow SWR: {policy}"
            );
            assert!(!policy.contains("s-maxage"), "{path} must not set s-maxage");
        }
    }

    #[test]
    fn wrangler_enables_workers_caching() {
        // `Cache-Control` alone only reaches browsers; Workers Caching
        // needs an explicit cache block before the edge consults it.
        const WRANGLER: &str = include_str!("../wrangler.toml");
        assert!(WRANGLER.contains("[cache]"), "wrangler.toml needs [cache]");
        assert!(
            WRANGLER.lines().any(|line| line.trim() == "enabled = true"),
            "wrangler.toml needs cache.enabled = true"
        );
    }

    #[test]
    fn wrangler_build_pins_worker_build() {
        // Deploys must not pick up a new worker-build release implicitly;
        // pinning keeps local and CI builds identical and cacheable.
        const WRANGLER: &str = include_str!("../wrangler.toml");
        assert!(
            WRANGLER.contains("cargo install -q worker-build --version 0.8.7 --locked"),
            "wrangler.toml must pin worker-build"
        );
    }

    #[test]
    fn deploy_workflow_pins_wrangler() {
        // CI shells out to Wrangler for every deploy step; a floating major
        // would silently change CLI behavior between deploys.
        const WORKFLOW: &str = include_str!("../../.github/workflows/deploy.yml");
        assert!(
            WORKFLOW.contains("WRANGLER_VERSION: \"4.151.3\""),
            "deploy workflow must pin Wrangler"
        );
        assert!(
            !WORKFLOW.contains("wrangler@4 "),
            "no step may use a floating Wrangler major"
        );
    }

    #[test]
    fn deploy_workflow_handles_password_lifecycle() {
        const WORKFLOW: &str = include_str!("../../.github/workflows/deploy.yml");
        assert!(
            WORKFLOW.contains("printf '%s' \"$STATUS_PASSWORD\""),
            "passwords must not be interpreted by echo"
        );
        assert!(
            WORKFLOW.contains("secret delete STATUS_PASSWORD"),
            "removing the repo secret must delete the worker secret"
        );
        assert!(
            WORKFLOW.contains("d1 execute DB --remote --json"),
            "deploy must inspect the saved auth config before deploying"
        );
    }

    #[test]
    fn deploy_workflow_pins_actions_and_limits_tokens() {
        const WORKFLOW: &str = include_str!("../../.github/workflows/deploy.yml");
        assert!(
            WORKFLOW.contains("permissions:\n  contents: read"),
            "workflow must default to a read-only GITHUB_TOKEN"
        );
        assert!(
            WORKFLOW.contains("persist-credentials: false"),
            "checkout must not leave the token in the git config"
        );
        for action in [
            "actions/checkout",
            "actions/setup-node",
            "Swatinem/rust-cache",
            "dtolnay/rust-toolchain",
        ] {
            let prefix = format!("uses: {action}@");
            let line = WORKFLOW
                .lines()
                .find(|line| line.contains(&prefix))
                .unwrap_or_else(|| panic!("{action} missing"));
            let sha = line
                .split('@')
                .nth(1)
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default();
            assert_eq!(
                sha.len(),
                40,
                "{action} must be pinned to a full SHA: {line}"
            );
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{action} must be pinned to a commit: {line}"
            );
        }
        const IGNORE: &str = include_str!("../../.gitignore");
        assert!(IGNORE.contains(".dev.vars*"), "wrangler secrets are local");
        assert!(
            IGNORE.contains(".env*"),
            "local env files must never commit"
        );
        assert!(
            WORKFLOW.contains("run: cargo audit"),
            "dependencies need an advisory check"
        );
    }

    #[test]
    fn badge_caches_briefly_and_errors_stay_uncached() {
        // Badge embeds poll constantly; 5 minutes of staleness caps D1 reads
        // while staying fresh with the probe cadence.
        assert_eq!(
            cache_control("/api/badge", false),
            Some("public, max-age=300, stale-while-revalidate=60")
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
        // edge cache must not serve one visitor's page to another, and a
        // shared browser profile must not replay gated data.
        for path in ["/", "/monitor", "/api/health", "/api/badge"] {
            let policy = cache_control(path, true).expect("{path} stays uncached");
            assert!(
                policy.starts_with("private,"),
                "{path} gated policy must be private, got {policy}"
            );
            assert!(
                policy.contains("no-store"),
                "{path} gated policy must not be stored, got {policy}"
            );
            assert!(!policy.contains("s-maxage"), "{path} must not set s-maxage");
        }
        assert_eq!(cache_control("/nope", true), None);
    }
}
