//! Probing: HTTP via outbound `fetch`, TCP via Workers sockets.
//!
//! Every probe races against a `Delay` deadline so one hung endpoint
//! cannot stall the whole cron tick.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::OnceLock;
use std::time::Duration;
use futures_util::StreamExt as _;
use vigil_core::checks::{place, Seen, What};
use vigil_core::config::{Monitor, Probe};
use worker::{console_log, Date, Delay, Error, Fetch, Headers, Method, Request, RequestInit, Socket};

/// Maximum body bytes kept for keyword matching (bounds memory + D1 rows).
pub const MAX_BODY_SAMPLE: usize = 8_192;

/// Maximum response bytes downloaded per probe. Keyword matching only
/// needs the prefix (see [`MAX_BODY_SAMPLE`]); anything beyond this is
/// abandoned mid-stream instead of buffered into isolate memory, so one
/// huge error page cannot OOM the tick.
pub const MAX_DOWNLOAD_BYTES: usize = 64 * 1_024;

/// Current time in milliseconds (Workers clock).
fn now_ms() -> u64 {
    Date::now().as_millis()
}

/// This isolate's Cloudflare colo (`SIN`, `BOM`, …), if determinable.
///
/// Colo is a property of the isolate's datacenter and never changes while
/// the isolate lives, so one trace lookup serves every tick the isolate
/// handles; later ticks reuse the memoized value with no subrequest.
static COLO_CACHE: OnceLock<String> = OnceLock::new();

/// Deadline for the colo trace lookup: a hung `cdn-cgi/trace` must not
/// stall the whole tick behind one subrequest (tail risk on cold isolates).
pub const COLO_TIMEOUT_MS: u64 = 2_000;

/// Resolve the tick's colo, using the memoized isolate colo when present.
pub async fn colo() -> Option<String> {
    if let Some(hit) = COLO_CACHE.get() {
        return Some(hit.clone());
    }
    let fresh = with_timeout(
        async { Ok::<Option<String>, Error>(fetch_colo().await) },
        COLO_TIMEOUT_MS,
    )
    .await
    .map_err(|_| {
        console_log!("colo trace timed out after {COLO_TIMEOUT_MS}ms");
    })
    .ok()
    .flatten();
    memo_colo(&COLO_CACHE, fresh)
}

/// Memoize one successful colo lookup in `cache`: the cached value wins on
/// later calls, and failures are never stored so the next tick retries.
fn memo_colo(cache: &OnceLock<String>, fresh: Option<String>) -> Option<String> {
    if let Some(hit) = cache.get() {
        return Some(hit.clone());
    }
    let found = fresh?;
    Some(cache.get_or_init(|| found).clone())
}

/// Human-readable note for a failed colo lookup. Every `None` becomes a
/// `"CRON"` sample, so each failure mode logs distinctly — `wrangler tail`
/// is the only way to see why an isolate can't resolve its colo.
fn colo_failure_note(status: Option<u16>) -> &'static str {
    match status {
        None => "colo trace request failed",
        Some(s) if (200..300).contains(&s) => "colo trace had no colo= line",
        Some(_) => "colo trace returned an error status",
    }
}

