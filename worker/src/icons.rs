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
