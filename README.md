# Vigil

Vigil is an uptime monitor and status page that runs entirely on Cloudflare
Workers, written in Rust and compiled to WebAssembly.

A scheduled cron trigger wakes the worker every minute. Each tick probes
every due monitor over HTTP or TCP (concurrently, each raced against its
own deadline), records latency samples and downtime windows in a D1
database, and sends webhook notifications on state transitions. Probe
cadence is configurable per monitor. A dependency-free status page renders
hourly history bars, latency charts with axes, upkeep banners, and
per-monitor detail pages, alongside a small JSON API and SVG badges.

## Repository layout

```text
.
  core/     # pure monitoring logic: verdicts, incidents, rollups, views
  config/   # site.yaml: monitors, cadence, hooks, upkeep (compiled in)
  worker/   # Worker entry points: fetch + scheduled handlers, D1, probes
```

- `core` has no I/O or platform dependencies and is fully unit-tested on
  the host.
- `config` defines the title, links, monitors, probe cadence,
  notification hooks, upkeep windows, and optional access gate in
  `config/site.yaml` (embedded at build time — deploy after editing).
- `worker` owns fetching, storage, scheduling, and HTML/JSON rendering. It
  calls into `core` for all pure computation.

## Prerequisites

- Rust stable toolchain with the `wasm32-unknown-unknown` target:
  `rustup target add wasm32-unknown-unknown`
- Node.js 22+ with `npx` (used to run the Wrangler CLI)
- A Cloudflare account with Workers and D1 enabled

## Configure monitors

Edit `config/site.yaml` (full field reference with examples lives in the
comment header at the top of that file):

- Each monitor needs a stable unique `id` — history in D1 is keyed off it,
  so never rename an id casually.
- HTTP checks: set `check.via: http` with `url`, optional `method`,
  `headers`, `body`, an `expect` status allow-list (empty means any `2xx`),
  `has` / `lacks` keyword rules, and `timeout_ms` (1–60000).
- TCP checks: `check.via: tcp` with `host` and a non-zero `port`.
- Display: `tip` (hover tooltip), `href` (link on the name, opens in a new
  tab), `quiet_chart` (hide the latency chart). Each row shows the site's
  own `/favicon.ico` (with `favicon.png`/`icon.*` fallbacks) beside the name.
- Probe cadence: top-level `check_every_min` sets the default cadence in
  minutes; a monitor's own `every_min` overrides it. Unset (or `0` globally)
  means every tick. Explicit `every_min: 0` on a monitor is rejected as a
  typo. The cron itself always fires every minute and skips monitors that
  aren't due — first sightings spread across the interval so a fleet never
  synchronizes onto one tick.
- `links` render as chips in file order (first link is leftmost) and open
  in a new tab.
- Group monitors with `groups` (name to list of monitor ids). Monitors not
  listed in any group render under "Other".
- Notifications: add `notify.hooks` entries (`url`, `method`, `mode`, `text`
  template with `$NAME` / `$STATUS` / `$REASON` / `$TIME` placeholders),
  set `grace_min` for consecutive failing ticks before paging, and list
  ids in `mute` to silence them.
- Upkeep: add `upkeep` windows (`title`, `body`, `from`, optional `until`
  as unix seconds or RFC-3339, optional `only` monitor ids).
- Access gate: set `auth` with a `user` plus either a `pass_env`
  environment variable name or a cleartext `pass` (prefer the secret).

## Run checks and tests

From the repository root:

```sh
cargo test -p vigil-core -p vigil
cargo clippy --workspace --all-targets
cargo fmt --check
cargo check -p vigil-worker --target wasm32-unknown-unknown
```

## Set up the database and deploy

All Wrangler commands run from the `worker/` directory:

```sh
cd worker
npx wrangler d1 create vigil
```

Copy the returned `database_id` into `wrangler.toml`, replacing the
placeholder value. Then apply the schema:

```sh
npx wrangler d1 migrations apply DB
```

For local development with simulated scheduled events:

```sh
npx wrangler dev --test-scheduled
```

To deploy:

```sh
npx wrangler deploy
```

Optional: enable the access gate by storing the password as a secret
(matching the `pass_env` name in the config):

```sh
npx wrangler secret put STATUS_PASSWORD
```

## Deploy with GitHub Actions

