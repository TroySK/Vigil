//! Split-cadence cron tick: probe every due monitor every 5 minutes for
//! uptime gaps, persist latency samples hourly, page on transitions.
//!
//! One tick = gap open/close maintenance per due monitor plus a `samples`
//! row only when the hourly sample gate fires (hourly charts need one dot
//! per hour, not one per 5-minute probe — this holds D1 rows-written
//! under quota), plus webhook notifications with consecutive-failure
//! grace. Old rows beyond [`RETENTION_SECS`](crate::db::RETENTION_SECS)
//! are pruned.

use std::collections::{BTreeMap, HashSet};

use futures_util::future::join_all;
use serde::Deserialize;
use vigil_core::checks::{judge, place, Say, Seen};
use vigil_core::config::{Hook, Monitor, Site};
use vigil_core::state::{Gap, Track};
use vigil_core::views;
use worker::d1::D1PreparedStatement;
use worker::{console_log, Date, Env, Fetch, Headers, Method, Request, RequestInit, Result};

use crate::db::{num, Db};
use crate::probe::{colo, probe, with_timeout};
use crate::reads::first_by_monitor;

/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Current time truncated to whole seconds.
#[allow(clippy::cast_possible_wrap)] // epoch millis fit in i64 for millennia
fn sec_now() -> i64 {
    (Date::now().as_millis() / 1_000) as i64
}

/// Probe all monitors once and persist the results.
///
/// D1 round trips dominate the tick's CPU budget (each crosses the
/// WASM/JS boundary), so reads fan in through three batched queries and
/// every write lands in one [`Db::batch`] call. The pre-batch design did
/// one round trip per monitor and got the isolate killed on the free-plan
/// CPU ceiling (`exceededCpu`) before the tick finished.
#[allow(clippy::too_many_lines)]
pub async fn tick(env: Env) -> Result<()> {
    // Editor mode pauses probing: the saved YAML may be half-edited, and
    // the status site is offline until `EDITOR=true` is removed.
    if crate::site_store::is_editor(&env) {
        console_log!("editor mode: skipping tick until EDITOR=true is removed");
        return Ok(());
    }
    let db = Db::of(&env)?;
    let loaded = match crate::site_store::load(&db).await? {
        Ok(loaded) => loaded,
        Err(e) => {
            console_log!("no valid site config in D1 ({e}): skipping tick");
            return Ok(());
        }
    };
    let site = &loaded.site;
    let now = sec_now();
    // One D1 round trip for every `kv` key the tick needs: cadence stamps,
    // alert counters, first-seen stamps, plus the schema marker (was: one
    // query per monitor for cadence, then two more per due monitor for
    // alerts). First sightings with a phase offset queue their marker
    // write into the end-of-tick batch instead of issuing it here.
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let mut state_query = state_keys(&ids);
    state_query.push("schema".to_owned());
    // Fresh databases have no `kv` table yet: `kv_map` fails before `init`
    // ever runs. Retry once after idempotent DDL instead of aborting the
    // first tick (transient D1 errors also retry once — `init` is
    // `IF NOT EXISTS`, so the extra DDL is harmless).
    let mut bootstrapped = false;
    let kv = match db.kv_map(&state_query).await {
        Ok(map) => map,
        Err(_) => {
            db.init().await?;
            bootstrapped = true;
            db.kv_map(&state_query).await?
        }
    };
    // Schema self-heal, once ever: CI migrations own DDL; the tick only
    // covers fresh/manual databases, records its marker in the same write
    // batch below, and never pays the DDL round trip again.
    let schema_ok = schema_current(kv.get("schema"));
    if !schema_ok && !bootstrapped {
        db.init().await?;
    }
    let mut writes: Vec<D1PreparedStatement> = Vec::new();
    // Single pass: due monitors collect in site order directly (was: an
    // id set plus a second filter pass over the fleet).
    let mut due_monitors: Vec<&Monitor> = Vec::new();
    for monitor in &site.monitors {
        let last = kv
            .get(&format!("last:{}", monitor.id))
            .and_then(|s| s.parse().ok());
        match plan_due(&monitor.id, last, site.cadence_min(monitor), now) {
            Due::Skip => {}
            Due::Probe => {
                due_monitors.push(monitor);
            }
            Due::Phase(key, value) => writes.push(db.put_stmt(&key, &value)?),
        }
    }
    // Probe every due monitor concurrently: 30 sequential fetches (each up
    // to `timeout_ms`) outlast the cron worker, so the tick died before
    // ever writing `counts`. `join_all` preserves order for the record
    // pass below. (`&Site` is a shared borrow: the worker runtime is
    // single-threaded so sharing across futures is safe.)
    // One lookup per tick: every probe in the wave shares this isolate.
    // (`Option<&str>` is `Copy`, so all thirty futures can hold it.)
    let colo_here = colo().await;
    // One location string per tick: every probe stamps the same origin
    // (was: an identical uppercase + alloc per monitor).
    let location = place(colo_here.as_deref());
    let mut probed = Vec::with_capacity(due_monitors.len());
    for (start, end) in wave_bounds(due_monitors.len(), probe_wave()) {
        // `probe` never throws: failures come back as `Seen::Dead`.
        let mut wave = join_all(
            due_monitors[start..end]
                .iter()
                .map(|monitor| async move { (*monitor, probe(monitor).await) }),
        )
        .await;
        probed.append(&mut wave);
    }
    // One more batched read covers gap maintenance for every due monitor
    // (was: one round trip per monitor).
    let due_ids: Vec<&str> = due_monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let gaps = open_gaps(&db, &due_ids).await?;
    // One batched backfill for monitors with samples but no `first:`
    // stamp (pre-existing fleets, first probes): steady-state ticks have
    // no missing stamps and skip this query entirely. Covers all monitors,
    // not just due ones, so a high-cadence monitor skipped this tick does
    // not sit gray until its own due tick.
    let missing_first: Vec<&str> = site
        .monitors
        .iter()
        .filter(|monitor| !kv.contains_key(&format!("first:{}", monitor.id)))
        .map(|monitor| monitor.id.as_str())
        .collect();
    let first_seen = first_by_monitor(&db, &missing_first).await?;
    let grace = i64::try_from(site.notify.grace_min.max(1)).unwrap_or(1);
    let mut up = 0_u32;
    let mut down = 0_u32;
    // Webhooks fire only after the write batch lands, so a failed tick
    // never pages without persisting its alert marker (and never pages
    // twice for one transition).
    let mut pending_hooks: Vec<(String, bool, String)> = Vec::new();
    let muted = muted_set(&site.notify.mute);
    for (monitor, seen) in &probed {
        let verdict = judge(monitor, seen);
        if verdict.up {
            up += 1;
        } else {
            down += 1;
        }
        let want_sample = sample_due(
            kv.get(&format!("sample:{}", monitor.id))
                .and_then(|s| s.parse().ok()),
            site.sample_cadence_min(monitor),
            now,
        );
        writes.extend(record_stmts(
            &db,
            &monitor.id,
            now,
            seen,
            &verdict,
            &location,
            gaps.get(monitor.id.as_str()),
            want_sample,
        )?);
        let (prev_fails, was_up) = alert_for(&kv, &monitor.id);
        let plan = plan_alert(prev_fails, was_up, verdict.up, grace);
        // Steady-up ticks dominate the fleet: rewriting an identical
        // `fails:0` every tick costs a D1 write per monitor for no state
        // change. Only persist when the counter actually moved.
        if plan.fails != prev_fails {
            writes.push(db.put_stmt(&format!("fails:{}", monitor.id), &plan.fails.to_string())?);
        }
        if let Some(marker) = plan.marker {
            writes.push(db.put_stmt(&format!("alert:{}", monitor.id), marker)?);
        }
        if !muted.contains(monitor.id.as_str()) {
            if let Some(notify_up) = plan.notify {
                pending_hooks.push((monitor.name.clone(), notify_up, verdict.why.clone()));
            }
        }
        writes.push(db.put_stmt(&format!("last:{}", monitor.id), &now.to_string())?);
        // Latency-sample stamp: only advance when a `samples` row was
        // actually written this tick, so the next probe tick re-checks
        // against the last real sample, not against "last probe".
        if want_sample {
            writes.push(db.put_stmt(&format!("sample:{}", monitor.id), &now.to_string())?);
        }
        // First-seen stamp for the history bars: backfilled from retained
        // samples once, then trusted forever (steady ticks find the key in
        // `kv` and write nothing).
        if !kv.contains_key(&format!("first:{}", monitor.id)) {
            let first = first_value(first_seen.get(monitor.id.as_str()), now);
            writes.push(db.put_stmt(&format!("first:{}", monitor.id), &first.to_string())?);
        }
    }
    // A skip-only tick probed nothing, so its 0/0 would overwrite the last
    // real counts with a lie: skipped monitors keep their state, making the
    // previous counts still accurate. Only publish when something probed.
    if up + down > 0 {
        writes.push(db.put_stmt("counts", &format!("{up}/{down}"))?);
        // Skip-only ticks probe nothing: without this marker the pages
        // would reload every tick on the tick clock even though no
        // displayed data changed (see the refresh poller).
        writes.push(db.put_stmt("data_at", &now.to_string())?);
    }
    writes.push(db.put_stmt("at", &now.to_string())?);
    if !schema_ok {
        writes.push(db.put_stmt("schema", crate::db::SCHEMA_VERSION)?);
    }
    // Retention sweeping runs once a day inside the off-peak window (see
    // prune_due): the DELETEs scan 90 days of rows, and running them at
    // midnight contended with peak fetch traffic. Chunked statements fold
    // into the same batch — no extra round trip, no giant statement.
    if prune_due(now) {
        writes.extend(prune_stmts(&db, now)?);
    }
    db.batch(writes).await?;
    notify_all(
        &site,
        &pending_hooks,
        now,
        hook_budget(due_monitors.len()),
    )
    .await;
    Ok(())
}

