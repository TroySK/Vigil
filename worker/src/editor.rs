//! Config editor: copy-paste YAML UI served only when `EDITOR=true`.
//!
//! While editor mode is on the status site does not serve — every route
//! renders this page — and the cron tick skips probing. The page says so
//! explicitly: remove `EDITOR=true` and redeploy to restore the site.
//!
//! Editor access is gated by `EDITOR=true` alone: while it is set,
//! anyone who can reach `/editor` can read and replace the site config.
//! Saves additionally require same-origin browser metadata so a
//! cross-site form cannot overwrite the config. Keep the window short.

use crate::auth::esc;
use crate::db::Db;
use crate::page::shell;
use worker::{Request, Response, ResponseBody, Result};

/// Maximum accepted editor body (config YAML). Large enough for any real
/// config, small enough that a stray upload cannot exhaust the isolate.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// `true` when the request's browser metadata permits a state change.
/// Requests without metadata (curl) pass; a present
/// `Origin`/`Sec-Fetch-Site` must agree with this origin.
fn same_origin(origin: Option<&str>, sec_fetch_site: Option<&str>, url: &url::Url) -> bool {
    if let Some(site) = sec_fetch_site {
        if site != "same-origin" && site != "none" {
            return false;
        }
    }
    match origin {
        None => true,
        Some(origin) => url::Url::parse(origin).is_ok_and(|parsed| {
            parsed.scheme() == url.scheme()
                && parsed.host_str() == url.host_str()
                && parsed.port_or_known_default() == url.port_or_known_default()
        }),
    }
}

/// `true` when a body exceeds the editor cap.
fn too_large(len: usize) -> bool {
    len > MAX_BODY_BYTES
}

/// `true` when an editor request may proceed. Reads always may; a POST
/// must carry same-origin browser metadata so a cross-site form cannot
/// overwrite the config.
pub fn permitted(req: &Request, url: &url::Url) -> bool {
    req.method() != worker::Method::Post
        || same_origin(
            header(req, "origin").as_deref(),
            header(req, "sec-fetch-site").as_deref(),
            url,
        )
}

/// One request header, header absence, or malformed headers as `None`.
fn header(req: &Request, name: &str) -> Option<String> {
    req.headers().get(name).ok().flatten()
}

/// 403 for a cross-origin editor submission.
pub fn deny() -> Result<Response> {
    Ok(Response::builder()
        .with_status(403)
        .with_header("cache-control", "no-store")?
        .with_header("x-robots-tag", "noindex, nofollow")?
        .body(ResponseBody::Body(b"cross-origin editor request".to_vec())))
}

/// 413 for oversized editor submissions.
fn payload_too_large() -> Result<Response> {
    Ok(Response::builder()
        .with_status(413)
        .with_header("cache-control", "no-store")?
        .body(ResponseBody::Body(b"editor body too large".to_vec())))
}

/// Render the editor: current saved YAML (if any) prefilled in a
/// textarea, plus validation errors / save confirmation when present.
pub async fn render(
    db: &Db,
    current: Option<&str>,
    errors: &[String],
    saved: bool,
) -> Result<Response> {
    let stored = match current {
        Some(text) => Some(text.to_owned()),
        None => crate::site_store::load_raw(db).await?,
    };
    let value = stored.unwrap_or_default();
    let mut notices = String::new();
    if saved {
        notices.push_str(
            "<div class=\"note\"><b>Saved.</b> Now remove <code>EDITOR=true</code> \
             from the worker environment and redeploy — the status site stays \
             offline until you do.</div>",
        );
    }
    if !errors.is_empty() {
        notices.push_str("<div class=\"note active\"><b>Config invalid — not saved.</b><ul>");
        for error in errors {
            let _ =
                std::fmt::Write::write_fmt(&mut notices, format_args!("<li>{}</li>", esc(error)));
        }
        notices.push_str("</ul></div>");
    }
    let body = format!(
        "<div class=\"note active\"><b>Editor mode.</b> The status site is offline while \
         <code>EDITOR=true</code> is set: pages, APIs, and cron probes are all paused. \
         Paste your <code>site.yaml</code> below, save, then <b>remove \
         <code>EDITOR=true</code> and redeploy</b> so the app works again. \
         See <code>config/site.example.yaml</code> for the field reference.</div>\
         <div class=\"note\">This editor is unauthenticated while \
         <code>EDITOR=true</code> is set: anyone who can reach this page can replace \
         the site config. Saves must come from this origin. Keep the window short \
         and redeploy without <code>EDITOR</code> as soon as you have saved.</div>\
         {notices}\
         <h2 class=group>Site config (YAML)</h2>\
         <form method=post action=\"/editor\">\
         <label class=sr-only for=ed-yaml>Site YAML</label>\
         <textarea id=ed-yaml name=yaml rows=32 cols=80 style=\"width:100%;font-family:monospace\" \
         spellcheck=false>{}</textarea>\
         <p><button class=det type=submit>Save config</button></p>\
         </form>",
        esc(&value),
    );
    let links = String::new();
    let menu = String::new();
    let speculation = String::new();
    Response::from_html(shell(
        "Vigil config editor",
        "Vigil config editor",
        &links,
        "0/0",
        0,
        0,
        &menu,
        "/editor",
        &speculation,
        0,
        &body,
    ))
}

