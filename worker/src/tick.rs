//! Cron tick: probe every monitor, persist samples/gaps, page on transitions.
//!
//! One tick = one `samples` row + gap open/close maintenance per monitor,
//! plus webhook notifications with consecutive-failure grace. Old rows
//! beyond [`RETENTION_DAYS`] are pruned.

use serde::Deserialize;
use vigil_core::checks::{judge, Say, Seen};
use vigil_core::config::{Hook, Site};
use vigil_core::state::{Gap, Track};
use vigil_core::views;
use worker::{console_log, Date, Env, Error, Fetch, Headers, Method, Request, RequestInit, Result};

use crate::db::{num, Db};
use crate::probe::probe;

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
    db.init().await?;
    let now = sec_now();
    let mut up = 0_u32;
    let mut down = 0_u32;
    for monitor in &site.monitors {
        // Cadence gate: the cron fires every minute, but a monitor set to
        // `every_min: N` (or the global `check_every_min`) only probes when
        // due. Skipped monitors keep their last state; nothing is recorded.
        if !due(&db, &monitor.id, site.cadence_min(monitor), now).await? {
            continue;
        }
        let (seen, location) = probe(monitor, None).await;
        let verdict = judge(monitor, &seen);
        if verdict.up {
            up += 1;
        } else {
            down += 1;
        }
        record(&db, &monitor.id, now, &seen, &verdict, &location).await?;
        db.put(&format!("last:{}", monitor.id), &now.to_string())
            .await?;
        if !site.notify.mute.iter().any(|muted| muted == &monitor.id) {
            maybe_alert(&db, &site, &monitor.id, &monitor.name, now, &verdict).await?;
        }
    }
    db.put("counts", &format!("{up}/{down}")).await?;
    db.put("at", &now.to_string()).await?;
    prune(&db, now).await?;
    Ok(())
}

/// Whether a monitor is due for probing: never probed before, or at least
/// `every_min` minutes elapsed since its last probe (tracked in `kv`).
async fn due(db: &Db, id: &str, every_min: u64, now: i64) -> Result<bool> {
    let last: Option<i64> = db
        .get(&format!("last:{id}"))
        .await?
        .and_then(|s| s.parse().ok());
    let Some(last) = last else {
        return Ok(true);
    };
    let elapsed = u64::try_from(now.saturating_sub(last)).unwrap_or(u64::MAX);
    Ok(elapsed >= every_min.saturating_mul(60))
}

/// Write one sample row and maintain the open/closed gap for a monitor.
async fn record(
    db: &Db,
    id: &str,
    now: i64,
    seen: &Seen,
    verdict: &Say,
    location: &str,
) -> Result<()> {
    let up_flag = i64::from(verdict.up);
    let elapsed_ms = i64::try_from(seen.ms).unwrap_or(i64::MAX);
    db.q("INSERT INTO samples (id, at, ms, up, place) VALUES (?1, ?2, ?3, ?4, ?5)")
        .bind(&[
            id.into(),
            num(now),
            num(elapsed_ms),
            num(up_flag),
            location.into(),
        ])?
        .run()
        .await?;
    let gaps: Vec<GapRow> = db
        .q("SELECT at, upto, why FROM gaps WHERE id = ?1 ORDER BY at DESC LIMIT 1")
        .bind(&[id.into()])?
        .all()
        .await?
        .results()?;
    let mut track = Track::default();
    if let Some(open) = gaps.first().filter(|g| g.to.is_none()) {
        track.gaps.push(Gap {
            at: open.at,
            to: None,
            why: open.why.clone(),
        });
    }
    if verdict.up {
        if track.note_up(now) {
            db.q("UPDATE gaps SET upto = ?1 WHERE id = ?2 AND upto IS NULL")
                .bind(&[num(now), id.into()])?
                .run()
                .await?;
        }
    } else if track.note_down(now, &verdict.why) {
        if let Some(open) = gaps.first().filter(|g| g.to.is_none()) {
            db.q("UPDATE gaps SET why = ?1 WHERE id = ?2 AND upto IS NULL")
                .bind(&[format!("{} | {}", open.why, verdict.why).into(), id.into()])?
                .run()
                .await?;
        } else {
            db.q("INSERT INTO gaps (id, at, upto, why) VALUES (?1, ?2, NULL, ?3)")
                .bind(&[id.into(), num(now), verdict.why.clone().into()])?
                .run()
                .await?;
        }
    }
    Ok(())
}

/// Latest gap row for gap-maintenance bookkeeping.
#[derive(Debug, Deserialize)]
struct GapRow {
    at: i64,
    #[serde(alias = "upto")]
    to: Option<i64>,
    why: String,
}

/// Page webhooks on down-transitions (after grace) and on every recovery.
async fn maybe_alert(
    db: &Db,
    site: &Site,
    id: &str,
    name: &str,
    now: i64,
    verdict: &Say,
) -> Result<()> {
    // Consecutive-failure grace: only page when a monitor has been down for
    // `grace_min` ticks in a row. A recovery always notifies (flap control
    // is the caller's job via mute, not ours).
    let fail_key = format!("fails:{id}");
    let fails: i64 = if verdict.up {
        db.put(&fail_key, "0").await?;
        0
    } else {
        let count: i64 = db
            .get(&fail_key)
            .await?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        db.put(&fail_key, &count.to_string()).await?;
        count
    };
    let state_key = format!("alert:{id}");
    let was_up: bool = db.get(&state_key).await?.map_or(true, |s| s != "0");
    if verdict.up {
        if !was_up {
            notify(db, site, &state_key, name, true, &verdict.why, now).await?;
        }
        return Ok(());
    }
    let grace = i64::try_from(site.notify.grace_min.max(1)).unwrap_or(1);
    if was_up && fails >= grace {
        notify(db, site, &state_key, name, false, &verdict.why, now).await?;
    } else if !was_up {
        // Still down: refresh the stored marker but don't re-page.
        db.put(&state_key, "0").await?;
    }
    Ok(())
}

/// Fire all configured webhooks, then persist the new alert state.
async fn notify(
    db: &Db,
    site: &Site,
    state_key: &str,
    name: &str,
    up: bool,
    why: &str,
    now: i64,
) -> Result<()> {
    let stamp = vigil_core::time_label(now);
    for hook in &site.notify.hooks {
        let body = views::fill(&hook.text, name, up, why, &stamp);
        fire_hook(hook, &body).await?;
    }
    db.put(state_key, if up { "1" } else { "0" }).await?;
    Ok(())
}

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
    // Webhook delivery is best-effort: a failing endpoint must not fail
    // the cron tick or block other hooks.
    let _ = Fetch::Request(req).send().await;
    Ok(())
}

/// Delete samples and closed gaps older than the retention window.
async fn prune(db: &Db, now: i64) -> Result<()> {
    let cutoff = now.saturating_sub(RETENTION_DAYS.saturating_mul(SECS_PER_DAY));
    db.q("DELETE FROM samples WHERE at < ?1")
        .bind(&[num(cutoff)])?
        .run()
        .await?;
    db.q("DELETE FROM gaps WHERE upto IS NOT NULL AND upto < ?1")
        .bind(&[num(cutoff)])?
        .run()
        .await?;
    Ok(())
}
