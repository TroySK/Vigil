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
pub fn favicon() -> Result<Response> {
    Ok(Response::builder()
        .with_header("content-type", "image/x-icon")?
        .with_header("cache-control", "public, max-age=604800, immutable")?
        .body(ResponseBody::Body(FAVICON_ICO.to_vec())))
}

/// Serve `/logo.svg` for the header brand mark.
pub fn logo() -> Result<Response> {
    Ok(Response::builder()
        .with_header("content-type", "image/svg+xml")?
        .with_header("cache-control", "public, max-age=604800, immutable")?
        .body(ResponseBody::Body(LOGO_SVG.to_vec())))
}

/// App stylesheet, extracted from the page shell so HTML responses stop
/// carrying ~25KB of inline CSS: browsers + edge cache it immutably.
pub const STYLE_CSS: &str = include_str!("../assets/style.css");

/// App script, extracted from the page shell for the same reason. Reads
/// the per-page poll interval from the inline `window.vigilRefresh` set
/// by [`crate::page::shell`].
pub const APP_JS: &str = include_str!("../assets/app.js");

/// Serve `/style.css` with a long immutable cache.
pub fn style() -> Result<Response> {
    Ok(Response::builder()
        .with_header("content-type", "text/css;charset=UTF-8")?
        .with_header("cache-control", "public, max-age=604800, immutable")?
        .body(ResponseBody::Body(STYLE_CSS.as_bytes().to_vec())))
}

/// Serve `/app.js` with a long immutable cache.
pub fn app_js() -> Result<Response> {
    Ok(Response::builder()
        .with_header("content-type", "text/javascript;charset=UTF-8")?
        .with_header("cache-control", "public, max-age=604800, immutable")?
        .body(ResponseBody::Body(APP_JS.as_bytes().to_vec())))
}

#[cfg(test)]
mod tests {
    use super::{APP_JS, STYLE_CSS};

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
    fn app_asset_is_plain_cacheable_js() {
        assert!(APP_JS.contains("applyTz"));
        assert!(APP_JS.contains("nextFavicon"));
        assert!(APP_JS.contains("window.vigilRefresh"));
        assert!(!APP_JS.contains("{{"), "no format escapes may ship");
        assert!(!APP_JS.contains("</script"), "must not break out of HTML");
    }
}
