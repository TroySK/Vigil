//! Embedded brand assets: favicon + logo served without static hosting.
//!
//! The ICO / SVG bytes are baked into the WASM bundle with `include_bytes!`
//! so `/favicon.ico` works on Workers with no extra static-asset setup.

use worker::{Response, ResponseBody, Result};

/// Multi-size ICO generated from `vigil.svg` (`worker/assets/favicon.ico`).
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");
/// Source logo (`worker/assets/vigil.svg`).
const LOGO_SVG: &[u8] = include_bytes!("../assets/vigil.svg");

/// Serve `/favicon.ico` with a long immutable cache.
pub fn favicon(if_none_match: Option<&str>) -> Result<Response> {
    asset(
        "image/x-icon",
        "public, max-age=604800, immutable",
        FAVICON_ICO,
        if_none_match,
    )
}

/// Serve `/logo.svg` for the header brand mark.
pub fn logo(if_none_match: Option<&str>) -> Result<Response> {
    asset(
        "image/svg+xml",
        "public, max-age=604800, immutable",
        LOGO_SVG,
        if_none_match,
    )
}

/// App stylesheet, extracted from the page shell so HTML responses stop
/// carrying ~25KB of inline CSS: browsers + edge cache it immutably.
pub const STYLE_CSS: &str = include_str!("../assets/style.css");

/// App script, extracted from the page shell for the same reason. Reads
/// the per-page poll interval from the inline `window.vigilRefresh` set
/// by [`crate::page::shell`].
pub const APP_JS: &str = include_str!("../assets/app.js");

/// Serve `/style.css` with a short revalidating cache (NOT immutable:
/// the URL is unversioned, so a week-long immutable pin would keep old
/// CSS/JS after a deploy while HTML is fresh).
pub fn style(if_none_match: Option<&str>) -> Result<Response> {
    asset(
        "text/css;charset=UTF-8",
        "public, max-age=300, must-revalidate",
        STYLE_CSS.as_bytes(),
        if_none_match,
    )
}

/// Serve `/app.js` with a short revalidating cache (see [`style`]).
pub fn app_js(if_none_match: Option<&str>) -> Result<Response> {
    asset(
        "text/javascript;charset=UTF-8",
        "public, max-age=300, must-revalidate",
        APP_JS.as_bytes(),
        if_none_match,
    )
}

/// Shared asset response: a matching `If-None-Match` gets a bodyless 304
/// (the browser reuses its copy), otherwise the bytes plus a strong `ETag`.
/// Unversioned URLs make the validator the deploy-safe half of the cache
/// policy: `must-revalidate` still costs a request, but no longer 30KB.
fn asset(
    content_type: &str,
    cache_control: &str,
    body: &[u8],
    if_none_match: Option<&str>,
) -> Result<Response> {
    let tag = etag(body);
    if not_modified(if_none_match, &tag) {
        return Ok(Response::builder()
            .with_status(304)
            .with_header("etag", &tag)?
            .with_header("cache-control", cache_control)?
            .empty());
    }
    Ok(Response::builder()
        .with_header("content-type", content_type)?
        .with_header("cache-control", cache_control)?
        .with_header("etag", &tag)?
        .body(ResponseBody::Body(body.to_vec())))
}

/// Strong validator for an embedded asset: FNV-1a over the shipped bytes,
/// stable across isolates and deploys until the asset changes.
fn etag(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{hash:016x}\"")
}

/// Parse an `If-None-Match` header. Accepts a comma-separated list, weak
/// validators (`W/"..."`), and `*`.
fn not_modified(header: Option<&str>, etag: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*"
            || candidate == etag
            || candidate.strip_prefix("W/") == Some(etag)
    })
}

#[cfg(test)]
mod tests {
    use super::{etag, not_modified, APP_JS, STYLE_CSS};

    #[test]
    fn etag_matches_only_the_same_asset() {
        let css = etag(STYLE_CSS.as_bytes());
        let js = etag(APP_JS.as_bytes());
        assert!(css.starts_with('"') && css.ends_with('"'));
        assert_ne!(css, js);
        assert_eq!(css, etag(STYLE_CSS.as_bytes()), "etag is deterministic");
        assert!(not_modified(Some(&css), &css));
        assert!(not_modified(Some(&format!("W/{css}")), &css));
        assert!(not_modified(Some(&format!("\"other\", {css}")), &css));
        assert!(not_modified(Some("*"), &css));
        assert!(!not_modified(Some(&js), &css));
        assert!(!not_modified(None, &css));
    }

