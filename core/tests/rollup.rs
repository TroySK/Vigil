use vigil_core::rollup::{
    bar_color, month_bars, percentile, smooth_path, sparkline, sparkline_points,
};
use vigil_core::state::Gap;

#[test]
fn bars_all_up_when_no_gaps() {
    let now = 90 * 86_400;
    let bars = month_bars(&Vec::<Gap>::new(), now, 30, Some(0));
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
    let bars = month_bars(&Vec::<Gap>::new(), now, 30, Some(29 * 86_400 + 43_200));
    assert_eq!(bars.len(), 30);
    assert!(bars[..28].iter().all(|b| !b.has_data));
    assert!(bars[28..].iter().all(|b| b.has_data));
    // No samples at all: every day is missing data, never fake 100%.
    let bars = month_bars(&Vec::<Gap>::new(), now, 30, None);
    assert!(bars.iter().all(|b| !b.has_data));
}

#[test]
fn hour_bars_cover_last_24_hours() {
    use vigil_core::rollup::hour_bars;
    let now = 30 * 86_400 + 3_600;
    let bars = hour_bars(&Vec::<Gap>::new(), now, 24, Some(0));
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
    let bars = hour_bars(&Vec::<Gap>::new(), now, 24, None);
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
    assert_eq!(svg.matches("r=\"2\"").count(), 3);
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
    assert!(svg.contains("data-tk=\"tip\""));
    assert!(svg.contains("@ 00:16 UTC"));
    assert_eq!(svg.matches("class=\"dot\"").count(), 2);
    assert_eq!(svg.matches("class=\"hit\"").count(), 2);
    assert!(sparkline_points(&[], 90, 28).is_empty());
}

#[test]
fn smooth_path_interpolates_without_overshoot() {
    assert_eq!(smooth_path(&[]), "");
    assert_eq!(smooth_path(&[(3, 7)]), "M3,7");
    // Two points render a straight segment.
    assert_eq!(smooth_path(&[(0, 10), (90, 10)]), "M0,10 L90,10");
    let curve = smooth_path(&[(0, 20), (45, 5), (90, 20)]);
    assert!(curve.starts_with("M0,20"));
    // Interpolating: every input point is a segment endpoint, so sample
    // dots sit on the line.
    let ends: Vec<&str> = curve
        .split(['M', 'C', 'L', ' '])
        .filter(|t| t.contains(','))
        .collect();
    for (x, y) in [(0, 20), (45, 5), (90, 20)] {
        assert!(
            ends.contains(&format!("{x},{y}").as_str()),
            "missing {x},{y}: {curve}"
        );
    }
    // A spike to the canvas edge must not overshoot outside it: every
    // coordinate stays within the inputs' range (no negative values).
    let spiky = smooth_path(&[(0, 20), (45, 0), (90, 20)]);
    assert!(!spiky.contains('-'));
}

#[test]
fn latency_chart_has_labeled_axes() {
    use vigil_core::rollup::latency_chart;
    let points = &[(1_000, 10, "BOM"), (2_000, 20, "SIN"), (3_000, 15, "BOM")];
    let svg = latency_chart(points, 600, 150, false);
    assert!(svg.contains("20ms"));
    assert!(svg.contains("10ms"));
    assert!(svg.contains(">0<"));
    assert!(svg.contains("data-tip"));
    assert!(svg.contains("SIN"));
    assert!(svg.contains("data-ts=\"1000\""));
    assert!(svg.contains("data-tk=\"day\""));
    // Day ranges label axes `DD-Month`; the 24-hour view uses `HH:MM`.
    assert!(svg.contains("01-January"));
    assert!(svg.contains("data-tip=\"01-January @ 00:16 UTC"));
    assert_eq!(svg.matches("class=\"dot\"").count(), 3);
    assert_eq!(svg.matches("class=\"hit\"").count(), 3);
    let hourly = latency_chart(points, 600, 150, true);
    assert!(hourly.contains("data-tk=\"hour\""));
    assert!(hourly.contains(">00:16<"));
    assert!(latency_chart(&[], 600, 150, false).is_empty());
}

