# Vigil

Vigil is an uptime monitor and status page that runs entirely on Cloudflare
Workers, written in Rust and compiled to WebAssembly.

A scheduled cron trigger wakes the worker every 5 minutes. Each tick probes
every due monitor over HTTP or TCP (concurrently, each raced against its
own deadline), records hourly latency samples and 5-minute downtime windows
in a D1 database, and sends webhook notifications on state transitions.
Uptime cadence is 5 minutes at minimum (per monitor or global); latency
samples stay hourly to hold D1 rows down. A dependency-free status page renders
hourly history bars, latency charts with axes, upkeep banners, and
per-monitor detail pages, alongside a small JSON API and SVG badges.

## Repository layout

```text
.
  core/     # pure monitoring logic: verdicts, incidents, rollups, views
  config/   # site config parsing helpers + site.example.yaml template
  worker/   # Worker entry points: fetch + scheduled handlers, D1, probes
```

- `core` has no I/O or platform dependencies and is fully unit-tested on
  the host.
- `config` offers YAML parsing/validation helpers. The live config is
  NOT in git: `config/site.yaml` is git-ignored and lives only in the
  D1 `kv` table (key `site_yaml`), edited through the worker's config
  editor (see [Configure monitors](#configure-monitors)).
  `config/site.example.yaml` is the committed template + field reference.
- `worker` owns fetching, storage, scheduling, and HTML/JSON rendering. It
  calls into `core` for all pure computation.

## Prerequisites

- Rust stable toolchain with the `wasm32-unknown-unknown` target:
  `rustup target add wasm32-unknown-unknown`
- Node.js 22+ with `npx` (used to run the Wrangler CLI)
- A Cloudflare account with Workers and D1 enabled

## Configure monitors

The site config (title, links, monitors, cadence, hooks, upkeep, auth
gate) is stored in the D1 database — not in git. To change it, enable
the config editor. Two ways to set `EDITOR=true` on the worker:

- **From GitHub** (recommended): set the `EDITOR` repo secret
  (Settings → Secrets and variables → Actions) to exactly `true`, then
  push or run the workflow manually. The workflow syncs it into the
  worker secret before deploying.
- **From your machine**: `npx wrangler secret put EDITOR` (value
  `true`) from `worker/`, then deploy.

Then:

```sh
# 1. Open https://<your-worker>/editor and paste your YAML
#    (full field reference with examples lives in
#    `config/site.example.yaml`; your previous `config/site.yaml`
#    contents work verbatim). Saving validates before persisting.
# 2. IMPORTANT: disable editor mode again and re-deploy — via GitHub,
#    delete (or empty) the EDITOR repo secret and re-run the workflow;
#    locally, `npx wrangler secret delete EDITOR` then redeploy.
```

**The app does not work while `EDITOR=true` is set** — and that is
deliberate. In editor mode every route (pages, APIs) renders the editor
instead, and the cron tick skips probing. The editor page says so too.
The editor is also unauthenticated (there may be no valid config to gate
against yet), so keep the window short: save, remove the var, redeploy.

Alternative: set the `SITE_YAML` repo secret (Settings → Secrets and
variables → Actions) to the full YAML contents. The deploy workflow
then seeds D1 from the secret on every deploy — no editor round-trip
needed. When the secret is set it is the source of truth: each deploy
overwrites the D1 value, so editor edits made between deploys get
replaced. Leave the secret unset to manage the config purely through
the editor.

Config rules (same YAML shape as before):

- Each monitor needs a stable unique `id` — history in D1 is keyed off it,
  so never rename an id casually.
- HTTP checks: set `check.via: http` with `url`, optional `method`,
  `headers`, `body`, an `expect` status allow-list (empty means any `2xx`),
  `has` / `lacks` keyword rules, and `timeout_ms` (1–60000).
- TCP checks: `check.via: tcp` with `host` and a non-zero `port`.
- Display: `tip` (hover tooltip), `href` (link on the name, opens in a new
  tab), `quiet_chart` (hide the latency chart). Each row shows the site's
  own `/favicon.ico` (with `favicon.png`/`favicon.svg` fallbacks) beside the name.
- Probe cadence (split): top-level `check_every_min` sets the default
  uptime-probe cadence in minutes; a monitor's own `uptime_every_min`
  (legacy `every_min`) overrides it. Both clamp to a 5-minute floor so
  downtime pages fast without burning the D1 rows quota.
  `sample_every_min` (global + per-monitor) sets the latency-sample
  cadence separately — unset means hourly, so charts and percentiles
  write ~12x fewer `samples` rows than 5-minute probing. Samples ride on
  probes and never run faster than the uptime cadence.
  The cron itself fires every 5 minutes and skips monitors that
  aren't due — first sightings spread across the interval so a fleet never
  synchronizes onto one tick.
- `refresh_secs`: page auto-refresh interval in seconds (default 300,
  `0` disables polling). Pages poll on this interval but reload only when
  a tick probed new data.
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

On a fresh database there is no site config yet, so the pages show a
"config missing" notice. Seed it via the editor flow
([Configure monitors](#configure-monitors)): deploy once with
`EDITOR = "true"`, paste your YAML at `/editor`, save, then **remove
the var and redeploy**.

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
push to `main` and on manual dispatch. What it does:

1. Runs `cargo test -p vigil-core -p vigil` and
   `cargo check -p vigil-worker --target wasm32-unknown-unknown`.
2. Requires the `CLOUDFLARE_ACCOUNT_ID` secret to be set and fails fast
   otherwise.
3. Refuses to deploy when `EDITOR = ...` is uncommented in
   `worker/wrangler.toml` — editor mode takes the live site offline, so
   it must never reach production through git.
4. Ensures the `vigil` D1 database exists (creates it on first run), pins
   its id into `worker/wrangler.toml`, and runs
   `wrangler d1 migrations apply DB --remote`.
5. Seeds the site config into D1 from the `SITE_YAML` repo secret when
   that secret exists (syntax-checked first; skipped otherwise — then
   the `EDITOR=true` editor flow owns the config).
6. Sets the `STATUS_PASSWORD` worker secret when that repo secret exists
   (skipped otherwise — the site stays public).
7. Syncs the `EDITOR` worker secret from the `EDITOR` repo secret
   (`true` enables editor mode; unset deletes the worker secret so a
   stale flag can't keep the site offline).
8. Runs `wrangler deploy` from `worker/`.

### Required secrets

Add these under **Settings → Secrets and variables → Actions** (or make
them [environment secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-environments)
if you want required reviewers before a deploy):

| Secret | Required | Purpose |
| --- | --- | --- |
| `CLOUDFLARE_API_TOKEN` | Yes | API token with **Workers Scripts: Edit** and **D1: Edit** permissions ([create one here](https://dash.cloudflare.com/profile/api-tokens)). |
| `CLOUDFLARE_ACCOUNT_ID` | Yes | Pins the deploy target (find it in the Cloudflare dashboard URL or **Workers & Pages → Overview**). The workflow fails fast when it is missing. |
| `STATUS_PASSWORD` | No | Value for the `STATUS_PASSWORD` worker secret (Basic-auth password when `pass_env = "STATUS_PASSWORD"` is configured). Omit to keep the status page public. |
| `SITE_YAML` | No | Full site-config YAML, seeded into D1 (`kv.site_yaml`) on every deploy. When set, it overwrites editor edits each deploy. Omit to manage config purely through the `EDITOR=true` editor flow. |
| `EDITOR` | No | Set to exactly `true` to sync the `EDITOR` worker secret on deploy, taking the live site offline into the config editor. Delete or empty it and re-run the workflow to restore the site. |

> Forks note: secrets are not available to pull-request runs from forks,
> so the workflow will fail closed there rather than leak credentials.

### Trigger a deploy

- Push to `main` (code changes redeploy the worker; config changes go
  through the [`EDITOR=true` flow](#configure-monitors), not git).
- Or run it manually: **Actions → Deploy to Cloudflare → Run workflow**.
- To deploy from your machine instead, follow
  [Set up the database and deploy](#set-up-the-database-and-deploy) above.

## Routes

- `/` — status page: per-monitor rows with history bars and latency
  charts for the selected range (`?range=24h|7d|30d|90d`, default 7d),
  upkeep banners, footer links, and groups. Light/dark toggle included.
- `/monitor?id=<id>` — per-monitor detail: range bars, latency chart with
  labeled axes, incident table, and upkeep scoped to that monitor.
- `/api/health` — JSON aggregate: up/down counts plus per-monitor status
  and day/week/month uptime percentages.
- `/api/badge?id=<id>` (or without `id` for the whole site) — SVG shield.
- `/api/history?id=<id>` — raw latency samples for charts.
- `/api/incidents?id=<id>&limit=<n>` — recent downtime windows.
- `/api/latency?id=<id>` — latency percentiles plus raw samples.
- `/favicon.ico` — tab icon generated from `worker/assets/vigil.svg` (public, long-cached).
- `/logo.svg` — header brand mark (same artwork as `worker/assets/vigil.svg`).
- `/editor` — copy-paste YAML config editor. Served **only** when the
  `EDITOR=true` worker var is set (otherwise 404); while it is set, all
  other routes render the editor and cron probes pause. Remove the var
  and redeploy to restore the site.

## Behavior notes

- Probes race against a per-monitor deadline and run concurrently, so one
  hung endpoint cannot stall the cron tick.
- Each tick maintains open/closed `gaps` rows per probed monitor and writes
  one `samples` row only when the hourly sample gate fires, then prunes
  rows older than 90 days. Skipped monitors (cadence) record nothing; the
  published up/down counts only refresh on ticks that probed at least one
  monitor.
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