/// One fast edge subrequest per uncached tick.
///
/// Scheduled handlers receive no `Request`, so there is no `cf.colo` to
/// read — instead the tick hits `cdn-cgi/trace` and parses its `colo=`
/// line. Any failure yields `None` and samples keep the `"CRON"` origin
/// via [`place`](vigil_core::checks::place).
async fn fetch_colo() -> Option<String> {
    let url = url::Url::parse("https://www.cloudflare.com/cdn-cgi/trace").ok()?;
    let mut resp = Fetch::Url(url)
        .send()
        .await
        .map_err(|e| console_log!("{}: {e}", colo_failure_note(None)))
        .ok()?;
    let status = resp.status_code();
    let body = resp
        .text()
        .await
        .map_err(|e| console_log!("colo trace body unreadable (status {status}): {e}"))
        .ok()?;
    parse_colo(&body).or_else(|| {
        console_log!("{}", colo_failure_note(Some(status)));
        None
    })
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
    use super::{colo_failure_note, memo_colo, parse_colo, read_capped, Error};
    use futures_util::FutureExt as _;
    use std::sync::OnceLock;

    #[test]
    fn capped_read_keeps_short_bodies_whole() {
        let items = vec![Ok::<_, Error>(b"hel".to_vec()), Ok(b"lo".to_vec())];
        let mut stream = Box::pin(futures_util::stream::iter(items));
        let (bytes, cut) = read_capped(&mut stream, 64)
            .now_or_never()
            .expect("ready stream resolves")
            .expect("ok");
        assert_eq!(bytes, b"hello");
        assert!(!cut);
    }

    #[test]
    fn capped_read_stops_at_the_cap() {
        let items = vec![Ok::<_, Error>(vec![b'a'; 10]), Ok(vec![b'b'; 10])];
        let mut stream = Box::pin(futures_util::stream::iter(items));
        let (bytes, cut) = read_capped(&mut stream, 12)
            .now_or_never()
            .expect("ready stream resolves")
            .expect("ok");
        let mut want = vec![b'a'; 10];
        want.extend_from_slice(&[b'b'; 2]);
        assert_eq!(bytes, want);
        assert!(cut);
    }

    #[test]
    fn capped_read_reports_stream_errors() {
        let items = vec![
            Ok::<_, Error>(b"ab".to_vec()),
            Err(Error::from("boom".to_owned())),
        ];
        let mut stream = Box::pin(futures_util::stream::iter(items));
        let error = read_capped(&mut stream, 64)
            .now_or_never()
            .expect("ready stream resolves")
            .expect_err("must fail");
        assert!(error.contains("boom"));
    }

    #[test]
    fn colo_failures_explain_themselves() {
        // Every `None` from the trace lookup becomes a "CRON" sample, so
        // each failure mode needs a distinct log line (`wrangler tail` is
        // the only way to see why an isolate can't resolve its colo).
        assert_eq!(colo_failure_note(None), "colo trace request failed");
        assert_eq!(
            colo_failure_note(Some(200)),
            "colo trace had no colo= line"
        );
        assert_eq!(
            colo_failure_note(Some(403)),
            "colo trace returned an error status"
        );
    }

    #[test]
    fn parses_colo_line() {
        let body = "fl=1\nh=www.cloudflare.com\ncolo=BOM\nloc=IN\n";
        assert_eq!(parse_colo(body).as_deref(), Some("BOM"));
        assert_eq!(parse_colo("no colo here\n"), None);
        assert_eq!(parse_colo("colo=\n"), None);
    }

    #[test]
    fn colo_memo_keeps_first_success() {
        let cache = OnceLock::new();
        assert_eq!(
            memo_colo(&cache, Some("BOM".to_owned())),
            Some("BOM".to_owned())
        );
        // The cached isolate colo wins over later lookups.
        assert_eq!(
            memo_colo(&cache, Some("SIN".to_owned())),
            Some("BOM".to_owned())
        );
        assert_eq!(memo_colo(&cache, None), Some("BOM".to_owned()));
    }

    #[test]
    fn colo_memo_retries_after_failure() {
        let cache = OnceLock::new();
        assert_eq!(memo_colo(&cache, None), None);
        assert!(cache.get().is_none());
        assert_eq!(
            memo_colo(&cache, Some("BOM".to_owned())),
            Some("BOM".to_owned())
        );
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
        } => {
            // The whole fetch incl. body read races the per-probe deadline:
            // the send has its own timeout inside, and this backstop bounds
            // a slow-drip body that would otherwise stall the tick forever.
            let fetch = probe_http(url, method, headers, body.as_deref(), monitor.timeout_ms);
            match with_timeout(
                async { fetch.await.map_err(Error::from) },
                monitor.timeout_ms,
            )
            .await
            {
                Ok((code, text, ms)) => {
                    let mut sample = text;
                    // `truncate` lands on a char boundary — safe for UTF-8 bodies.
                    sample.truncate(sample_char_boundary(&sample, MAX_BODY_SAMPLE));
                    Seen {
                        ms,
                        what: What::Http { code, body: sample },
                    }
                }
                Err(e) => Seen {
                    ms: now_ms().saturating_sub(start),
                    what: What::Dead {
                        why: e.to_string(),
                    },
                },
            }
        }
    };
    (seen, location)
}

/// Read at most `cap` bytes from a byte-chunk stream, then stop polling:
/// the network read is abandoned instead of buffering an unbounded body
/// into isolate memory. Returns the bytes plus whether the body was cut
/// (a body ending exactly at the cap still reports cut — more may follow).
async fn read_capped<S>(stream: &mut S, cap: usize) -> Result<(Vec<u8>, bool), String>
where
    S: futures_util::Stream<Item = Result<Vec<u8>, Error>> + Unpin,
{
    let mut buf = Vec::new();
    while buf.len() < cap {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if buf.len() >= cap {
                    return Ok((buf, true));
                }
            }
            Some(Err(e)) => return Err(e.to_string()),
            None => break,
        }
    }
    Ok((buf, false))
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
    // Stream with a byte cap: `resp.text()` would buffer an unbounded
    // body (one huge error page × concurrent probes OOMs the isolate).
    // Lossy UTF-8 matches the old `text()` decoding; the existing
    // char-boundary truncate below still trims to `MAX_BODY_SAMPLE`.
    let mut stream = Box::pin(resp.stream().map_err(|e| e.to_string())?);
    let (bytes, _) = read_capped(&mut stream, MAX_DOWNLOAD_BYTES).await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
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
