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
        every_min: None,
        uptime_every_min: None,
        sample_every_min: None,
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
                every_min: None,
                uptime_every_min: None,
                sample_every_min: None,
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
                every_min: None,
                uptime_every_min: None,
                sample_every_min: None,
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

#[test]
fn cadence_prefers_monitor_override_then_global() {
    use vigil_core::config::{Site, MIN_CADENCE_MIN, DEFAULT_SAMPLE_MIN};
    let site = Site {
        check_every_min: 30,
        sample_every_min: 120,
        ..Site::default()
    };
    let mut inherited = web();
    inherited.every_min = None;
    assert_eq!(site.cadence_min(&inherited), 30);
    // Legacy `every_min` still overrides the global uptime cadence.
    let mut overridden = web();
    overridden.every_min = Some(60);
    assert_eq!(site.cadence_min(&overridden), 60);
    // `uptime_every_min` wins over legacy `every_min` when both are set.
    let mut both = web();
    both.every_min = Some(60);
    both.uptime_every_min = Some(30);
    assert_eq!(site.cadence_min(&both), 30);
    // Sub-5-minute cadences clamp to the floor to protect D1 rows.
    let mut fast = web();
    fast.uptime_every_min = Some(1);
    assert_eq!(site.cadence_min(&fast), MIN_CADENCE_MIN);
    // Unset global uptime cadence means every tick (floor still applies);
    // unset sample cadence means hourly.
    let bare = Site::default();
    assert_eq!(bare.cadence_min(&inherited), MIN_CADENCE_MIN);
    assert_eq!(bare.sample_cadence_min(&inherited), DEFAULT_SAMPLE_MIN);
    // Sample cadence follows its own override, but never runs faster than
    // the uptime cadence (samples ride on probes).
    assert_eq!(site.sample_cadence_min(&inherited), 120);
    let mut frequent = web();
    frequent.sample_every_min = Some(5);
    assert_eq!(
        site.sample_cadence_min(&frequent),
        site.cadence_min(&frequent)
    );
}

#[test]
fn refresh_interval_defaults_and_disables() {
    use vigil_core::config::{Site, DEFAULT_REFRESH_SECS};
    assert_eq!(
        Site::default().refresh_ms(),
        DEFAULT_REFRESH_SECS.saturating_mul(1_000)
    );
    let off = Site {
        refresh_secs: Some(0),
        ..Site::default()
    };
    assert_eq!(off.refresh_ms(), 0);
    let custom = Site {
        refresh_secs: Some(45),
        ..Site::default()
    };
    assert_eq!(custom.refresh_ms(), 45_000);
    // A one-second poll would issue 86,400 health reads a day per tab.
    for secs in [1, 5, 29, 30] {
        let floored = Site {
            refresh_secs: Some(secs),
            ..Site::default()
        };
        assert_eq!(
            floored.refresh_ms(),
            vigil_core::config::MIN_REFRESH_SECS.saturating_mul(1_000),
            "{secs}s must clamp to the floor"
        );
    }
    let huge = Site {
        refresh_secs: Some(u64::MAX),
        ..Site::default()
    };
    assert_eq!(huge.refresh_ms(), u64::MAX);
}

#[test]
fn sub_floor_uptime_cadence_is_rejected() {
    use vigil_core::config::Site;
    for every in [1, 2, 4] {
        let mut bad = web();
        bad.id = "z".into();
        bad.uptime_every_min = Some(every);
        let site = Site {
            monitors: vec![bad],
            ..Site::default()
        };
        let errors = site.check().unwrap_err();
        assert!(
            errors.iter().any(|e| e.to_string().contains("bad every_min")),
            "uptime_every_min={every} must be rejected"
        );
    }
    // 5-minute floor and slower uptime cadences pass (legacy field too).
    for every in [0, 5, 30, 60] {
        let mut ok = web();
        ok.every_min = if every == 0 { None } else { Some(every) };
        let site = Site {
            monitors: vec![ok],
            ..Site::default()
        };
        assert!(site.check().is_ok(), "uptime every_min={every} must pass");
    }
    // Global sample cadence below the floor is a typo; hourly+ passes.
    let bad_sample = Site {
        sample_every_min: 1,
        ..Site::default()
    };
    assert!(
        bad_sample
            .check()
            .unwrap_err()
            .iter()
            .any(|e| e.to_string().contains("bad sample_every_min"))
    );
    let ok_sample = Site {
        sample_every_min: 60,
        ..Site::default()
    };
    // No monitors -> "no monitors" error, but never a sample error.
    assert!(
        ok_sample
            .check()
            .unwrap_err()
            .into_iter()
            .all(|e| !e.to_string().contains("sample"))
    );
}

