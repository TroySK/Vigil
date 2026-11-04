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
        candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
    })
}

#[cfg(test)]
mod tests {
    use super::{etag, not_modified, APP_JS, STYLE_CSS};

    /// Declarations of the first exact `selector{…}` rule in `css`.
    fn declarations<'a>(css: &'a str, selector: &str) -> &'a str {
        let needle = format!("{selector}{{");
        let start = css
            .find(&needle)
            .unwrap_or_else(|| panic!("missing rule {selector}"))
            + needle.len();
        let end = css[start..]
            .find('}')
            .unwrap_or_else(|| panic!("unclosed rule {selector}"))
            + start;
        &css[start..end]
    }

    /// Body of the top-level `@media <query>{…}` block, braces balanced.
    fn media_block(query: &str) -> &str {
        let needle = format!("@media {query}{{");
        let start = STYLE_CSS
            .find(&needle)
            .unwrap_or_else(|| panic!("missing @media {query}"))
            + needle.len();
        let mut depth = 1_u32;
        for (offset, byte) in STYLE_CSS[start..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &STYLE_CSS[start..start + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unclosed @media {query}");
    }

    #[test]
    fn style_keeps_a_compact_mobile_header() {
        let mobile = media_block("(max-width:40rem)");
        // Touch-sized controls with tight gaps, and a brand that can
        // truncate instead of colliding with the status pill.
        assert!(declarations(mobile, ".nav-edge").contains("gap:var(--space-xs)"));
        assert!(declarations(mobile, ".head-right").contains("gap:var(--space-2xs)"));
        assert!(declarations(mobile, ".brand img").contains("28px"));
        assert!(declarations(mobile, ".brand-name").contains("text-overflow:ellipsis"));
        assert!(
            declarations(media_block("(max-width:22rem)"), ".brand-name").contains("display:none"),
            "very narrow phones keep only the logo mark"
        );
        assert!(
            STYLE_CSS.contains("env(safe-area-inset-left)"),
            "notched phones need horizontal safe-area padding"
        );
        assert!(
            STYLE_CSS.contains("env(safe-area-inset-top)"),
            "standalone phones must clear the notch above the header"
        );
        assert!(
            STYLE_CSS.contains("env(safe-area-inset-bottom)"),
            "home-indicator phones need bottom safe-area padding"
        );
        assert!(
            STYLE_CSS.contains("-webkit-backdrop-filter"),
            "iOS Safari needs the prefixed sticky-header blur"
        );
        assert!(
            STYLE_CSS.contains("-webkit-text-size-adjust:100%"),
            "iOS must not inflate text in landscape"
        );
    }

    #[test]
    fn style_scales_sparklines_and_keeps_daily_bars_on_one_row() {
        // The rule used to be scoped to `table.inc td .spark`, so index and
        // detail sparklines kept their 560px intrinsic width on phones.
        assert!(declarations(STYLE_CSS, ".spark").contains("max-width:100%"));
        let mobile = media_block("(max-width:40rem)");
        assert!(
            declarations(mobile, ".spark").contains("display:block"),
            "phones stack the sparkline above its percentile text"
        );
        assert!(declarations(mobile, ".bars90").contains("flex-wrap:nowrap"));
        assert!(declarations(mobile, ".bars90 i").contains("min-width:0"));
    }

    #[test]
    fn style_stacks_monitor_rows_without_overflowing() {
        let mobile = media_block("(max-width:40rem)");
        assert!(
            declarations(mobile, ".r-head").contains("grid-template-columns:10px minmax(0,1fr)")
        );
        assert!(declarations(mobile, ".r-meta").contains("flex-wrap:wrap"));
        assert!(declarations(mobile, ".r-meta s").contains("flex:1 1 100%"));
        assert!(declarations(mobile, ".r-state").contains("justify-content:space-between"));
    }

    #[test]
    fn style_lays_mobile_tables_out_as_label_value_rows() {
        let mobile = media_block("(max-width:40rem)");
        assert!(declarations(mobile, "table.inc td")
            .contains("grid-template-columns:7rem minmax(0,1fr)"));
    }

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
    fn app_reloads_when_probe_data_changes() {
        // A tick can open/close a gap, so history bars, sparklines, detail
        // charts, and incidents change with it. Patching only the status
        // text left all of those stale until a manual reload.
        assert!(
            APP_JS.contains("if(nxt>seen)location.reload();"),
            "a data tick must reload the whole page"
        );
        assert!(
            !APP_JS.contains("patchHealth("),
            "no in-place-only path may suppress the reload"
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