/// Whether the tick's schema self-heal already ran (marker present and
/// current): fresh/manual databases run DDL once, every later tick skips
/// it with no extra round trip (the marker rides the existing `kv` read).
fn schema_current(stored: Option<&String>) -> bool {
    stored.is_some_and(|version| version == crate::db::SCHEMA_VERSION)
}

/// Mute-list as a set for O(1) membership per monitor (was: a linear
/// scan per monitor per tick).
fn muted_set(mute: &[String]) -> HashSet<&str> {
    mute.iter().map(String::as_str).collect()
}

/// Cadence decision for one monitor: probe now, skip this tick, or record
/// a staggered first-sighting marker (`last:{id}` key/value) so a fleet
/// with the same cadence doesn't synchronize onto one tick.
#[derive(Debug, PartialEq, Eq)]
enum Due {
    Probe,
    Skip,
    Phase(String, String),
}

/// Pure cadence math behind the tick's due pass. `last` is the stored
/// `last:{id}` stamp, if any; see [`is_due`] for the interval rule.
///
/// First sightings spread across the interval by id hash (and into a
/// shared skip window). The default every-tick cadence always probes
/// immediately.
fn plan_due(id: &str, last: Option<i64>, every_min: u64, now: i64) -> Due {
    let every_secs = every_min.saturating_mul(60);
    match last {
        Some(_) if is_due(last, every_min, now) => Due::Probe,
        Some(_) => Due::Skip,
        None => {
            if every_min <= 1 {
                return Due::Probe;
            }
            let offset = stagger_offset(id, every_secs);
            if offset == 0 {
                return Due::Probe;
            }
            let phased = now.saturating_sub(i64::try_from(offset).unwrap_or(i64::MAX));
            Due::Phase(format!("last:{id}"), phased.to_string())
        }
    }
}

/// Every `kv` key one tick needs: cadence stamps, alert counters,
/// first-seen stamps, AND sample stamps for all monitors, so the whole
/// tick reads `kv` in as few queries as the param budget allows.
/// (Counter rows for monitors that never failed simply come back absent.)
fn state_keys(ids: &[&str]) -> Vec<String> {
    let mut keys = Vec::with_capacity(ids.len() * 5);
    for id in ids {
        keys.push(format!("last:{id}"));
        keys.push(format!("fails:{id}"));
        keys.push(format!("alert:{id}"));
        keys.push(format!("first:{id}"));
        keys.push(format!("sample:{id}"));
    }
    keys
}