/// Reason-less downtime window: bar rollups only read bounds, so callers
/// must not have to clone reason strings to paint bars.
struct Span(i64, Option<i64>);

impl vigil_core::state::GapSpan for Span {
    fn at(&self) -> i64 {
        self.0
    }
    fn to(&self) -> Option<i64> {
        self.1
    }
}

/// Naive O(cells x gaps) overlap math: the pre-sweep algorithm, kept as a
/// differential oracle for the linear engine.
#[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
fn naive_cells(
    gaps: &[(i64, Option<i64>)],
    now: i64,
    cells: usize,
    width: i64,
    t0: i64,
    since: Option<i64>,
) -> Vec<(i64, f64, bool)> {
    let mut out = Vec::with_capacity(cells);
    for i in 0..cells {
        let start = t0 + i as i64 * width;
        let end = start + width;
        let has_data = since.is_some_and(|s| end > s);
        let mut down = 0_i64;
        for (at, to) in gaps {
            let s = (*at).max(start);
            let e = to.unwrap_or(now).min(end);
            if e > s {
                down = down.saturating_add(e - s);
            }
        }
        let up = (width - down).max(0) as f64 / width as f64 * 100.0;
        out.push((start, (up * 10.0).round() / 10.0, has_data));
    }
    out
}

fn nasty_spans(now: i64) -> Vec<Span> {
    vec![
        Span(0, Some(10)),                       // pre-window dust
        Span(now - 100_000, Some(now - 50_000)), // multi-day, overlaps next
        Span(now - 60_000, Some(now - 55_000)),  // nested overlap (double counts)
        Span(now - 1_000, None),                 // open-ended
        Span(now + 1_000, Some(now + 2_000)),    // future (clock skew)
        Span(500, Some(500)),                    // zero-length
        Span(now - 86_400, Some(now - 43_200)),  // exact half day
    ]
}

/// Deterministic pseudo-random gaps (xorshift): differential fuzz across
/// shapes the hand fixtures might miss.
#[allow(clippy::cast_possible_wrap)]
fn fuzz_spans(now: i64, count: usize) -> Vec<Span> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..count)
        .map(|_| {
            let at = now - (next() % 100) as i64 * 9_000 - 1;
            let len = (next() % 200) as i64 * 900;
            let open = next() % 5 == 0;
            Span(at, (!open).then(|| at + len))
        })
        .collect()
}

#[test]
fn sweep_matches_naive_oracle_on_nasty_fixtures() {
    use vigil_core::rollup::{hour_bars, month_bars};
    let now = 90 * 86_400;
    let mut spans = nasty_spans(now);
    spans.extend(fuzz_spans(now, 200));
    let tuples: Vec<(i64, Option<i64>)> = spans.iter().map(|s| (s.0, s.1)).collect();
    for since in [None, Some(0), Some(now - 10_000)] {
        let days = month_bars(&spans, now, 90, since);
        let naive = naive_cells(
            &tuples,
            now,
            90,
            86_400,
            now - now % 86_400 - 89 * 86_400,
            since,
        );
        assert_eq!(days.len(), naive.len());
        for (bar, (day, pct, has_data)) in days.iter().zip(naive.iter()) {
            assert_eq!((bar.day, bar.pct, bar.has_data), (*day, *pct, *has_data));
        }
        let hours = hour_bars(&spans, now, 24, since);
        let naive = naive_cells(&tuples, now, 24, 3_600, now - 23 * 3_600, since);
        assert_eq!(hours.len(), naive.len());
        for (bar, (day, pct, has_data)) in hours.iter().zip(naive.iter()) {
            assert_eq!((bar.day, bar.pct, bar.has_data), (*day, *pct, *has_data));
        }
    }
}