    #[test]
    fn style_asset_is_plain_cacheable_css() {
        assert!(STYLE_CSS.contains(":root{"));
        assert!(STYLE_CSS.contains(".hero-stat"));
        assert!(!STYLE_CSS.contains("{{"), "no format escapes may ship");
        assert!(!STYLE_CSS.contains("\\\""), "no string escapes may ship");
        assert_eq!(
            STYLE_CSS.matches('{').count(),
            STYLE_CSS.matches('}').count(),
            "braces must balance"
        );
    }

    #[test]
    fn style_slides_pages_with_pure_css() {
        // Slide in/out is unconditional root-snapshot CSS — no transition
        // types, no JS. Type-scoped rules never matched (old-document types
        // don't propagate; prerender activations have no `from`).
        assert!(STYLE_CSS.contains("@view-transition{navigation:auto}"));
        assert!(STYLE_CSS.contains("::view-transition-old(root){animation:vt-out-left"));
        assert!(STYLE_CSS.contains("::view-transition-new(root){animation:vt-in-right"));
        assert!(
            !STYLE_CSS.contains("active-view-transition-type"),
            "no type-gated rules: every navigation slides"
        );
    }

    #[test]
    fn app_asset_is_plain_cacheable_js() {
        assert!(APP_JS.contains("applyTz"));
        assert!(
            !APP_JS.contains("nextFavicon"),
            "favicon fallback lives in the blocking head, before deferred JS"
        );
        assert!(APP_JS.contains("window.vigilRefresh"));
        assert!(!APP_JS.contains("{{"), "no format escapes may ship");
        assert!(!APP_JS.contains("</script"), "must not break out of HTML");
        // Tooltip elements are selected by tag, not by a server-rendered
        // data-tip: bars/circles ship title (or nothing) and the client
        // derives data-tip on load.
        assert!(APP_JS.contains("i[data-ts],circle[data-ts]"));
        assert!(!APP_JS.contains("[data-tip][data-ts]"));
        // Transition-direction tagging lives in the shell's blocking head
        // script (pagereveal must run before first render): types set in the
        // old document's pageswap never reach the new document, so a
        // deferred duplicate here would both miss pagereveal and split
        // ownership.
        assert!(!APP_JS.contains("pageswap"));
        assert!(!APP_JS.contains("pagereveal"));
    }

    #[test]
    fn app_hoists_one_timezone_abbreviation_per_pass() {
        // Local-mode tooltips reuse a single abbreviation computed at the
        // top of `applyTz`; constructing an `Intl.DateTimeFormat` per
        // tooltip made the 30-second pass cost thousands of formatters.
        assert!(
            APP_JS.contains("tzAb=local?tzAbbr():''"),
            "abbreviation must be hoisted once per pass"
        );
        assert_eq!(
            APP_JS.matches("tzAbbr()").count(),
            3,
            "definition, pass-level lookup, and toggle label only"
        );
    }

    #[test]
    fn app_patches_health_without_reloading_stable_pages() {
        assert!(APP_JS.contains("function patchHealth("));
        assert!(APP_JS.contains("function patchRow("));
        assert!(APP_JS.contains("data-place"));
        assert!(APP_JS.contains("data-ms"));
        assert!(APP_JS.contains("data-has"));
        assert!(
            APP_JS.contains("if(!patchHealth(j))location.reload();"),
            "reload stays a structural-change fallback"
        );
        assert!(
            !APP_JS.contains("if(nxt>seen)location.reload();"),
            "a data tick must patch, not reload"
        );
    }

    #[test]
    fn app_timer_updates_only_relative_timestamps() {
        // Absolute labels and tooltips are static between timezone changes;
        // the 30-second tick must not rescan and rewrite every tooltip.
        assert!(APP_JS.contains("function applyRel("));
        assert!(APP_JS.contains("applyRel();},30000)"));
        assert!(
            !APP_JS.contains("},30000){if(document.hidden)return;applyTz();}"),
            "tick must not rerun the full timezone pass"
        );
    }

}
