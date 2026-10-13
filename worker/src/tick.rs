//! Cron tick: probe every monitor, persist samples/gaps, page on transitions.
//!
//! One tick = one `samples` row + gap open/close maintenance per monitor,
//! plus webhook notifications with consecutive-failure grace. Old rows
//! beyond [`RETENTION_DAYS`] are pruned.

use std::collections::{BTreeMap, BTreeSet};

use futures_util::future::join_all;
use serde::Deserialize;
use vigil_core::checks::{judge, Say, Seen};
use vigil_core::config::{Hook, Monitor, Site};
use vigil_core::state::{Gap, Track};
use vigil_core::views;
use worker::d1::D1PreparedStatement;
use worker::{console_log, Date, Env, Error, Fetch, Headers, Method, Request, RequestInit, Result};

use crate::db::{num, schema_due, Db};
use crate::probe::{colo, probe, with_timeout};

/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;
/// How long samples and closed gaps are kept.
pub const RETENTION_DAYS: i64 = 90;

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
    let site = vigil::site();
    site.check().map_err(|errors| {
        Error::from(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    let db = Db::of(&env)?;
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
    let kv = kv_map(&db, &state_keys(&ids)).await?;
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
    for (start, end) in wave_bounds(due_monitors.len(), PROBE_WAVE) {
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
    // Retention sweeping runs on the first tick of each clock day: the
    // DELETEs cost round trips every tick otherwise, for data that only
    // ages daily. Folded into the same batch — no extra round trip.
    if now % (60 * 60 * 24) < 60 {
        writes.extend(prune_stmts(&db, now)?);
    }
    db.batch(writes).await?;
    for (name, notify_up, why) in &pending_hooks {
        notify(&site, name, *notify_up, why, now).await?;
    }
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

/// Every `kv` key one tick needs: cadence stamps plus alert counters for
/// all monitors, so the whole tick reads `kv` exactly once. (Counter rows
/// for monitors that never failed simply come back absent.)
fn state_keys(ids: &[&str]) -> Vec<String> {
    let mut keys = Vec::with_capacity(ids.len() * 3);
    for id in ids {
        keys.push(format!("last:{id}"));
        keys.push(format!("fails:{id}"));
        keys.push(format!("alert:{id}"));
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

/// One `kv` row for batched reads.
#[derive(Debug, Deserialize)]
struct KvRow {
    k: String,
    v: String,
}

/// Read many `kv` entries in one D1 round trip (empty input: no query).
async fn kv_map(db: &Db, keys: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if keys.is_empty() {
        return Ok(out);
    }
    let placeholders: Vec<String> = (0..keys.len()).map(|i| format!("?{}", i + 1)).collect();
    let stmt = db
        .q(&format!(
            "SELECT k, v FROM kv WHERE k IN ({})",
            placeholders.join(",")
        ))
        .bind(
            &keys
                .iter()
                .map(|key| key.clone().into())
                .collect::<Vec<wasm_bindgen::JsValue>>(),
        )?;
    let rows: Vec<KvRow> = stmt.all().await?.results()?;
    for row in rows {
        out.insert(row.k, row.v);
    }
    Ok(out)
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

/// Open (unterminated) gap per monitor id, from one batched query over the
/// due monitors. Monitors without an open gap are absent from the map.
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
    if ids.is_empty() {
        return Ok(out);
    }
    let stmt = db.q(&open_gaps_sql(ids.len())).bind(
        &ids.iter()
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
    Ok(out)
}

/// Pure cadence math behind [`due`]: due with no history, or once at least
/// `every_min` minutes elapsed since `last` (future stamps never count).
#[must_use]
pub fn is_due(last: Option<i64>, every_min: u64, now: i64) -> bool {
    let every_secs = every_min.saturating_mul(60);
    match last {
        None => true,
        Some(stamp) => {
            u64::try_from(now.saturating_sub(stamp)).unwrap_or(0) >= every_secs
        }
    }
}

/// Max concurrent probe subrequests per wave. A full-transition tick fires
/// probes plus one webhook each; waves of 10 keep that comfortably under
/// the 50-subrequest ceiling.
pub const PROBE_WAVE: usize = 10;

/// `[start, end)` index ranges partitioning `total` items into waves of at
/// most `wave` — so one burst never exhausts the subrequest budget.
fn wave_bounds(total: usize, wave: usize) -> Vec<(usize, usize)> {
    let wave = wave.max(1);
    (0..total).step_by(wave).map(|s| (s, (s + wave).min(total))).collect()
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
        alert_for, is_due, open_gaps_sql, plan_alert, plan_due, stagger_offset, state_keys,
        wave_bounds, Due,
    };
    use std::collections::BTreeMap;

    #[test]
    fn waves_partition_the_fleet() {
        assert_eq!(wave_bounds(30, 10), vec![(0, 10), (10, 20), (20, 30)]);
        assert_eq!(wave_bounds(25, 10), vec![(0, 10), (10, 20), (20, 25)]);
        assert_eq!(wave_bounds(0, 10), vec![]);
        assert_eq!(wave_bounds(3, 10), vec![(0, 3)]);
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
    fn state_keys_cover_cadence_and_alerts() {
        assert_eq!(
            state_keys(&["a", "b"]),
            vec![
                "last:a".to_owned(),
                "fails:a".to_owned(),
                "alert:a".to_owned(),
                "last:b".to_owned(),
                "fails:b".to_owned(),
                "alert:b".to_owned(),
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

/// Fire all configured webhooks for a transition. Best-effort by design:
/// delivery never fails the tick, and the alert marker is already in the
/// write batch before this runs, so a failed tick can't double-page.
async fn notify(site: &Site, name: &str, up: bool, why: &str, now: i64) -> Result<()> {
    let stamp = vigil_core::time_label(now);
    for hook in &site.notify.hooks {
        let body = views::fill(&hook.text, name, up, why, &stamp);
        fire_hook(hook, &body).await?;
    }
    Ok(())
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

/// Retention-window DELETEs as prepared statements, folded into the
/// end-of-tick write batch so sweeping costs no extra round trip.
fn prune_stmts(db: &Db, now: i64) -> Result<Vec<D1PreparedStatement>> {
    let cutoff = now.saturating_sub(RETENTION_DAYS.saturating_mul(SECS_PER_DAY));
    Ok(vec![
        db.q("DELETE FROM samples WHERE at < ?1")
            .bind(&[num(cutoff)])?,
        db.q("DELETE FROM gaps WHERE upto IS NOT NULL AND upto < ?1")
            .bind(&[num(cutoff)])?,
    ])
}
