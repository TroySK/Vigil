//! Cron tick: probe every monitor, persist samples/gaps, page on transitions.
//!
//! One tick = one `samples` row + gap open/close maintenance per monitor,
//! plus webhook notifications with consecutive-failure grace. Old rows
//! beyond [`RETENTION_SECS`](crate::db::RETENTION_SECS) are pruned.

use std::collections::{BTreeMap, BTreeSet};

use futures_util::future::join_all;
use serde::Deserialize;
use vigil_core::checks::{judge, Say, Seen};
use vigil_core::config::{Hook, Monitor, Site};
use vigil_core::state::{Gap, Track};
use vigil_core::views;
use worker::d1::D1PreparedStatement;
use worker::{console_log, Date, Env, Fetch, Headers, Method, Request, RequestInit, Result};

use crate::db::{num, schema_due, Db};
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
    let site = match crate::site_store::load(&db).await? {
        Ok(site) => site,
        Err(e) => {
            console_log!("no valid site config in D1 ({e}): skipping tick");
            return Ok(());
        }
    };
    let now = sec_now();
    // Schema setup runs hourly (see schema_due): CI migrations own it.
    if schema_due(now) {
        db.init().await?;
    }
    // One D1 round trip for every `kv` key the tick needs: cadence stamps
    // plus alert counters for all monitors (was: one query per monitor for
    // cadence, then two more per due monitor for alerts). First sightings
    // with a phase offset queue their marker write into the end-of-tick
    // batch instead of issuing it here.
    let ids: Vec<&str> = site
        .monitors
        .iter()
        .map(|monitor| monitor.id.as_str())
        .collect();
    let kv = db.kv_map(&state_keys(&ids)).await?;
    let mut writes: Vec<D1PreparedStatement> = Vec::new();
    let mut due_set = BTreeSet::new();
    for monitor in &site.monitors {
        let last = kv
            .get(&format!("last:{}", monitor.id))
            .and_then(|s| s.parse().ok());
        match plan_due(&monitor.id, last, site.cadence_min(monitor), now) {
            Due::Skip => {}
            Due::Probe => {
                due_set.insert(monitor.id.as_str());
            }
            Due::Phase(key, value) => writes.push(db.put_stmt(&key, &value)?),
        }
    }
    let due_monitors: Vec<&Monitor> = site
        .monitors
        .iter()
        .filter(|monitor| due_set.contains(monitor.id.as_str()))
        .collect();
    // Probe every due monitor concurrently: 30 sequential fetches (each up
    // to `timeout_ms`) outlast the cron worker, so the tick died before
    // ever writing `counts`. `join_all` preserves order for the record
    // pass below. (`&Site` is a shared borrow: the worker runtime is
    // single-threaded so sharing across futures is safe.)
    // One lookup per tick: every probe in the wave shares this isolate.
    // (`Option<&str>` is `Copy`, so all thirty futures can hold it.)
    let colo_here = colo().await;
    let colo_ref = colo_here.as_deref();
    let mut probed = Vec::with_capacity(due_monitors.len());
    for (start, end) in wave_bounds(due_monitors.len(), probe_wave(site.notify.hooks.len())) {
        // `probe` never throws: failures come back as `Seen::Dead`.
        let mut wave = join_all(due_monitors[start..end].iter().map(|monitor| async move {
            let (seen, location) = probe(monitor, colo_ref).await;
            (*monitor, seen, location)
        }))
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
    // One batched backfill for probed monitors with samples but no
    // `first:` stamp (pre-existing fleets, first probes): steady-state
    // ticks have no missing stamps and skip this query entirely.
    let missing_first: Vec<&str> = due_monitors
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
    for (monitor, seen, location) in &probed {
        let verdict = judge(monitor, seen);
        if verdict.up {
            up += 1;
        } else {
            down += 1;
        }
        writes.extend(record_stmts(
            &db,
            &monitor.id,
            now,
            seen,
            &verdict,
            location,
            gaps.get(monitor.id.as_str()),
        )?);
        let (prev_fails, was_up) = alert_for(&kv, &monitor.id);
        let plan = plan_alert(prev_fails, was_up, verdict.up, grace);
        // Steady-up ticks dominate the fleet: rewriting an identical
        // `fails:0` every minute costs a D1 write per monitor for no state
        // change. Only persist when the counter actually moved.
        if plan.fails != prev_fails {
            writes.push(db.put_stmt(&format!("fails:{}", monitor.id), &plan.fails.to_string())?);
        }
        if let Some(marker) = plan.marker {
            writes.push(db.put_stmt(&format!("alert:{}", monitor.id), marker)?);
        }
        if !site.notify.mute.iter().any(|muted| muted == &monitor.id) {
            if let Some(notify_up) = plan.notify {
                pending_hooks.push((monitor.name.clone(), notify_up, verdict.why.clone()));
            }
        }
        writes.push(db.put_stmt(&format!("last:{}", monitor.id), &now.to_string())?);
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
        // would reload every minute on the tick clock even though no
        // displayed data changed (see the refresh poller).
        writes.push(db.put_stmt("data_at", &now.to_string())?);
    }
    writes.push(db.put_stmt("at", &now.to_string())?);
    // Retention sweeping runs once a day inside the off-peak window (see
    // prune_due): the DELETEs scan 90 days of rows, and running them at
    // midnight contended with peak fetch traffic. Chunked statements fold
    // into the same batch — no extra round trip, no giant statement.
    if prune_due(now) {
        writes.extend(prune_stmts(&db, now)?);
    }
    db.batch(writes).await?;
    notify_all(&site, &pending_hooks, now).await;
    Ok(())
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

/// Every `kv` key one tick needs: cadence stamps, alert counters, and
/// first-seen stamps for all monitors, so the whole tick reads `kv` in as
/// few queries as the param budget allows. (Counter rows for monitors
/// that never failed simply come back absent.)
fn state_keys(ids: &[&str]) -> Vec<String> {
    let mut keys = Vec::with_capacity(ids.len() * 4);
    for id in ids {
        keys.push(format!("last:{id}"));
        keys.push(format!("fails:{id}"));
        keys.push(format!("alert:{id}"));
        keys.push(format!("first:{id}"));
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

/// Pure cadence math behind [`due`]: due with no history, or once at least
/// `every_min` minutes elapsed since `last` (future stamps never count).
#[must_use]
pub fn is_due(last: Option<i64>, every_min: u64, now: i64) -> bool {
    let every_secs = every_min.saturating_mul(60);
    match last {
        None => true,
        Some(stamp) => u64::try_from(now.saturating_sub(stamp)).unwrap_or(0) >= every_secs,
    }
}

/// Concurrent probes per wave, fitted to the 50-subrequest ceiling: one
/// colo lookup plus one request per webhook hook plus headroom, clamped so
/// small fleets still parallelize (25) and huge ones still progress (5).
/// A fixed wave of 10 serialized large fleets for no reason; the tail was
/// the sum of the slowest probe per wave.
fn probe_wave(hooks: usize) -> usize {
    50usize.saturating_sub(hooks.saturating_add(3)).clamp(5, 25)
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
        alert_for, first_value, is_due, open_gaps_sql, plan_alert, plan_deliveries, plan_due,
        probe_wave, prune_due, prune_gaps_sql, prune_samples_sql, stagger_offset, state_keys,
        wave_bounds, Due,
    };
    use rusqlite::params_from_iter;
    use std::collections::BTreeMap;
    use vigil_core::config::Site;

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
    fn probe_waves_fit_the_subrequest_budget() {
        // 50-subrequest ceiling minus colo + webhooks + headroom, clamped
        // so small fleets still parallelize and huge ones still progress.
        assert_eq!(probe_wave(0), 25);
        assert_eq!(probe_wave(5), 25);
        assert_eq!(probe_wave(22), 25);
        assert_eq!(probe_wave(23), 24);
        assert_eq!(probe_wave(40), 7);
        assert_eq!(probe_wave(100), 5);
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
    fn state_keys_cover_cadence_and_alerts() {
        assert_eq!(
            state_keys(&["a", "b"]),
            vec![
                "last:a".to_owned(),
                "fails:a".to_owned(),
                "alert:a".to_owned(),
                "first:a".to_owned(),
                "last:b".to_owned(),
                "fails:b".to_owned(),
                "alert:b".to_owned(),
                "first:b".to_owned(),
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

/// One sample row plus gap open/close maintenance, as prepared statements
/// for the end-of-tick write batch. `open` is this monitor's unterminated
/// gap, if any. Decision logic mirrors the old executing version exactly.
fn record_stmts(
    db: &Db,
    id: &str,
    now: i64,
    seen: &Seen,
    verdict: &Say,
    location: &str,
    open: Option<&Gap>,
) -> Result<Vec<D1PreparedStatement>> {
    let mut out = Vec::with_capacity(3);
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
            out.push(
                db.q("UPDATE gaps SET upto = ?1 WHERE id = ?2 AND upto IS NULL")
                    .bind(&[num(now), id.into()])?,
            );
        }
    } else if track.note_down(now, &verdict.why) {
        if let Some(gap) = open {
            let joined = format!("{} | {}", gap.why, verdict.why);
            let why = cap_why(&joined).to_owned();
            out.push(
                db.q("UPDATE gaps SET why = ?1 WHERE id = ?2 AND upto IS NULL")
                    .bind(&[why.into(), id.into()])?,
            );
        } else {
            out.push(
                db.q("INSERT INTO gaps (id, at, upto, why) VALUES (?1, ?2, NULL, ?3)")
                    .bind(&[id.into(), num(now), verdict.why.clone().into()])?,
            );
        }
    }
    Ok(out)
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

/// Fire all configured webhooks for every pending transition.
/// Best-effort by design: delivery never fails the tick, and the alert
/// markers are already in the write batch before this runs, so a failed
/// tick can't double-page. Deliveries run concurrently in small waves so
/// a fleet-wide flap doesn't serialize into minutes of wall time (each
/// hook still races its own [`HOOK_TIMEOUT_MS`] deadline).
async fn notify_all(site: &Site, pending: &[(String, bool, String)], now: i64) {
    for wave in plan_deliveries(site, pending, now).chunks(HOOK_WAVE) {
        let sends = wave.iter().map(|(hook, body)| async move {
            let _ = with_timeout(fire_hook(hook, body), HOOK_TIMEOUT_MS).await;
        });
        join_all(sends).await;
    }
}

/// Max concurrent webhook deliveries per wave: bounds subrequest bursts
/// on fleet-wide flaps without serializing the common single-page case.
pub const HOOK_WAVE: usize = 5;

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

/// Deliver one webhook (`json` mode or form-encoded fallback).
async fn fire_hook(hook: &Hook, body: &str) -> Result<()> {
    let mut init = RequestInit::new();
    init.with_method(Method::from(hook.method.clone()));
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
        let payload = format!("text={body}");
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&payload)));
    }
    let req = Request::new_with_init(&hook.url, &init)?;
    // Webhook delivery is best-effort: a failing or hanging endpoint must
    // not fail the cron tick or block other hooks.
    let _ = with_timeout(Fetch::Request(req).send(), HOOK_TIMEOUT_MS).await;
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
/// day of worst-case volume (~72k rows at 50 monitors) without one giant
/// DELETE stalling D1 mid-tick.
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
    let mut out = Vec::with_capacity(2 * PRUNE_CHUNKS);
    for _ in 0..PRUNE_CHUNKS {
        out.push(
            db.q(prune_samples_sql())
                .bind(&[num(cutoff), num(PRUNE_CHUNK)])?,
        );
        out.push(
            db.q(prune_gaps_sql())
                .bind(&[num(cutoff), num(PRUNE_CHUNK)])?,
        );
    }
    Ok(out)
}

/// Exact prune text, exposed so tests execute the shipped statements
/// against bundled SQLite.
fn prune_samples_sql() -> &'static str {
    "DELETE FROM samples WHERE rowid IN (SELECT rowid FROM samples WHERE at < ?1 LIMIT ?2)"
}

/// Exact prune text, exposed so tests execute the shipped statements
/// against bundled SQLite.
fn prune_gaps_sql() -> &'static str {
    "DELETE FROM gaps WHERE rowid IN (SELECT rowid FROM gaps WHERE upto IS NOT NULL AND upto < ?1 LIMIT ?2)"
}
