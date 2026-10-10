//! Request authentication: optional Basic-auth gate + HTML escaping.
//!
//! The gate is intentionally dependency-free (hand-rolled Base64 decode,
//! constant-time credential comparison) to keep the worker bundle small.

use vigil_core::config::Site;
use worker::{Env, Request, Response, ResponseBody, Result};

/// `true` when the request passes the site gate (or no gate is configured).
#[must_use]
pub fn authed(req: &Request, site: &Site, env: &Env) -> bool {
    let Some(gate) = &site.auth else {
        return true;
    };
    let want: String = match (&gate.pass, &gate.pass_env) {
        (_, Some(key)) => env.var(key).map(|v| v.to_string()).unwrap_or_default(),
        (Some(password), None) => password.clone(),
        (None, None) => String::new(),
    };
    let Some(encoded) = authorization_b64(req) else {
        return false;
    };
    let Ok(decoded) = base64_decode(&encoded) else {
        return false;
    };
    constant_time_eq(
        decoded.as_bytes(),
        format!("{}:{want}", gate.user).as_bytes(),
    )
}

/// Extract the Base64 credentials from a `Basic …` authorization header.
fn authorization_b64(req: &Request) -> Option<String> {
    let value = req.headers().get("authorization").ok()??;
    value.strip_prefix("Basic ").map(str::to_owned)
}

/// Standard-alphabet Base64 decode. `Err(())` on any invalid input.
fn base64_decode(input: &str) -> Result<String, ()> {
    let mut out = Vec::with_capacity(input.len().saturating_mul(3) / 4);
    let mut acc = 0_u32;
    let mut bits = 0_u32;
    for c in input.chars() {
        if c == '=' {
            break;
        }
        let value = match c {
            'A'..='Z' => u32::from(c as u8 - b'A'),
            'a'..='z' => u32::from(c as u8 - b'a') + 26,
            '0'..='9' => u32::from(c as u8 - b'0') + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(()),
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

/// Constant-time equality: no early exit, so response time does not leak
/// how many leading bytes matched.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 401 challenge response for unauthenticated requests.
pub fn deny() -> Result<Response> {
    Ok(Response::builder()
        .with_status(401)
        .with_header("www-authenticate", "Basic realm=\"status\"")?
        .body(ResponseBody::Body(b"auth needed".to_vec())))
}

/// Escape text for HTML element content and double-quoted attributes.
#[must_use]
pub fn esc(s: &str) -> String {
    vigil_core::views::escape_html(s)
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, constant_time_eq, esc};

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_decode("YWRtaW46c2VjcmV0").as_deref(), Ok("admin:secret"));
        assert_eq!(base64_decode("").as_deref(), Ok(""));
        assert!(base64_decode("!!!").is_err());
        assert!(base64_decode("/w==").is_err()); // 0xFF, not UTF-8
    }

    #[test]
    fn equality_is_constant_time_and_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn esc_neutralizes_markup() {
        assert_eq!(esc("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
        assert_eq!(esc("plain"), "plain");
    }
}
