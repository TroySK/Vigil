use vigil_core::state::Track;

#[test]
fn open_close() {
    let mut t = Track::default();
    assert!(t.shut());
    assert!(t.note_down(100, "boom"));
    assert!(!t.shut());
    assert!(!t.note_down(160, "boom"));
    assert!(t.note_down(170, "other"));
    assert!(t.note_up(200));
    assert!(t.shut());
    assert_eq!(t.gaps.len(), 1);
    assert_eq!(t.gaps[0].to, Some(200));
}

#[test]
fn pct_math() {
    let mut t = Track::default();
    t.note_down(0, "x");
    t.note_up(600);
    let p = t.uptime_pct(3600, 3600);
    assert!(p > 80.0 && p < 84.0);
}