/// Consecutive-failure count and stored up-state for one monitor from the
/// batched `kv` map. Missing keys mean "never failed, currently up",
/// matching the old per-monitor reads.
fn alert_for(map: &BTreeMap<String, String>, id: &str) -> (i64, bool) {
    let fails: i64 = map
        .get(&format!("fails:{id}"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let was_up = map.get(&format!("alert:{id}")).map_or(true, |s| s != "0");
    (fails, was_up)
}

/// Earliest-known sample time for a monitor lacking a `first:` stamp:
/// the retained minimum when backfilling a pre-existing fleet, else now
/// (first probe ever — the sample about to be written lands at `now`).
fn first_value(backfilled: Option<&i64>, now: i64) -> i64 {
    backfilled.copied().unwrap_or(now)
}

/// Exact query behind [`open_gaps`], exposed so tests execute the shipped
/// text against bundled SQLite. Only unterminated gaps leave D1 — the
/// record pass discards closed ones, so fetching 90 days of closed history
/// every tick only burns rows-read quota.
fn open_gaps_sql(n: usize) -> String {
    format!(
        "SELECT id, at, upto, why FROM gaps WHERE id IN ({}) AND upto IS NULL ORDER BY id ASC, at DESC",
        (0..n).map(|i| format!("?{}", i + 1)).collect::<Vec<_>>().join(",")
    )
}

/// Open (unterminated) gap per monitor id, from batched queries over the
/// due monitors (one query per [`MAX_PARAMS`](crate::db::MAX_PARAMS)-id
/// chunk — a single `IN (D)` query would exceed D1's 100-param budget on
/// large fleets). Monitors without an open gap are absent from the map.
async fn open_gaps(db: &Db, ids: &[&str]) -> Result<BTreeMap<String, Gap>> {
    #[derive(Debug, Deserialize)]
    struct GapIdRow {
        id: String,
        at: i64,
        #[serde(alias = "upto")]
        to: Option<i64>,
        why: String,
    }
    let mut out = BTreeMap::new();
    for chunk in ids.chunks(crate::db::MAX_PARAMS) {
        let stmt = db.q(&open_gaps_sql(chunk.len())).bind(
            &chunk
                .iter()
                .map(|id| (*id).into())
                .collect::<Vec<wasm_bindgen::JsValue>>(),
        )?;
        let rows: Vec<GapIdRow> = stmt.all().await?.results()?;
        for row in rows {
            // Newest first per id: the first row decides; older rows are history.
            // (The query only returns open gaps, so no closed-row filtering.)
            out.entry(row.id.clone()).or_insert_with(|| Gap {
                at: row.at,
                to: row.to,
                why: row.why.clone(),
            });
        }
    }
    Ok(out)
}

/// Whether a latency `samples` row is due: never sampled, or at least
/// `every_min` (sample cadence) elapsed since `last_sample`. Same interval
/// rule as [`is_due`] but over the independent `sample:` stamp, so uptime
/// probes can run every 5 minutes while charts stay hourly.
#[must_use]
pub fn sample_due(last_sample: Option<i64>, every_min: u64, now: i64) -> bool {
    is_due(last_sample, every_min, now)
}

/// Pure cadence math behind [`plan_due`]: due with no history, or once at
/// least `every_min` minutes elapsed since `last` (future stamps never
/// count).
#[must_use]
pub fn is_due(last: Option<i64>, every_min: u64, now: i64) -> bool {
    let every_secs = every_min.saturating_mul(60);
    match last {
        None => true,
        Some(stamp) => u64::try_from(now.saturating_sub(stamp)).unwrap_or(0) >= every_secs,
    }
}

/// Concurrent probes per wave. Concurrency only: the total probe count is
/// every due monitor, and the webhook budget (see [`hook_budget`]) yields
/// to it. 25 keeps small fleets parallel without one burst exhausting the
/// isolate's connection pool.
fn probe_wave() -> usize {
    PROBE_WAVE
}

/// `[start, end)` index ranges partitioning `total` items into waves of at
/// most `wave` — so one burst never exhausts the subrequest budget.
fn wave_bounds(total: usize, wave: usize) -> Vec<(usize, usize)> {
    let wave = wave.max(1);
    (0..total)
        .step_by(wave)
        .map(|s| (s, (s + wave).min(total)))
        .collect()
}

/// FNV-1a hash for staggering first-probe phases across monitors.
fn hash_id(id: &str) -> u64 {
    let mut hash: u64 = 14_695_981_039_346_656_037;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

/// First-sighting phase within `[0, every_secs)`: deterministic per id so
/// restarts don't reshuffle the fleet.
fn stagger_offset(id: &str, every_secs: u64) -> u64 {
    hash_id(id) % every_secs.max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        alert_for, cap_len, close_gap_sql, daily_upsert_sql, day_start, first_value, form_encode,
        gap_why_update, hook_budget, hook_host, is_due, muted_set, open_gaps_sql, plan_alert,
        plan_deliveries, plan_due, probe_wave, prune_daily_sql, prune_due, prune_gaps_sql,
        prune_samples_sql, region_upsert_sql, sample_due, schema_current, stagger_offset,
        state_keys, wave_bounds, why_gap_sql, Due, MAX_HOOK_DELIVERIES, SUBREQUEST_CEILING,
        SUBREQUEST_OVERHEAD,
    };
    use rusqlite::params_from_iter;
    use std::collections::BTreeMap;
    use vigil_core::config::Site;
    use vigil_core::state::Gap;

    #[test]
    fn form_payload_is_url_encoded() {
        assert_eq!(form_encode("ok"), "ok");
        assert_eq!(form_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(hook_host("https://example.com/hook"), "example.com");
        assert_eq!(hook_host("not a url"), "invalid-url");
    }

    #[test]
    fn gap_why_update_skips_identical_text() {
        // Steady failure with a fresh reason appends once...
        let fresh = Gap {
            at: 1,
            to: None,
            why: "refused".to_owned(),
        };
        assert_eq!(
            gap_why_update(&fresh, "timeout"),
            Some("refused | timeout".to_owned())
        );
        // ...but a capped reason that truncates back to the stored text
        // issues no write at all.
        let capped = Gap {
            at: 1,
            to: None,
            why: "x".repeat(500),
        };
        assert_eq!(gap_why_update(&capped, "timeout"), None);
    }

    #[test]
    fn gap_close_converges_dupes_at_once() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        // Legacy duplicate opens for m1 plus an unrelated monitor.
        for at in [100.0, 300.0] {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["m1", at, Option::<f64>::None, "old"],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["m2", 200.0, Option::<f64>::None, "old"],
        )
        .unwrap();
        conn.execute(why_gap_sql(), rusqlite::params!["new", "m1"])
            .unwrap();
        let whys: Vec<(f64, String)> = {
            let mut stmt = conn
                .prepare("SELECT at, why FROM gaps WHERE id = 'm1' ORDER BY at")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        // Newest open took the text; the older duplicate is untouched.
        assert_eq!(
            whys,
            vec![(100.0, "old".to_owned()), (300.0, "new".to_owned()),]
        );
        conn.execute(close_gap_sql(), rusqlite::params![999.0, "m1"])
            .unwrap();
        let open: Vec<f64> = {
            let mut stmt = conn
                .prepare("SELECT at FROM gaps WHERE id = 'm1' AND upto IS NULL ORDER BY at")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        // One recovery closes every open row: legacy duplicates converge
        // at once instead of draining one per tick while bars double-count
        // the remainder as downtime.
        assert_eq!(open, Vec::<f64>::new());
        // m2 untouched throughout.
        let m2: String = conn
            .query_row("SELECT why FROM gaps WHERE id = 'm2'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(m2, "old");
    }

    #[test]
    fn open_gaps_reads_chunk_within_param_budget() {
        // 150 ids exceed one query's param budget: split into chunks,
        // exactly like `open_gaps` must. Every chunk stays within
        // `MAX_PARAMS` and every open gap is found.
        let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        for id in &ids {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, 100.0, Option::<f64>::None, "x"],
            )
            .unwrap();
        }
        let mut found = 0_usize;
        for chunk in ids.chunks(crate::db::MAX_PARAMS) {
            assert!(!chunk.is_empty() && chunk.len() <= crate::db::MAX_PARAMS);
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let mut stmt = conn.prepare(&open_gaps_sql(chunk.len())).unwrap();
            let mut rows = stmt.query(params_from_iter(refs)).unwrap();
            while rows.next().unwrap().is_some() {
                found += 1;
            }
        }
        assert_eq!(found, 150);
    }

    #[test]
    fn waves_partition_the_fleet() {
        assert_eq!(wave_bounds(30, 10), vec![(0, 10), (10, 20), (20, 30)]);
        assert_eq!(wave_bounds(25, 10), vec![(0, 10), (10, 20), (20, 25)]);
        assert_eq!(wave_bounds(0, 10), vec![]);
        assert_eq!(wave_bounds(3, 10), vec![(0, 3)]);
    }

    #[test]
    fn probes_are_never_dropped_for_webhooks() {
        // Wave size bounds concurrency, not the number of probes. Every due
        // monitor still gets exactly one probe even when hooks fill the
        // subrequest budget.
        assert_eq!(probe_wave(), 25);
        let bounds = wave_bounds(60, probe_wave());
        assert_eq!(bounds.first().copied(), Some((0, 25)));
        assert_eq!(bounds.last().copied(), Some((50, 60)));
        assert_eq!(
            bounds.iter().map(|(start, end)| end - start).sum::<usize>(),
            60
        );
    }

    #[test]
    fn webhook_budget_never_overspends_the_subrequest_ceiling() {
        // Probes are the product feature; webhooks are best-effort. The
        // per-tick budget must leave room for colo + D1 even when every
        // monitor is due, and shrink deliveries instead of dropping probes.
        assert_eq!(hook_budget(0), 40);
        assert_eq!(hook_budget(10), 30);
        assert_eq!(hook_budget(30), 10);
        assert_eq!(hook_budget(45), 0);
        for due in 0..=60 {
            let hooks = hook_budget(due);
            assert!(hooks <= MAX_HOOK_DELIVERIES);
            assert!(
                due + hooks + SUBREQUEST_OVERHEAD <= SUBREQUEST_CEILING
                    || due + SUBREQUEST_OVERHEAD > SUBREQUEST_CEILING,
                "due={due} hooks={hooks}"
            );
        }
    }

    #[test]
    fn deliveries_cap_to_the_remaining_budget() {
        assert_eq!(cap_len(40, 9), 9);
        assert_eq!(cap_len(2, 9), 2);
        assert_eq!(cap_len(40, 0), 0);
    }

    #[test]
    fn region_upsert_keeps_only_the_newest_sample_per_place() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE region_latest (id TEXT NOT NULL, place TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, PRIMARY KEY (id, place));",
        )
        .unwrap();
        for (place, at, ms, up) in [("SIN", 10, 10, 1), ("SIN", 20, 30, 0), ("BOM", 15, 15, 1)] {
            conn.execute(
                region_upsert_sql(),
                rusqlite::params!["m", place, f64::from(at), f64::from(ms), f64::from(up)],
            )
            .unwrap();
        }
        let mut stmt = conn
            .prepare("SELECT place, at, ms, up FROM region_latest ORDER BY place")
            .unwrap();
        let rows: Vec<(String, i64, i64, i64)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("BOM".into(), 15, 15, 1), ("SIN".into(), 20, 30, 0)]
        );
    }

    #[test]
    fn daily_rollup_keeps_the_newest_sample_per_day() {
        assert_eq!(day_start(5), 0);
        assert_eq!(day_start(86_400 + 5), 86_400);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE monitor_daily (id TEXT NOT NULL, day INTEGER NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL, PRIMARY KEY (id, day));",
        )
        .unwrap();
        for (at, ms, place) in [(10, 10, "SIN"), (20, 20, "BOM")] {
            conn.execute(
                daily_upsert_sql(),
                rusqlite::params!["m", 0.0, f64::from(at), f64::from(ms), 1.0, place],
            )
            .unwrap();
        }
        let (at, ms, place): (i64, i64, String) = conn
            .query_row("SELECT at, ms, place FROM monitor_daily", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!((at, ms, place.as_str()), (20, 20, "BOM"));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM monitor_daily", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn daily_rollup_prune_respects_retention() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE monitor_daily (id TEXT NOT NULL, day INTEGER NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL, PRIMARY KEY (id, day));",
        )
        .unwrap();
        for day in [1_i64, 2, 3, 1_000] {
            conn.execute(
                "INSERT INTO monitor_daily VALUES ('m', ?1, ?1, 10, 1, 'SIN')",
                rusqlite::params![day],
            )
            .unwrap();
        }
        conn.execute(prune_daily_sql(), rusqlite::params![100.0, 2.0])
            .unwrap();
        let days: Vec<i64> = conn
            .prepare("SELECT day FROM monitor_daily ORDER BY day")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(days, vec![3, 1_000]);
    }

    #[test]
    fn prune_runs_in_the_off_peak_window() {
        // 03:00–03:05 UTC, not midnight: the DELETEs scan 90 days of rows
        // and must not contend with peak fetch traffic (P90 spikes).
        assert!(prune_due(3 * 3_600));
        assert!(prune_due(3 * 3_600 + 299));
        assert!(!prune_due(3 * 3_600 + 300));
        assert!(!prune_due(0));
        assert!(!prune_due(12 * 3_600));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn prune_chunks_delete_only_expired_rows() {
        // The shipped DELETE text against bundled SQLite: one chunk with
        // LIMIT 5 removes 5 of 7 expired rows and keeps fresh ones.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);
             CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        for at in [1_i64, 2, 3, 4, 5, 6, 7, 1_000] {
            conn.execute(
                "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["m", at as f64, 10.0, 1.0, "SIN"],
            )
            .unwrap();
        }
        conn.execute(prune_samples_sql(), rusqlite::params![100.0, 5.0])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 3);
        // Gaps: open rows and fresh closures survive; expired closures go.
        conn.execute(
            "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["m", 10.0, Option::<f64>::None, "open"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["m", 10.0, Some(50.0), "old"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["m", 10.0, Some(500.0), "fresh"],
        )
        .unwrap();
        conn.execute(prune_gaps_sql(), rusqlite::params![100.0, 10.0])
            .unwrap();
        let mut stmt = conn.prepare("SELECT why FROM gaps").unwrap();
        let kept: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(kept, vec!["open".to_owned(), "fresh".to_owned()]);
    }

    #[test]
    fn schema_marker_short_circuits_init() {
        assert!(!schema_current(None));
        assert!(!schema_current(Some(&"1".to_owned())));
        assert!(schema_current(Some(&crate::db::SCHEMA_VERSION.to_owned())));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn prune_deletes_oldest_first() {
        // Deterministic sweep: one LIMIT-5 chunk over 7 expired rows must
        // remove the 5 oldest, not an arbitrary 5.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id TEXT NOT NULL, at INTEGER NOT NULL, ms INTEGER NOT NULL, up INTEGER NOT NULL, place TEXT NOT NULL);
             CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        for at in [1_i64, 2, 3, 4, 5, 6, 7, 1_000] {
            conn.execute(
                "INSERT INTO samples VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["m", at as f64, 10.0, 1.0, "SIN"],
            )
            .unwrap();
        }
        conn.execute(prune_samples_sql(), rusqlite::params![100.0, 5.0])
            .unwrap();
        let mut stmt = conn.prepare("SELECT at FROM samples ORDER BY at").unwrap();
        let ats: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ats, vec![6, 7, 1_000]);
        // Gaps: oldest expired closures go first; open + fresh survive.
        for upto in [Some(10.0), Some(20.0), Some(500.0), None] {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["m", 5.0, upto, "x"],
            )
            .unwrap();
        }
        conn.execute(prune_gaps_sql(), rusqlite::params![100.0, 2.0])
            .unwrap();
        let mut stmt = conn.prepare("SELECT upto FROM gaps ORDER BY upto").unwrap();
        let uptos: Vec<Option<f64>> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(uptos, vec![None, Some(500.0)]);
    }

    #[test]
    fn muted_set_matches_by_id() {
        let mute = ["a".to_owned(), "b".to_owned()];
        let muted = muted_set(&mute);
        assert!(muted.contains("a"));
        assert!(muted.contains("b"));
        assert!(!muted.contains("c"));
        assert!(muted_set(&[]).is_empty());
    }

    #[test]
    fn due_without_history() {
        assert!(is_due(None, 1, 1_000));
        assert!(is_due(None, 60, 1_000));
    }

    #[test]
    fn due_after_interval_elapses() {
        assert!(is_due(Some(1_000), 1, 1_060));
        assert!(!is_due(Some(1_000), 1, 1_059));
        assert!(is_due(Some(1_000), 5, 1_300));
        assert!(!is_due(Some(1_000), 5, 1_299));
    }

    #[test]
    fn future_stamps_never_count_as_due() {
        assert!(!is_due(Some(2_000), 1, 1_000));
    }

    #[test]
    fn sample_gate_fires_hourly_on_five_minute_probes() {
        // Split cadence: uptime probes every 5m, `samples` rows hourly.
        // First probe always samples; a probe 5m later skips; an hour
        // later samples again. Future stamps never count.
        assert!(sample_due(None, 60, 10_000));
        assert!(!sample_due(Some(10_000), 60, 10_300));
        assert!(sample_due(Some(10_000), 60, 13_600));
        assert!(!sample_due(Some(20_000), 60, 10_000));
    }

    #[test]
    fn plan_due_probes_skips_and_phases() {
        // Due or overdue history probes; fresh history skips.
        assert_eq!(plan_due("a", Some(1_000), 1, 1_060), Due::Probe);
        assert_eq!(plan_due("a", Some(1_000), 1, 1_059), Due::Skip);
        // Every-tick cadence always probes, even on first sighting.
        assert_eq!(plan_due("a", None, 1, 1_000), Due::Probe);
        assert_eq!(plan_due("a", None, 0, 1_000), Due::Probe);
        // First sightings with a cadence either probe (zero offset) or
        // queue a staggered marker write for the end-of-tick batch.
        match plan_due("alpha", None, 5, 10_000) {
            Due::Probe => {}
            Due::Phase(key, value) => {
                assert_eq!(key, "last:alpha");
                let phased: i64 = value.parse().unwrap();
                assert!(phased <= 10_000 && phased > 10_000 - 300);
            }
            Due::Skip => panic!("first sighting never hard-skips"),
        }
    }

    #[test]
    fn deliveries_render_every_transition_hook_pair() {
        use vigil_core::config::{Hook, Notify};
        let site = Site {
            notify: Notify {
                hooks: vec![Hook {
                    url: "https://hooks.example/notify".to_owned(),
                    method: "POST".to_owned(),
                    mode: "json".to_owned(),
                    text: "[$STATUS] $NAME: $REASON".to_owned(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let pending = vec![
            ("A".to_owned(), false, "refused".to_owned()),
            ("B".to_owned(), true, "recovered".to_owned()),
        ];
        let deliveries = plan_deliveries(&site, &pending, 1_000);
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].1, "[DOWN] A: refused");
        assert_eq!(deliveries[1].1, "[UP] B: recovered");
        assert!(deliveries
            .iter()
            .all(|(hook, _)| hook.url == "https://hooks.example/notify"));
    }

    #[test]
    fn no_hooks_no_deliveries() {
        let site = Site::default();
        let pending = vec![("A".to_owned(), false, "refused".to_owned())];
        assert!(plan_deliveries(&site, &pending, 1_000).is_empty());
        assert!(plan_deliveries(&site, &[], 1_000).is_empty());
    }

    #[test]
    fn plan_alert_pages_once_per_transition() {
        // Steady up: reset the counter, page nothing, touch nothing.
        let steady = plan_alert(0, true, true, 2);
        assert_eq!(steady.fails, 0);
        assert_eq!(steady.marker, None);
        assert_eq!(steady.notify, None);
        // Recovery pages and flips the marker in the same batch.
        let recovery = plan_alert(3, false, true, 2);
        assert_eq!(recovery.fails, 0);
        assert_eq!(recovery.marker, Some("1"));
        assert_eq!(recovery.notify, Some(true));
        // First failure below grace: count, page nothing, touch nothing.
        let first = plan_alert(0, true, false, 2);
        assert_eq!(first.fails, 1);
        assert_eq!(first.marker, None);
        assert_eq!(first.notify, None);
        // Grace reached: page down and mark it.
        let page = plan_alert(1, true, false, 2);
        assert_eq!(page.fails, 2);
        assert_eq!(page.marker, Some("0"));
        assert_eq!(page.notify, Some(false));
        // Still down: refresh the marker, never re-page.
        let ongoing = plan_alert(5, false, false, 2);
        assert_eq!(ongoing.fails, 6);
        assert_eq!(ongoing.marker, Some("0"));
        assert_eq!(ongoing.notify, None);
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    fn open_gaps_sql_returns_only_unterminated() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gaps (id TEXT NOT NULL, at INTEGER NOT NULL, upto INTEGER, why TEXT NOT NULL);",
        )
        .unwrap();
        for (at, upto) in [(100, None), (200, Some(250)), (300, None)] {
            conn.execute(
                "INSERT INTO gaps VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["m1", at as f64, upto.map(|u| u as f64), "x"],
            )
            .unwrap();
        }
        let mut stmt = conn.prepare(&open_gaps_sql(1)).unwrap();
        let ats: Vec<i64> = stmt
            .query_map(rusqlite::params!["m1"], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ats, vec![300, 100]);
    }

    #[test]
    fn first_value_prefers_earliest_sample() {
        assert_eq!(first_value(Some(&1_000), 2_000), 1_000);
        assert_eq!(first_value(None, 2_000), 2_000);
    }

    #[test]
    fn state_keys_cover_cadence_alerts_and_samples() {
        assert_eq!(
            state_keys(&["a", "b"]),
            vec![
                "last:a".to_owned(),
                "fails:a".to_owned(),
                "alert:a".to_owned(),
                "first:a".to_owned(),
                "sample:a".to_owned(),
                "last:b".to_owned(),
                "fails:b".to_owned(),
                "alert:b".to_owned(),
                "first:b".to_owned(),
                "sample:b".to_owned(),
            ]
        );
        assert!(state_keys(&[]).is_empty());
    }

    #[test]
    fn alert_for_defaults_to_clean_upstream() {
        assert_eq!(alert_for(&BTreeMap::new(), "a"), (0, true));
        let map = BTreeMap::from([
            ("fails:a".to_owned(), "3".to_owned()),
            ("alert:a".to_owned(), "0".to_owned()),
            ("fails:b".to_owned(), "oops".to_owned()),
        ]);
        assert_eq!(alert_for(&map, "a"), (3, false));
        // Unparsable counters read as zero, never as failure.
        assert_eq!(alert_for(&map, "b"), (0, true));
    }

    #[test]
    fn stagger_is_deterministic_bounded_and_spread() {
        let a = stagger_offset("alpha", 300);
        assert_eq!(a, stagger_offset("alpha", 300));
        assert!(a < 300);
        let phases: std::collections::BTreeSet<u64> = (0..30)
            .map(|i| stagger_offset(&format!("monitor-{i}"), 300))
            .collect();
        assert!(phases.len() > 1, "offsets must spread, got {phases:?}");
    }
}

/// Cap for a gap's accumulated `why` text: each failing tick appends
/// `" | {reason}"`, so a month-long outage would otherwise grow one TEXT
/// value (and every copy of it) without bound.
pub const MAX_WHY_LEN: usize = 500;

/// Truncate `s` to at most [`MAX_WHY_LEN`] bytes on a char boundary.
fn cap_why(s: &str) -> &str {
    if s.len() <= MAX_WHY_LEN {
        return s;
    }
    let mut end = MAX_WHY_LEN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One sample row (only when `want_sample`) plus gap open/close maintenance,
/// as prepared statements for the end-of-tick write batch. `open` is this
/// monitor's unterminated gap, if any. Decision logic mirrors the old
/// executing version exactly.
fn record_stmts(
    db: &Db,
    id: &str,
    now: i64,
    seen: &Seen,
    verdict: &Say,
    location: &str,
    open: Option<&Gap>,
    want_sample: bool,
) -> Result<Vec<D1PreparedStatement>> {
    let mut out = Vec::with_capacity(4);
    if want_sample {
        let up_flag = i64::from(verdict.up);
        let elapsed_ms = i64::try_from(seen.ms).unwrap_or(i64::MAX);
        out.push(
            db.q("INSERT INTO samples (id, at, ms, up, place) VALUES (?1, ?2, ?3, ?4, ?5)")
                .bind(&[
                    id.into(),
                    num(now),
                    num(elapsed_ms),
                    num(up_flag),
                    location.into(),
                ])?,
        );
        // One representative row per UTC day backs long-range sparklines:
        // the newest sample wins, matching the raw-sample point it replaces.
        out.push(db.q(daily_upsert_sql()).bind(&[
            id.into(),
            num(day_start(now)),
            num(now),
            num(elapsed_ms),
            num(up_flag),
            location.into(),
        ])?);
        // Keep the detail page's per-region status a primary-key lookup
        // instead of a scan of every retained sample (see reads.rs).
        out.push(db.q(region_upsert_sql()).bind(&[
            id.into(),
            location.into(),
            num(now),
            num(elapsed_ms),
            num(up_flag),
        ])?);
    }
    let mut track = Track::default();
    if let Some(gap) = open {
        track.gaps.push(Gap {
            at: gap.at,
            to: None,
            why: gap.why.clone(),
        });
    }
    if verdict.up {
        if track.note_up(now) {
            out.push(db.q(close_gap_sql()).bind(&[num(now), id.into()])?);
        }
    } else if track.note_down(now, &verdict.why) {
        if let Some(gap) = open {
            // Steady failures with an unchanged (possibly capped) reason
            // skip the rewrite: an identical UPDATE every tick costs a D1
            // write per failing monitor for no state change.
            if let Some(why) = gap_why_update(gap, &verdict.why) {
                out.push(db.q(why_gap_sql()).bind(&[why.into(), id.into()])?);
            }
        } else {
            out.push(
                db.q("INSERT INTO gaps (id, at, upto, why) VALUES (?1, ?2, NULL, ?3)")
                    .bind(&[id.into(), num(now), verdict.why.clone().into()])?,
            );
        }
    }
    Ok(out)
}

/// Midnight UTC starting the day containing `at`.
fn day_start(at: i64) -> i64 {
    at - at.rem_euclid(SECS_PER_DAY)
}

/// Daily rollup upsert used by [`record_stmts`]. The newest sample in a
/// day replaces the previous representative (no averaging), so long-range
/// charts keep an honest point-in-time reading.
fn daily_upsert_sql() -> &'static str {
    "INSERT INTO monitor_daily (id, day, at, ms, up, place) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id, day) DO UPDATE SET at = excluded.at, ms = excluded.ms, up = excluded.up, place = excluded.place"
}

/// Per-region latest upsert used by [`record_stmts`]. Only one row per
/// monitor/origin survives, so the detail page reads a bounded table
/// instead of grouping the raw sample history.
fn region_upsert_sql() -> &'static str {
    "INSERT INTO region_latest (id, place, at, ms, up) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id, place) DO UPDATE SET at = excluded.at, ms = excluded.ms, up = excluded.up"
}

