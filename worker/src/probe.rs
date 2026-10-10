//! Probing: HTTP via outbound `fetch`, TCP via Workers sockets.
//!
//! Every probe races against a `Delay` deadline so one hung endpoint
//! cannot stall the whole cron tick.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::time::Duration;
use vigil_core::checks::{place, Seen, What};
use vigil_core::config::{Monitor, Probe};
use worker::{Date, Delay, Error, Fetch, Headers, Method, Request, RequestInit, Socket};

/// Maximum body bytes kept for keyword matching (bounds memory + D1 rows).
pub const MAX_BODY_SAMPLE: usize = 8_192;

/// Current time in milliseconds (Workers clock).
fn now_ms() -> u64 {
    Date::now().as_millis()
}

/// This isolate's Cloudflare colo (`SIN`, `BOM`, …), if determinable.
///
/// Scheduled handlers receive no `Request`, so there is no `cf.colo` to
/// read — instead one fast edge subrequest per tick hits `cdn-cgi/trace`
/// and parses its `colo=` line. Any failure yields `None` and samples
/// keep the `"CRON"` origin via [`place`](vigil_core::checks::place).
pub async fn colo() -> Option<String> {
    let url = url::Url::parse("https://www.cloudflare.com/cdn-cgi/trace").ok()?;
    let mut resp = Fetch::Url(url).send().await.ok()?;
    let body = resp.text().await.ok()?;
    parse_colo(&body)
}

/// Extract the `colo=` line from a `cdn-cgi/trace` response body.
fn parse_colo(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        line.strip_prefix("colo=")
            .map(|colo| colo.trim().to_uppercase())
            .filter(|colo| !colo.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::parse_colo;

    #[test]
    fn parses_colo_line() {
        let body = "fl=1\nh=www.cloudflare.com\ncolo=BOM\nloc=IN\n";
        assert_eq!(parse_colo(body).as_deref(), Some("BOM"));
        assert_eq!(parse_colo("no colo here\n"), None);
        assert_eq!(parse_colo("colo=\n"), None);
    }
}

/// Run one monitor's probe; always returns a [`Seen`], never throws.
pub async fn probe(monitor: &Monitor, colo: Option<&str>) -> (Seen, String) {
    let location = place(colo);
    let start = now_ms();
    let seen = match &monitor.check {
        Probe::Tcp { host, port } => match probe_tcp(host, *port, monitor.timeout_ms).await {
            Ok(ms) => Seen {
                ms,
                what: What::Tcp { open: true },
            },
            Err(reason) => Seen {
                ms: now_ms().saturating_sub(start),
                what: What::Dead { why: reason },
            },
        },
        Probe::Http {
            url,
            method,
            headers,
            body,
        } => match probe_http(url, method, headers, body.as_deref(), monitor.timeout_ms).await {
            Ok((code, text, ms)) => {
                let mut sample = text;
                // `truncate` lands on a char boundary — safe for UTF-8 bodies.
                sample.truncate(sample_char_boundary(&sample, MAX_BODY_SAMPLE));
                Seen {
                    ms,
                    what: What::Http { code, body: sample },
                }
            }
            Err(reason) => Seen {
                ms: now_ms().saturating_sub(start),
                what: What::Dead { why: reason },
            },
        },
    };
    (seen, location)
}

/// Largest prefix of `s` that is `<= max_bytes` and ends on a char boundary.
fn sample_char_boundary(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn to_method(method: &str) -> Method {
    Method::from(method.to_owned())
}

async fn probe_http(
    url: &str,
    method: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
    timeout_ms: u64,
) -> Result<(u16, String, u64), String> {
    let start = now_ms();
    let mut init = RequestInit::new();
    init.with_method(to_method(method));
    let request_headers = Headers::new();
    for (key, value) in headers {
        request_headers.set(key, value).map_err(|e| e.to_string())?;
    }
    init.with_headers(request_headers);
    if let Some(payload) = body {
        init.with_body(Some(wasm_bindgen::JsValue::from_str(payload)));
    }
    let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
    let mut resp = with_timeout(Fetch::Request(req).send(), timeout_ms)
        .await
        .map_err(|e| e.to_string())?;
    let code = resp.status_code();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    Ok((code, text, now_ms().saturating_sub(start)))
}

async fn probe_tcp(host: &str, port: u16, timeout_ms: u64) -> Result<u64, String> {
    let start = now_ms();
    let target = host.to_owned();
    let connect = async move {
        let sock = Socket::builder()
            .connect(target, port)
            .map_err(|e| e.to_string())?;
        sock.opened().await.map_err(|e| e.to_string())?;
        Ok::<(), Error>(())
    };
    with_timeout(connect, timeout_ms)
        .await
        .map_err(|e| e.to_string())?;
    Ok(now_ms().saturating_sub(start))
}

/// Race `future` against a deadline; timeout yields a worker [`Error`].
/// Shared with the tick path so webhook delivery honors a deadline too.
pub(crate) async fn with_timeout<T>(
    future: impl Future<Output = Result<T, Error>>,
    ms: u64,
) -> Result<T, Error> {
    use futures_util::future::{select, Either};
    let deadline = Delay::from(Duration::from_millis(ms.max(1)));
    let mut future = pin!(future);
    let mut deadline = pin!(deadline);
    match select(future.as_mut(), deadline.as_mut()).await {
        Either::Left((value, _)) => value,
        Either::Right(((), _)) => Err(Error::from("probe timeout")),
    }
}
