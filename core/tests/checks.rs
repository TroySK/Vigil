use std::collections::BTreeMap;
use vigil_core::checks::{judge, Seen, What};
use vigil_core::config::{Monitor, Probe};

fn web() -> Monitor {
    Monitor {
        id: "w".into(),
        name: "W".into(),
        check: Probe::Http {
            url: "https://x.example".into(),
            method: "GET".into(),
            headers: BTreeMap::new(),
            body: None,
        },
        expect: vec![],
        timeout_ms: 5_000,
        has: None,
        lacks: None,
        tip: None,
        href: None,
        quiet_chart: false,
    }
}

#[test]
fn ok_2xx() {
    let m = web();
    let s = Seen {
        ms: 9,
        what: What::Http {
            code: 200,
            body: "hi".into(),
        },
    };
    assert!(judge(&m, &s).up);
}

#[test]
fn bad_code() {
    let m = web();
    let s = Seen {
        ms: 9,
        what: What::Http {
            code: 500,
            body: "hi".into(),
        },
    };
    let r = judge(&m, &s);
    assert!(!r.up);
}

#[test]
fn keywords() {
    let mut m = web();
    m.has = Some("ready".into());
    let s = Seen {
        ms: 1,
        what: What::Http {
            code: 200,
            body: "warming".into(),
        },
    };
    assert!(!judge(&m, &s).up);
    m.has = None;
    m.lacks = Some("oops".into());
    let s2 = Seen {
        ms: 1,
        what: What::Http {
            code: 200,
            body: "oops here".into(),
        },
    };
    assert!(!judge(&m, &s2).up);
}

#[test]
fn tcp() {
    let m = Monitor {
        check: Probe::Tcp {
            host: "h".into(),
            port: 80,
        },
        ..web()
    };
    let ok = Seen {
        ms: 2,
        what: What::Tcp { open: true },
    };
    assert!(judge(&m, &ok).up);
    let shut = Seen {
        ms: 2,
        what: What::Tcp { open: false },
    };
    assert!(!judge(&m, &shut).up);
}

#[test]
fn upkeep_windows() {
    let u = vigil_core::config::Upkeep {
        title: "Win".into(),
        body: "Body".into(),
        from: "2030-01-01T00:00:00Z".into(),
        until: None,
        only: vec![],
    };
    assert_eq!(
        vigil_core::upkeep::parse_moment("1970-01-01T00:00:00Z").unwrap(),
        0
    );
    assert_eq!(
        vigil_core::upkeep::parse_moment("2025-04-27T00:00:00+08:00").unwrap(),
        1_745_683_200
    );
    assert_eq!(
        vigil_core::upkeep::upkeep_banner(&u, 1_700_000_000)
            .unwrap()
            .0,
        "upcoming"
    );
}

#[test]
fn unicode_reason_never_panics() {
    // Regression: `clip` used byte-slicing (`&s[..160]`) which panicked on
    // multi-byte reasons. Feed a long emoji reason through `judge`.
    let m = web();
    let long_reason = "🔥".repeat(200);
    let seen = Seen {
        ms: 1,
        what: What::Dead { why: long_reason },
    };
    let verdict = judge(&m, &seen);
    assert!(!verdict.up);
    assert!(verdict.why.chars().count() <= 162); // 160 + ".."
}

#[test]
fn empty_keywords_are_ignored() {
    // An empty `has`/`lacks` must not flip the verdict (avoids `contains("")`
    // always-true footguns if a config author leaves them blank).
    let mut m = web();
    m.has = Some(String::new());
    m.lacks = Some(String::new());
    let seen = Seen {
        ms: 1,
        what: What::Http {
            code: 200,
            body: "anything".into(),
        },
    };
    assert!(judge(&m, &seen).up);
}

#[test]
fn colo_place_normalizes() {
    use vigil_core::checks::place;
    assert_eq!(place(None), "CRON");
    assert_eq!(place(Some("")), "CRON");
    assert_eq!(place(Some("  ")), "CRON");
    assert_eq!(place(Some(" lax ")), "LAX");
}

#[test]
fn svg_escapes_label() {
    // Badge labels come from monitor names (operator input); they must not
    // break out of the SVG text element.
    let svg = vigil_core::views::svg("<b>&\"'", "up", "#2da44e");
    assert!(svg.contains("&lt;b&gt;&amp;&quot;&#39;"));
    assert!(!svg.contains("<b>"));
}

#[test]
fn moment_errors_are_typed() {
    use vigil_core::upkeep::{parse_moment, MomentError};
    assert_eq!(
        parse_moment("-5").unwrap_err(),
        MomentError::NegativeTimestamp
    );
    assert!(matches!(
        parse_moment("not-a-time").unwrap_err(),
        MomentError::InvalidFormat(_)
    ));
}

#[test]
fn config_check_reports_typed_errors() {
    use vigil_core::config::{Monitor, Probe, Site};
    let site = Site {
        monitors: vec![
            Monitor {
                id: "dup".into(),
                name: "A".into(),
                check: Probe::Tcp {
                    host: "h".into(),
                    port: 80,
                },
                expect: vec![],
                timeout_ms: 0, // invalid
                has: None,
                lacks: None,
                tip: None,
                href: None,
                quiet_chart: false,
            },
            Monitor {
                id: "dup".into(), // duplicate
                name: "B".into(),
                check: Probe::Http {
                    url: "ftp://bad".into(), // invalid scheme
                    method: "GET".into(),
                    headers: BTreeMap::new(),
                    body: None,
                },
                expect: vec![],
                timeout_ms: 5_000,
                has: None,
                lacks: None,
                tip: None,
                href: None,
                quiet_chart: false,
            },
        ],
        ..Site::default()
    };
    let errors = site.check().unwrap_err();
    assert_eq!(errors.len(), 3);
    assert!(errors.iter().any(|e| e.to_string().contains("bad timeout")));
    assert!(errors.iter().any(|e| e.to_string().contains("dup id")));
    assert!(errors.iter().any(|e| e.to_string().contains("bad url")));
}
