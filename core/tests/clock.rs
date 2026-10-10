//! UTC calendar formatting vectors, including pre-epoch instants.

use vigil_core::clock::{date, short, utc};

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
