# Vigil security runbook

Threat model: a public, unauthenticated status page and JSON API running on
Cloudflare Workers + D1. Config authors are trusted; visitors are not.

## Handled in code

- All D1 access is parameterized; no SQL string interpolation exists.
- All config- and DB-derived strings are HTML-escaped at render time
  (element content and double-quoted attribute contexts).
- Baseline response headers on every route: `referrer-policy: no-referrer`,
  `x-content-type-options: nosniff`, `x-frame-options: DENY`.
- Tick-granular pages/APIs carry `Cache-Control: public, max-age=30`
  (the badge keeps `no-store`); this bounds D1 fan-out cost per visitor.
- Probes race per-monitor deadlines; webhook delivery has a 10s deadline;
  probe waves are capped to stay under the subrequest ceiling.
- Site icons always load over TLS (even for plain-HTTP probes) and render
  from escaped origins.
- `site.yaml` refuses `every_min: 0` and documents that secrets pasted
  into headers end up in git.

## Operator actions (Cloudflare dashboard)

1. **Rate limiting (do this first while the site is public).**
   Security → WAF → Rate limiting rules → Create rule:
   - Expression: `(http.request.uri.path starts_with "/api/")`,
     100 requests / 10 seconds per IP, action: Block 1 minute.
   - Second rule for `/` and `/monitor`: 60 requests / 10 seconds
     per IP, action: Managed Challenge.
2. **Access gate (if the page must not be public).** Uncomment `[auth]`
   in `config/site.yaml` with `pass_env = "STATUS_PASSWORD"`, then
   `wrangler secret put STATUS_PASSWORD`. There is no lockout or attempt
   throttling in the worker — the WAF rule above is the brute-force
   backstop, so keep it even with auth on. Use a 20+ character password.
3. **Secret rotation.** Worker secrets: `wrangler secret put NAME`.
   If a token ever lands in `site.yaml`, revoke it at the issuer —
   git history keeps it forever.
4. **After enabling auth**, re-check `/api/badge` consumers: badge URLs
   embedded as `<img>` tags cannot send Basic credentials and will break.

## Deliberately not handled

- No CSP: scripts and styles are inline by design; a strict policy needs
  a nonce/hash refactor first.
- No in-worker rate limiting: per-isolate memory is not shared state, so
  real throttling lives in the WAF rules above.
- Cron placement is Cloudflare's choice: probe origins are displayed,
  never assigned.
