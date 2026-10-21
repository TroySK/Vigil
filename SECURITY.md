# Vigil security runbook

Threat model: a public, unauthenticated status page and JSON API running on
Cloudflare Workers + D1. Config authors are trusted; visitors are not.

## Handled in code

- All D1 access is parameterized; no SQL string interpolation exists.
- All config- and DB-derived strings are HTML-escaped at render time
  (element content and double-quoted attribute contexts).
- API ids are checked against the configured monitor list before any D1
  read: history/incidents/latency return 404 for missing or removed ids,
  and unknown/duplicate query parameters (plus unknown `?range=` values)
  return 400 so arbitrary URLs cannot bypass the edge cache.
- Baseline response headers on every route: `referrer-policy: no-referrer`,
  `x-content-type-options: nosniff`, `x-frame-options: DENY`.
- Config validation allowlists link schemes: monitor `href` and webhook
  URLs accept `http`/`https` (plus same-origin `/path` for `href`);
  footer `links` also accept `mailto:`. `javascript:`, `data:`,
  `vbscript:`, `blob:`, `file:`, protocol-relative `//host`, and
  control-character obfuscation are rejected before anything renders.
- 5-minute pages/APIs carry `Cache-Control: public, max-age=300`
  (the badge matches); this bounds D1 fan-out cost per visitor.
- Probes race per-monitor deadlines; webhook delivery has a 10s deadline;
  probe waves are capped to stay under the subrequest ceiling.
- Site icons always load over TLS (even for plain-HTTP probes) and render
  from escaped origins.
- The site config refuses sub-5-minute uptime cadences and documents that secrets
  pasted into headers end up in D1 (readable by anyone with DB access).
- The config editor (`EDITOR=true`) is token-gated: `EDITOR_TOKEN` must
  be a 16+ character worker secret, requests without it are rejected,
  saves must carry same-origin metadata, bodies are capped at 1 MiB, and
  every editor response is `no-store` + `noindex`. A missing/short token
  disables the editor entirely (fail closed). Remove the flag and
  redeploy immediately after saving.
- Basic auth fails closed: a configured gate with a missing/empty
  `pass`/`pass_env` secret denies every request instead of accepting
  `user:` as credentials. Config validation also rejects empty users,
  empty credentials, colons in the username, and both password sources.
- Gated responses are `private, no-store`; 401 challenges are never
  cached.
- Deploy CI pins action SHAs plus Wrangler and worker-build versions,
  runs with `contents: read`, and does not persist the checkout token;
  `.dev.vars*` and `.env*` are git-ignored.

## Operator actions (Cloudflare dashboard)

1. **Rate limiting (do this first while the site is public).**
   Security → WAF → Rate limiting rules → Create rule:
   - Expression: `(http.request.uri.path starts_with "/api/")`,
     100 requests / 10 seconds per IP, action: Block 1 minute.
   - Second rule for `/` and `/monitor`: 60 requests / 10 seconds
     per IP, action: Managed Challenge.
2. **Access gate (if the page must not be public).** Set `[auth]`
   in the editor-saved config with `pass_env = "STATUS_PASSWORD"`, then
   `wrangler secret put STATUS_PASSWORD`. There is no lockout or attempt
   throttling in the worker — the WAF rule above is the brute-force
   backstop, so keep it even with auth on. Use a 20+ character password.
3. **Secret rotation.** Worker secrets: `wrangler secret put NAME`.
   Removing the `STATUS_PASSWORD` repo secret deletes the worker secret;
   the deploy refuses to proceed while the saved config still references
   it, so change or remove `auth` in the same change. If a token ever lands
   in the saved config, revoke it at the issuer — D1 keeps it until
   overwritten, and the old YAML may live in backups.
4. **After enabling auth**, re-check `/api/badge` consumers: badge URLs
   embedded as `<img>` tags cannot send Basic credentials and will break.

## Deliberately not handled

- No CSP: scripts and styles are inline by design; a strict policy needs
  a nonce/hash refactor first.
- No in-worker rate limiting: per-isolate memory is not shared state, so
  real throttling lives in the WAF rules above.
- Cron placement is Cloudflare's choice: probe origins are displayed,
  never assigned.