/// Save handler for `POST /editor`: validate first, persist only when the
/// YAML parses and passes [`vigil_core::config::Site::check`].
pub async fn save(req: &mut Request, db: &Db) -> Result<Response> {
    if header(req, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(too_large)
    {
        return payload_too_large();
    }
    let raw_body = req.text().await.unwrap_or_default();
    if too_large(raw_body.len()) {
        return payload_too_large();
    }
    let yaml = form_yaml(&raw_body, req);
    match vigil::parse_checked(&yaml) {
        Ok(_) => {
            db.batch(vec![db.put_stmt(vigil::SITE_KV_KEY, &yaml)?])
                .await?;
            // Drop this isolate's cached raw/parsed config so the saved
            // bytes are used immediately if editor mode is still serving.
            crate::site_store::invalidate();
            render(db, Some(&yaml), &[], true).await
        }
        Err(errors) => render(db, Some(&yaml), &errors, false).await,
    }
}

/// Extract the `yaml` field from a form-encoded POST body, or treat the
/// whole body as raw YAML when it is not form-encoded.
fn form_yaml(body: &str, req: &Request) -> String {
    let is_form = req
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .is_some_and(|value| value.contains("application/x-www-form-urlencoded"));
    // Only trust the `yaml=` prefix when the content-type says form: a raw
    // YAML paste starting with the literal bytes `yaml=` must not collapse
    // into one decoded line.
    if is_form {
        for pair in body.split('&') {
            if let Some(encoded) = pair.strip_prefix("yaml=") {
                return url_decode(encoded);
            }
        }
    }
    body.to_owned()
}

/// Minimal `application/x-www-form-urlencoded` decoder (`+` → space,
/// `%XX` → byte). Malformed escapes pass through literally. Byte-based:
/// never slices `&str` at non-char boundaries (a `%` followed by
/// multibyte input must not panic).
fn url_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match bytes.get(i + 1..i + 3) {
                    Some(hex_bytes) if hex_bytes.iter().all(u8::is_ascii_hexdigit) => {
                        // ASCII-hex only, so UTF-8 conversion cannot fail.
                        let hex = std::str::from_utf8(hex_bytes).unwrap_or("");
                        if let Ok(byte) = u8::from_str_radix(hex, 16) {
                            out.push(byte);
                            i += 3;
                        } else {
                            out.push(b'%');
                            i += 1;
                        }
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 500 page for normal mode when the DB holds no (or bad) config.
pub fn missing_config_page(reason: &str) -> Result<Response> {
    let body = format!(
        "<div class=\"note active\"><b>Site config missing or invalid.</b> {}</div>\
         <div class=\"note\">Deploy once with the <code>EDITOR=true</code> worker var, \
         open <code>/editor</code>, paste your YAML, save — then <b>remove \
         <code>EDITOR=true</code> and redeploy</b> so the app works.</div>",
        esc(reason),
    );
    let html = shell(
        "Vigil — no config",
        "Vigil",
        "",
        "0/0",
        0,
        0,
        "",
        "/",
        "",
        0,
        &body,
    );
    Ok(Response::builder()
        .with_status(500)
        .with_header("content-type", "text/html;charset=UTF-8")?
        .body(worker::ResponseBody::Body(html.into_bytes())))
}

#[cfg(test)]
mod tests {
    use super::url_decode;

    #[test]
    fn url_decode_handles_form_escapes() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("a%0Ab"), "a\nb");
        assert_eq!(url_decode("100%25"), "100%");
        assert_eq!(url_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn url_decode_never_panics_on_multibyte_input() {
        // `%` followed by multibyte chars must not slice a char boundary.
        assert_eq!(url_decode("%€"), "%€");
        assert_eq!(url_decode("a%€b"), "a%€b");
        assert_eq!(url_decode("%"), "%");
        assert_eq!(url_decode("%A"), "%A");
    }

    #[test]
    fn checked_parse_rejects_bad_config() {
        assert!(vigil::parse_checked("title: X\nmonitors: []\n").is_err());
        assert!(vigil::parse_checked(
            "title: X\nmonitors:\n  - id: m\n    name: M\n    check:\n      via: tcp\n      host: example.com\n      port: 80\n"
        )
        .is_ok());
    }
}

#[cfg(test)]
mod security_tests {
    use super::{same_origin, too_large, MAX_BODY_BYTES};

    fn url() -> url::Url {
        url::Url::parse("https://status.example/editor").unwrap()
    }

    #[test]
    fn editor_saves_require_a_same_origin_request() {
        // No browser metadata (curl) is allowed.
        assert!(same_origin(None, None, &url()));
        // Same-origin form posts and direct navigations.
        assert!(same_origin(
            Some("https://status.example"),
            Some("same-origin"),
            &url()
        ));
        assert!(same_origin(None, Some("none"), &url()));
        // Cross-site and sibling-origin posts are CSRF attempts.
        assert!(!same_origin(None, Some("cross-site"), &url()));
        assert!(!same_origin(None, Some("same-site"), &url()));
        assert!(!same_origin(Some("https://evil.example"), None, &url()));
        assert!(!same_origin(Some("http://status.example"), None, &url()));
        assert!(!same_origin(
            Some("https://status.example:8443"),
            None,
            &url()
        ));
        assert!(!same_origin(Some("not a url"), None, &url()));
    }

    #[test]
    fn editor_body_is_capped() {
        assert!(!too_large(0));
        assert!(!too_large(MAX_BODY_BYTES));
        assert!(too_large(MAX_BODY_BYTES + 1));
    }
}