`.github/workflows/deploy.yml` automates test → migrate → deploy on every
push to `main`, on manual dispatch, and every 6 hours (to re-pin the
`worker-build` toolchain). What it does:

1. Runs `cargo test -p vigil-core -p vigil` and
   `cargo check -p vigil-worker --target wasm32-unknown-unknown`.
2. Resolves the Cloudflare account id: uses the `CLOUDFLARE_ACCOUNT_ID`
   secret when set, otherwise looks it up via the API token.
3. Ensures the `vigil` D1 database exists (creates it on first run), pins
   its id into `worker/wrangler.toml`, and runs
   `wrangler d1 migrations apply DB --remote`.
4. Sets the `STATUS_PASSWORD` worker secret when that repo secret exists
   (skipped otherwise — the site stays public).
5. Runs `wrangler deploy` from `worker/`.

### Required secrets

Add these under **Settings → Secrets and variables → Actions** (or make
them [environment secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-environments)
if you want required reviewers before a deploy):

| Secret | Required | Purpose |
| --- | --- | --- |
| `CLOUDFLARE_API_TOKEN` | Yes | API token with **Workers Scripts: Edit**, **D1: Edit**, and **Account: Read** permissions ([create one here](https://dash.cloudflare.com/profile/api-tokens)). First run without `CLOUDFLARE_ACCOUNT_ID` needs the account lookup to succeed. |
| `CLOUDFLARE_ACCOUNT_ID` | No | Pins the deploy target; when omitted the workflow fetches it via the API token. Set it to skip the lookup and tighten token scopes. |
| `STATUS_PASSWORD` | No | Value for the `STATUS_PASSWORD` worker secret (Basic-auth password when `pass_env = "STATUS_PASSWORD"` is configured). Omit to keep the status page public. |

> Forks note: secrets are not available to pull-request runs from forks,
> so the workflow will fail closed there rather than leak credentials.

### Trigger a deploy

- Push to `main` (a `site.yaml` edit redeploys monitors).
- Or run it manually: **Actions → Deploy to Cloudflare → Run workflow**.
- To deploy from your machine instead, follow
  [Set up the database and deploy](#set-up-the-database-and-deploy) above.

## Routes

- `/` — status page: per-monitor rows with last-24-hour bars, latency
  charts with labeled axes, upkeep banners, footer links, and groups.
  Light/dark toggle included.
- `/monitor?id=<id>` — per-monitor detail: 90-day bars, latency chart with
  labeled axes, incident table, and upkeep scoped to that monitor.
- `/api/health` — JSON aggregate: up/down counts plus per-monitor status
  and day/week/month uptime percentages.
- `/api/badge?id=<id>` (or without `id` for the whole site) — SVG shield.
- `/api/history?id=<id>` — raw latency samples for charts.
- `/api/incidents?id=<id>&limit=<n>` — recent downtime windows.
- `/api/latency?id=<id>` — latency percentiles plus raw samples.
- `/favicon.ico` — tab icon generated from `worker/assets/vigil.svg` (public, long-cached).
- `/logo.svg` — header brand mark (same artwork as `worker/assets/vigil.svg`).

## Behavior notes

- Probes race against a per-monitor deadline and run concurrently, so one
  hung endpoint cannot stall the cron tick.
- Each tick writes one `samples` row and maintains open/closed `gaps`
  rows per probed monitor in D1, then prunes rows older than 90 days.
  Skipped monitors (cadence) record nothing; the published up/down counts
  only refresh on ticks that probed at least one monitor.
- Recovery always notifies; down-transitions notify only after
  `grace_min` consecutive failing ticks.
- History bars show honest gaps: days/hours before a monitor's first
  recorded sample render as "no data", never as 100% uptime.
- Every sample records its Cloudflare colo (resolved once per tick via
  `cdn-cgi/trace`); rows from before that change read `"CRON"`. The
  homepage and detail pages show the origin beside latency figures and in
  chart tooltips. Workers choose the colo — it can be displayed, not set.
- Workers have no raw ICMP/UDP sockets: front exotic checks with a small
  external prober that reports over HTTP.
- Keep roughly 50 or fewer monitors per minute; each cron tick is
  CPU-bounded. The release profile uses size optimizations
  (`opt-level = "z"`, LTO, `strip = "debuginfo"` — not `strip = true`,
  which breaks wasm-bindgen's externref transform); avoid heavy
  dependencies in the worker crate to keep the bundle small.