/// Close a monitor's open gaps: every unterminated row, so one recovery
/// converges legacy duplicates at once (was: newest-only, needing N ticks
/// to drain N dupes while bars double-counted the remainder as downtime).
fn close_gap_sql() -> &'static str {
    "UPDATE gaps SET upto = ?1 WHERE id = ?2 AND upto IS NULL"
}

/// Append a failure reason to one monitor's open gap: same newest-only
/// targeting as [`close_gap_sql`].
fn why_gap_sql() -> &'static str {
    "UPDATE gaps SET why = ?1 WHERE rowid = (SELECT rowid FROM gaps WHERE id = ?2 AND upto IS NULL ORDER BY at DESC LIMIT 1)"
}

/// Appended gap text for another failing tick, or `None` when the capped
/// text equals the stored text (steady failure at the cap: no write).
fn gap_why_update(open: &Gap, verdict_why: &str) -> Option<String> {
    let joined = format!("{} | {}", open.why, verdict_why);
    let capped = cap_why(&joined);
    (capped != open.why).then(|| capped.to_owned())
}

/// Webhook plan for one verdict: the `fails:` value to persist, the
/// `alert:` marker to persist (if any), and whether to page (with the
/// new state) after the write batch lands.
struct AlertPlan {
    fails: i64,
    marker: Option<&'static str>,
    notify: Option<bool>,
}