#[test]
fn auth_config_rejects_missing_or_ambiguous_credentials() {
    use vigil_core::config::{Gate, Site};
    let with_auth = |auth: Gate| Site {
        auth: Some(auth),
        monitors: vec![web()],
        ..Site::default()
    };
    let errors = |site: Site| {
        site.check()
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    // Exactly one non-empty credential source is required.
    let none = errors(with_auth(Gate {
        user: "admin".into(),
        pass: None,
        pass_env: None,
    }));
    assert!(
        none.iter().any(|e| e.contains("auth needs a password")),
        "{none:?}"
    );

    let both = errors(with_auth(Gate {
        user: "admin".into(),
        pass: Some("secret".into()),
        pass_env: Some("STATUS_PASSWORD".into()),
    }));
    assert!(
        both.iter().any(|e| e.contains("both pass and pass_env")),
        "{both:?}"
    );

    let empty_pass = errors(with_auth(Gate {
        user: "admin".into(),
        pass: Some(String::new()),
        pass_env: None,
    }));
    assert!(
        empty_pass.iter().any(|e| e.contains("empty auth password")),
        "{empty_pass:?}"
    );

    let empty_env = errors(with_auth(Gate {
        user: "admin".into(),
        pass: None,
        pass_env: Some(String::new()),
    }));
    assert!(
        empty_env.iter().any(|e| e.contains("empty auth password")),
        "{empty_env:?}"
    );

    let empty_user = errors(with_auth(Gate {
        user: "  ".into(),
        pass: Some("secret".into()),
        pass_env: None,
    }));
    assert!(
        empty_user.iter().any(|e| e.contains("empty auth user")),
        "{empty_user:?}"
    );

    let colon_user = errors(with_auth(Gate {
        user: "ad:min".into(),
        pass: Some("secret".into()),
        pass_env: None,
    }));
    assert!(
        colon_user.iter().any(|e| e.contains("colon")),
        "{colon_user:?}"
    );

    // Valid forms: cleartext pass, or a non-empty env var name.
    assert!(with_auth(Gate {
        user: "admin".into(),
        pass: Some("secret".into()),
        pass_env: None,
    })
    .check()
    .is_ok());
    assert!(with_auth(Gate {
        user: "admin".into(),
        pass: None,
        pass_env: Some("STATUS_PASSWORD".into()),
    })
    .check()
    .is_ok());
}

#[test]
fn links_reject_dangerous_schemes() {
    use vigil_core::config::{Hook, Site};
    let site = |href: Option<&str>, link: &str, hook: &str| Site {
        monitors: vec![Monitor {
            href: href.map(str::to_owned),
            ..web()
        }],
        links: [("Home".to_owned(), link.to_owned())].into_iter().collect(),
        notify: vigil_core::config::Notify {
            hooks: vec![Hook {
                url: hook.to_owned(),
                method: "POST".into(),
                mode: "json".into(),
                text: "x".into(),
            }],
            ..Default::default()
        },
        ..Site::default()
    };
    let bad = |value: &str| {
        site(Some(value), value, value)
            .check()
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    for value in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "vbscript:msgbox(1)",
        "blob:https://example.com/x",
        "file:///etc/passwd",
        "https://example.com/\nnext",
        "//evil.example",
    ] {
        let errors = bad(value);
        assert!(errors.iter().any(|e| e.contains("bad href")), "{value:?}: {errors:?}");
        assert!(errors.iter().any(|e| e.contains("bad link")), "{value:?}: {errors:?}");
        assert!(errors.iter().any(|e| e.contains("bad hook")), "{value:?}: {errors:?}");
    }

    // Normal web URLs and same-origin paths remain valid.
    for value in ["https://example.com/x", "http://example.com", "/status"] {
        assert!(site(Some(value), value, value).check().is_ok(), "{value:?}");
    }
    // Footer links may use mailto; monitor links and hooks may not.
    assert!(site(None, "mailto:ops@example.com", "https://hooks.example/x")
        .check()
        .is_ok());
    let errors = site(Some("mailto:ops@example.com"), "https://example.com", "https://hooks.example/x")
        .check()
        .unwrap_err()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(errors.iter().any(|e| e.contains("bad href")), "{errors:?}");
}
