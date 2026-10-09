//! Maintenance windows: parse start/end moments and pick a banner state.

use std::fmt;

/// Seconds in one day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Banner state plus display text, or `None` when the window expired.
#[must_use]
pub fn upkeep_banner(upkeep: &crate::config::Upkeep, now: i64) -> Option<(&'static str, String)> {
    let from = parse_moment(&upkeep.from).unwrap_or(0);
    if let Some(end) = upkeep.until.as_deref().and_then(|s| parse_moment(s).ok()) {
        if now > end {
            return None;
        }
    }
    if now < from {
        return Some((
            "upcoming",
            format!("{} — scheduled {}", upkeep.title, crate::time_label(from)),
        ));
    }
    Some(("active", format!("{} — {}", upkeep.title, upkeep.body)))
}

/// Why a maintenance timestamp could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MomentError {
    /// Negative unix timestamp.
    NegativeTimestamp,
    /// Neither unix seconds nor RFC-3339.
    InvalidFormat(String),
}

impl fmt::Display for MomentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeTimestamp => write!(f, "timestamp must be >= 0"),
            Self::InvalidFormat(raw) => {
                write!(f, "invalid time `{raw}`; want unix seconds or RFC3339")
            }
        }
    }
}

impl std::error::Error for MomentError {}

/// Parse unix seconds or an RFC-3339 timestamp into unix seconds.
///
/// # Errors
/// Returns [`MomentError`] for negative timestamps or unparseable input.
pub fn parse_moment(raw: &str) -> Result<i64, MomentError> {
    let text = raw.trim();
    if let Ok(sec) = text.parse::<i64>() {
        if sec < 0 {
            return Err(MomentError::NegativeTimestamp);
        }
        return Ok(sec);
    }
    parse_rfc3339(text).ok_or_else(|| MomentError::InvalidFormat(raw.to_owned()))
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut zone_idx = None;
    for (i, b) in rest.bytes().enumerate() {
        if b == b'Z' || b == b'z' {
            zone_idx = Some(i);
            break;
        }
        if (b == b'+' || b == b'-') && i >= 8 {
            // Record the *last* `+`/`-` at time-position: the date half
            // never reaches here, so this must be the zone designator.
            zone_idx = Some(i);
        }
    }
    let (clock, zone) = match zone_idx {
        Some(i) => (rest.get(..i)?, rest.get(i..)?),
        None => (rest, "Z"),
    };
    let mut fields = clock.split(':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next().unwrap_or("0").parse().ok()?;
    let secs: i64 = fields
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    if fields.next().is_some() || hour > 23 || minute > 59 || secs > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(days * SECS_PER_DAY + hour * 3_600 + minute * 60 + secs - zone_offset(zone)?)
}

fn zone_offset(zone: &str) -> Option<i64> {
    if zone.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1_i64,
        b'-' => -1_i64,
        _ => return None,
    };
    let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
    let (hours, minutes) = match digits.len() {
        2 => (digits.parse::<i64>().ok()?, 0),
        4 => (
            digits.get(..2)?.parse::<i64>().ok()?,
            digits.get(2..)?.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    if hours > 14 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Civil-date math: `yoe` ∈ [0, 399], `mp` ∈ [0, 11], `doe` fits in
    // `i64` with huge margin; casts below cannot lose information.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = (year - era * 400) as u64; // [0, 399]
    let mp = ((month + 9) % 12) as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}