/// Pure alert bookkeeping behind the tick's alert pass.
///
/// Consecutive-failure grace: page on down-transitions only after
/// `grace` failing ticks in a row. A recovery always notifies (flap
/// control is the caller's job via mute, not ours).
fn plan_alert(prev_fails: i64, was_up: bool, verdict_up: bool, grace: i64) -> AlertPlan {
    if verdict_up {
        return AlertPlan {
            fails: 0,
            // A recovery flips the marker alongside the page; a steady up
            // leaves it untouched, exactly like the old executing version.
            marker: (!was_up).then_some("1"),
            notify: (!was_up).then_some(true),
        };
    }
    let fails = prev_fails.saturating_add(1);
    if was_up && fails >= grace {
        AlertPlan {
            fails,
            marker: Some("0"),
            notify: Some(false),
        }
    } else {
        AlertPlan {
            fails,
            marker: (!was_up).then_some("0"),
            notify: None,
        }
    }
}

/// Free-plan ceiling for subrequests made by one Worker invocation. Probes,
/// the colo lookup, D1 round trips, and webhook deliveries all share it.
pub const SUBREQUEST_CEILING: usize = 50;
/// Worst-case per-tick subrequests that are not probes or webhooks: colo
/// lookup (1), chunked `kv_map`/`open_gaps`/`first_by_monitor` reads, the
/// chunked write batch, and the daily prune batch. Kept conservative so a
/// fleet-wide flap can never push a tick over the ceiling.
pub const SUBREQUEST_OVERHEAD: usize = 10;
/// Concurrent probes per wave.
const PROBE_WAVE: usize = 25;

