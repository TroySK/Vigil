//! UTC calendar formatting vectors, including pre-epoch instants.

use vigil_core::clock::{date, day_month, hour_min, rel, short, tip_stamp, utc};

#[test]
fn utc_full_stamp() {
    assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(utc(1_791_000_000), "2026-10-03 04:00:00 UTC");
    assert_eq!(utc(-1), "1969-12-31 23:59:59 UTC");
}

#[test]
fn date_only_drops_the_clock() {
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(1_791_000_000), "2026-10-03");
    assert_eq!(date(-86_400), "1969-12-31");
}

#[test]
fn short_axis_labels() {
    assert_eq!(short(0), "01-01 00:00");
    assert_eq!(short(1_791_000_000), "10-03 04:00");
    assert_eq!(short(-1), "12-31 23:59");
}

#[test]
fn chart_labels_use_day_month_and_hour_minute() {
    // 2026-06-12 14:30:45 UTC.
    let sec = 1_781_274_645;
    assert_eq!(day_month(sec), "12-June");
    assert_eq!(hour_min(sec), "14:30");
    assert_eq!(tip_stamp(sec), "12-June @ 14:30");
    assert_eq!(day_month(0), "01-January");
    assert_eq!(hour_min(0), "00:00");
}

#[test]
fn relative_age_floors_to_whole_minutes() {
    assert_eq!(rel(1_000, 1_000), "just now");
    assert_eq!(rel(1_000, 959), "just now");
    assert_eq!(rel(1_000, 940), "1 minute ago");
    assert_eq!(rel(1_000, 820), "3 minutes ago");
    assert_eq!(rel(1_000, 1_000 - 59 * 60), "59 minutes ago");
    assert_eq!(rel(1_000, 1_000 - 60 * 60), "1 hour ago");
    assert_eq!(rel(1_000, 1_000 - 5 * 3_600), "5 hours ago");
    assert_eq!(rel(1_000, 1_000 - 86_400), "1 day ago");
    assert_eq!(rel(1_000, 1_000 - 3 * 86_400), "3 days ago");
    // Future timestamps (clock skew) never read negative.
    assert_eq!(rel(1_000, 2_000), "just now");
}
