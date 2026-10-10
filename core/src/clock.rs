//! Dependency-free UTC calendar formatting (no `chrono`/`time` in core).

/// Seconds in a civil day.
pub const SECS_PER_DAY: i64 = 86_400;
/// Seconds in an hour / minute.
pub const SECS_PER_HOUR: i64 = 3_600;
/// Seconds in a minute.
pub const SECS_PER_MINUTE: i64 = 60;

/// Format seconds since the Unix epoch as `YYYY-MM-DD` (date only).
#[must_use]
pub fn date(sec: i64) -> String {
    let (year, month, day) = civil(sec.div_euclid(SECS_PER_DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Format seconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Uses Howard Hinnant's civil-date algorithm;pure integer math so it
/// works on every target (including `wasm32-unknown-unknown`).
#[must_use]
pub fn utc(sec: i64) -> String {
    let days = sec.div_euclid(SECS_PER_DAY);
    let clock = sec.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        clock / SECS_PER_HOUR,
        clock % SECS_PER_HOUR / SECS_PER_MINUTE,
        clock % SECS_PER_MINUTE
    )
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]
fn civil(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm: `doe`, `yoe`, `doy`, `mp` are all
    // provably in tiny ranges; the casts below cannot lose information.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let doe = (days - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}