/// Deliveries left after every due probe and the fixed overhead. Probes are
/// never traded away for webhooks: when a fleet is large enough that the
/// probes alone fill the ceiling, this returns zero and the tick logs the
/// truncation. On the paid plan (1000 subrequests) the cap is still useful
/// as a burst limiter.
fn hook_budget(due_monitors: usize) -> usize {
    SUBREQUEST_CEILING
        .saturating_sub(SUBREQUEST_OVERHEAD.saturating_add(due_monitors))
        .min(MAX_HOOK_DELIVERIES)
}

/// Number of deliveries to keep from `total`, given the remaining budget.
fn cap_len(total: usize, budget: usize) -> usize {
    total.min(budget)
}

/// Fire all configured webhooks for every pending transition.
/// Best-effort by design: delivery never fails the tick, and the alert
/// markers are already in the write batch before this runs, so a failed
/// tick can't double-page. Deliveries run concurrently in small waves so
/// a fleet-wide flap doesn't serialize into minutes of wall time (each
/// hook still races its own [`HOOK_TIMEOUT_MS`] deadline). Failures are
/// logged (host + error, never the body) so `wrangler tail` shows drops.
async fn notify_all(site: &Site, pending: &[(String, bool, String)], now: i64, budget: usize) {
    let deliveries = plan_deliveries(site, pending, now);
    let keep = cap_len(deliveries.len(), budget);
    if keep < deliveries.len() {
        console_log!(
            "webhooks: truncating {} deliveries to {keep} (subrequest budget)",
            deliveries.len()
        );
    }
    let capped = &deliveries[..keep];
    for wave in capped.chunks(HOOK_WAVE) {
        let sends = wave.iter().map(|(hook, body)| async move {
            if let Err(e) = fire_hook(hook, body).await {
                console_log!("hook {} failed: {e}", hook_host(&hook.url));
            }
        });
        join_all(sends).await;
    }
}

