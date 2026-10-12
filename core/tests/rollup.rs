use vigil_core::rollup::{
    bar_color, month_bars, percentile, smooth_path, sparkline, sparkline_points,
};
use vigil_core::state::Gap;

#[test]
fn bars_all_up_when_no_gaps() {
    let now = 90 * 86_400;
    let bars = month_bars(&[], now, 30, Some(0));
    assert_eq!(bars.len(), 30);
    assert!(bars.iter().all(|b| (b.pct - 100.0).abs() < f64::EPSILON));
    assert!(bars.iter().all(|b| b.has_data));
    assert_eq!(bar_color(100.0), "#2da44e");
    assert_eq!(bar_color(97.0), "#bf8700");
    assert_eq!(bar_color(50.0), "#cf222e");
}

#[test]
fn bars_before_first_sample_have_no_data() {
    let now = 30 * 86_400;
    // First sample recorded at noon on the second-to-last day: only the
    // day containing it and today have data.
    let bars = month_bars(&[], now, 30, Some(29 * 86_400 + 43_200));
    assert_eq!(bars.len(), 30);
    assert!(bars[..28].iter().all(|b| !b.has_data));
    assert!(bars[28..].iter().all(|b| b.has_data));
    // No samples at all: every day is missing data, never fake 100%.
    let bars = month_bars(&[], now, 30, None);
    assert!(bars.iter().all(|b| !b.has_data));
}

#[test]
fn hour_bars_cover_last_24_hours() {
    use vigil_core::rollup::hour_bars;
    let now = 30 * 86_400 + 3_600;
    let bars = hour_bars(&[], now, 24, Some(0));
    assert_eq!(bars.len(), 24);
    assert!(bars.iter().all(|b| b.has_data));
    assert!(bars.iter().all(|b| (b.pct - 100.0).abs() < f64::EPSILON));
    // A 30-minute outage inside one hour cell halves that cell.
    let gaps = vec![Gap {
        at: now - 1_800,
        to: Some(now),
        why: "x".into(),
    }];
    let bars = hour_bars(&gaps, now, 24, Some(0));
    assert!((bars[22].pct - 50.0).abs() < 0.11);
    assert!((bars[21].pct - 100.0).abs() < f64::EPSILON);
    // No samples: every hour is missing data.
    let bars = hour_bars(&[], now, 24, None);
    assert!(bars.iter().all(|b| !b.has_data));
}
#[test]
fn date_label_is_date_only() {
    assert_eq!(vigil_core::date_label(0), "1970-01-01");
    assert_eq!(vigil_core::date_label(1_791_000_000), "2026-10-03");
}

#[test]
fn bars_reflect_half_day_outage() {
    let now = 2 * 86_400;
    let gaps = vec![Gap {
        at: 86_400,
        to: Some(86_400 + 43_200),
        why: "x".into(),
    }];
    let bars = month_bars(&gaps, now, 2, Some(0));
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
    assert!(svg.contains("<path"));
    assert!(sparkline(&[], 90, 28).is_empty());
}

#[test]
fn spark_points_carry_time_tooltips() {
    let svg = sparkline_points(&[(1_000, 10), (2_000, 20)], 90, 28);
    assert!(svg.contains("<path"));
    assert!(svg.contains("data-tip"));
    assert!(svg.contains("10ms"));
    assert!(svg.contains("20ms"));
    assert!(svg.contains("data-ts=\"1000\""));
    assert!(svg.contains("data-tk=\"full\""));
    assert!(sparkline_points(&[], 90, 28).is_empty());
}

#[test]
fn smooth_path_curves_through_every_point() {
    assert_eq!(smooth_path(&[]), "");
    assert_eq!(smooth_path(&[(3, 7)]), "M3,7");
    // Two points render a straight segment; three or more curve.
    let line = smooth_path(&[(0, 10), (90, 10)]);
    assert!(line.starts_with("M0,10"));
    assert!(line.ends_with("90,10"));
    let curve = smooth_path(&[(0, 20), (45, 5), (90, 20)]);
    assert!(curve.starts_with("M0,20"));
    assert!(curve.contains('C'));
    assert!(curve.ends_with("90,20"));
}

#[test]
fn latency_chart_has_labeled_axes() {
    use vigil_core::rollup::latency_chart;
    let svg = latency_chart(
        &[(1_000, 10, "BOM"), (2_000, 20, "SIN"), (3_000, 15, "BOM")],
        600,
        150,
    );
    assert!(svg.contains("20ms"));
    assert!(svg.contains("10ms"));
    assert!(svg.contains(">0<"));
    assert!(svg.contains("data-tip"));
    assert!(svg.contains("SIN"));
    assert!(svg.contains("data-ts=\"1000\""));
    assert!(svg.contains("data-tk=\"short\""));
    assert!(svg.contains("data-place=\"SIN\""));
    assert!(latency_chart(&[], 600, 150).is_empty());
}
