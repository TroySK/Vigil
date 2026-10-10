use vigil_core::rollup::{bar_color, month_bars, percentile, sparkline, sparkline_points};
use vigil_core::state::Gap;

#[test]
fn bars_all_up_when_no_gaps() {
    let now = 90 * 86_400;
    let bars = month_bars(&[], now, 30);
    assert_eq!(bars.len(), 30);
    assert!(bars.iter().all(|b| (b.pct - 100.0).abs() < f64::EPSILON));
    assert_eq!(bar_color(100.0), "#2da44e");
    assert_eq!(bar_color(97.0), "#bf8700");
    assert_eq!(bar_color(50.0), "#cf222e");
}

#[test]
fn bars_reflect_half_day_outage() {
    let now = 2 * 86_400;
    let gaps = vec![Gap {
        at: 86_400,
        to: Some(86_400 + 43_200),
        why: "x".into(),
    }];
    let bars = month_bars(&gaps, now, 2);
    // Oldest first: yesterday had the 12h outage, today is clean.
    assert!((bars[0].pct - 50.0).abs() < 0.11);
    assert!((bars[1].pct - 100.0).abs() < f64::EPSILON);
}

#[test]
fn percentiles_and_spark() {
    assert_eq!(percentile(&[], 95.0), 0);
    assert_eq!(percentile(&[10, 20, 30, 40], 50.0), 20);
    assert_eq!(percentile(&[10, 20, 30, 40], 95.0), 40);
    let svg = sparkline(&[10, 20, 15], 90, 28);
    assert!(svg.contains("<polyline"));
    assert!(sparkline(&[], 90, 28).is_empty());
}

#[test]
fn spark_points_carry_time_tooltips() {
    let svg = sparkline_points(&[(1_000, 10), (2_000, 20)], 90, 28);
    assert!(svg.contains("<polyline"));
    assert!(svg.contains("<title>"));
    assert!(svg.contains("10ms"));
    assert!(svg.contains("20ms"));
    assert!(sparkline_points(&[], 90, 28).is_empty());
}