/// Max concurrent webhook deliveries per wave: bounds subrequest bursts
/// on fleet-wide flaps without serializing the common single-page case.
pub const HOOK_WAVE: usize = 5;

/// Max total webhook deliveries per tick (see [`notify_all`]).
pub const MAX_HOOK_DELIVERIES: usize = 40;

/// Host for webhook failure logs (never the body).
fn hook_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid-url".to_owned())
}

/// Percent-encode a form value (`application/x-www-form-urlencoded`).
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
        }
    }
    out
}

/// Render every (transition × hook) delivery up front (pure): the send
/// phase is just concurrent I/O over this list.
fn plan_deliveries<'a>(
    site: &'a Site,
    pending: &[(String, bool, String)],
    now: i64,
) -> Vec<(&'a Hook, String)> {
    let stamp = vigil_core::time_label(now);
    let mut out = Vec::with_capacity(pending.len().saturating_mul(site.notify.hooks.len()));
    for (name, up, why) in pending {
        for hook in &site.notify.hooks {
            out.push((hook, views::fill(&hook.text, name, *up, why, &stamp)));
        }
    }
    out
}

/// Webhook delivery deadline: a hung receiver must not stall the tick.
pub const HOOK_TIMEOUT_MS: u64 = 10_000;

/// Deliver one webhook (`json` mode or form-encoded fallback). Single
/// timeout layer (the caller does not wrap again); errors propagate as
/// strings for logging — never fails the tick.
async fn fire_hook(hook: &Hook, body: &str) -> std::result::Result<(), String> {
    let method = match hook.method.to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "OPTIONS" => Method::Options,
        other => return Err(format!("unsupported method {other}")),
    };
    let mut init = RequestInit::new();
    init.with_method(method);
    if hook.mode == "json" {
        let headers = Headers::new();
        // Worker `Headers::set` only fails on an invalid name/value, which
        // cannot happen for this literal; log-and-continue keeps one bad
        // hook from failing the whole tick.
        if let Err(e) = headers.set("content-type", "application/json") {
            console_log!("hook header failed: {e}");
        }
        init.with_headers(headers);
        let payload = serde_json::json!({ "text": body }).to_string();
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&payload)));
    } else {
        let headers = Headers::new();
        if let Err(e) = headers.set("content-type", "application/x-www-form-urlencoded") {
            console_log!("hook header failed: {e}");
        }
        init.with_headers(headers);
        let payload = format!("text={}", form_encode(body));
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&payload)));
    }
    let req = Request::new_with_init(&hook.url, &init).map_err(|e| e.to_string())?;
    // Webhook delivery is best-effort: a failing or hanging endpoint must
    // not fail the cron tick or block other hooks.
    with_timeout(Fetch::Request(req).send(), HOOK_TIMEOUT_MS)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether retention sweeping runs on a tick landing at `now_secs`: the
/// first five minutes after 03:00 UTC, when fetch traffic is lowest.
fn prune_due(now_secs: i64) -> bool {
    const START: i64 = 3 * 3_600;
    const LEN: i64 = 300;
    (START..START + LEN).contains(&now_secs.rem_euclid(SECS_PER_DAY))
}

/// Rows removed per prune statement: the daily sweep pushes [`PRUNE_CHUNKS`]
/// bounded statements per table into the same write batch, draining a full
/// day of split-cadence volume (~9k gap/steady rows + ~1k hourly samples
/// at 30 monitors) without one giant DELETE stalling D1 mid-tick.
pub const PRUNE_CHUNK: i64 = 20_000;
/// Prune statements per table per daily sweep (see [`PRUNE_CHUNK`]).
pub const PRUNE_CHUNKS: usize = 5;

/// Retention-window DELETEs as prepared statements, folded into the
/// end-of-tick write batch so sweeping costs no extra round trip.
///
/// Each statement deletes at most [`PRUNE_CHUNK`] rows via an inner
/// `SELECT rowid ... LIMIT`: portable across SQLite/D1 (a bare
/// `DELETE ... LIMIT` is a dialect risk) and bounded per statement.
fn prune_stmts(db: &Db, now: i64) -> Result<Vec<D1PreparedStatement>> {
    let cutoff = now.saturating_sub(crate::db::RETENTION_SECS);
    let mut out = Vec::with_capacity(3 * PRUNE_CHUNKS);
    for _ in 0..PRUNE_CHUNKS {
        out.push(
            db.q(prune_samples_sql())
                .bind(&[num(cutoff), num(PRUNE_CHUNK)])?,
        );
        out.push(
            db.q(prune_gaps_sql())
                .bind(&[num(cutoff), num(PRUNE_CHUNK)])?,
        );
        out.push(
            db.q(prune_daily_sql())
                .bind(&[num(cutoff), num(PRUNE_CHUNK)])?,
        );
    }
    Ok(out)
}

/// Exact prune text, exposed so tests execute the shipped statements
/// against bundled SQLite. Oldest rows go first (`ORDER BY`): without it
/// each `LIMIT` chunk removes an arbitrary set and the sweep may never
/// converge on the oldest rows.
fn prune_samples_sql() -> &'static str {
    "DELETE FROM samples WHERE rowid IN (SELECT rowid FROM samples WHERE at < ?1 ORDER BY at ASC, rowid ASC LIMIT ?2)"
}

/// Exact prune text, exposed so tests execute the shipped statements
/// against bundled SQLite.
fn prune_gaps_sql() -> &'static str {
    "DELETE FROM gaps WHERE rowid IN (SELECT rowid FROM gaps WHERE upto IS NOT NULL AND upto < ?1 ORDER BY upto ASC, rowid ASC LIMIT ?2)"
}

/// Exact prune text for the daily rollup, exposed so tests execute the
/// shipped statement against bundled SQLite. One row per monitor per day
/// still grows forever without its own retention sweep.
fn prune_daily_sql() -> &'static str {
    "DELETE FROM monitor_daily WHERE rowid IN (SELECT rowid FROM monitor_daily WHERE day < ?1 ORDER BY day ASC, rowid ASC LIMIT ?2)"
}
