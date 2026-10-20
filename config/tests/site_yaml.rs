//! `site.yaml` parsing: fixture mechanics plus parse-helper validation.
//!
//! The live `config/site.yaml` is operator-owned, git-ignored config, so
//! tests pin only inline fixtures plus the `vigil::parse` helpers — never
//! the live file's title or monitor list.

use vigil_core::config::{Probe, Site, DEFAULT_REFRESH_SECS, DEFAULT_SAMPLE_MIN, MIN_CADENCE_MIN};

/// Full example exercising both probe shapes, timeouts, grace, and upkeep.
const EXAMPLE_YAML: &str = r"
title: Example Status

links:
  Home: https://example.com

monitors:
  - id: site
    name: Example Site
    expect: [200]
    timeout_ms: 10000
    tip: Homepage
    href: https://example.com
    check:
      via: http
      url: https://example.com
      method: GET
      headers:
        User-Agent: vigil/0.1

  - id: dns-tcp
    name: DNS (TCP 53)
    timeout_ms: 5000
    quiet_chart: true
    check:
      via: tcp
      host: 1.1.1.1
      port: 53

notify:
  grace_min: 2

upkeep:
  - title: Example upkeep
    body: Replace with real maintenance windows.
    from: 2030-01-01T00:00:00Z
";

#[test]
fn example_yaml_matches_documented_site() {
    let site: Site = serde_yaml::from_str(EXAMPLE_YAML).expect("fixture parses");
    assert_eq!(site.title, "Example Status");
    assert_eq!(site.monitors.len(), 2);
    assert_eq!(site.monitors[0].id, "site");
    assert_eq!(site.monitors[0].timeout_ms, 10_000);
    assert_eq!(
        site.monitors[0].check,
        Probe::Http {
            url: "https://example.com".into(),
            method: "GET".into(),
            headers: [("User-Agent".to_owned(), "vigil/0.1".to_owned())]
                .into_iter()
                .collect(),
            body: None,
        }
    );
    assert_eq!(
        site.monitors[1].check,
        Probe::Tcp {
            host: "1.1.1.1".into(),
            port: 53,
        }
    );
    assert!(site.monitors[1].quiet_chart);
    assert_eq!(site.notify.grace_min, 2);
    assert!(site.notify.hooks.is_empty());
    assert_eq!(site.upkeep.len(), 1);
    assert!(site.check().is_ok());
}

#[test]
fn gated_matches_site_auth_without_cloning() {
    // The worker derives its gate from the DB-loaded config; the `auth`
    // flag must survive a parse round trip.
    let site: Site = serde_yaml::from_str(EXAMPLE_YAML).expect("fixture parses");
    assert!(site.auth.is_none());
    let gated: Site = serde_yaml::from_str(
        "auth:\n  user: admin\n  pass_env: STATUS_PASSWORD\nmonitors:\n  - id: m\n    name: M\n    check:\n      via: tcp\n      host: example.com\n      port: 80\n",
    )
    .expect("gated parses");
    assert!(gated.auth.is_some());
}

#[test]
fn parse_helpers_accept_and_reject_yaml() {
    assert!(vigil::parse(EXAMPLE_YAML).is_ok());
    assert!(vigil::parse_checked(EXAMPLE_YAML).is_ok());
    assert!(vigil::parse("title: [unclosed").is_err());
    // Parses but has no monitors: `parse` ok, `parse_checked` rejects.
    let empty = "title: Empty\nmonitors: []\n";
    assert!(vigil::parse(empty).is_ok());
    assert!(vigil::parse_checked(empty).is_err());
}

#[test]
fn links_keep_file_order() {
    let site: Site = serde_yaml::from_str(
        r"
links:
  Home: https://www.troysk.com
  Blog: https://blog.troysk.com
",
    )
    .expect("links parse");
    let keys: Vec<&str> = site.links.keys().map(String::as_str).collect();
    assert_eq!(keys, ["Home", "Blog"]);
}

#[test]
fn omitted_fields_take_documented_defaults() {
    let site: Site = serde_yaml::from_str(
        r"
monitors:
  - id: m
    name: M
    check:
      via: tcp
      host: example.com
      port: 80
",
    )
    .expect("minimal monitor parses");
    assert_eq!(site.title, "Status");
    assert_eq!(site.monitors[0].timeout_ms, 10_000);
    assert_eq!(site.monitors[0].expect, Vec::<u16>::new());
    assert!(site.notify.hooks.is_empty());
    // Split cadence and refresh controls are optional; old configs inherit
    // the documented defaults instead of failing to parse.
    assert_eq!(site.check_every_min, 0);
    assert_eq!(site.sample_every_min, 0);
    assert_eq!(site.refresh_secs, None);
    assert_eq!(site.monitors[0].every_min, None);
    assert_eq!(site.monitors[0].uptime_every_min, None);
    assert_eq!(site.monitors[0].sample_every_min, None);
    assert_eq!(
        site.cadence_min(&site.monitors[0]),
        MIN_CADENCE_MIN,
        "unset uptime cadence probes every tick, clamped to the floor"
    );
    assert_eq!(
        site.sample_cadence_min(&site.monitors[0]),
        DEFAULT_SAMPLE_MIN,
        "unset sample cadence samples hourly"
    );
    assert_eq!(
        site.refresh_ms(),
        DEFAULT_REFRESH_SECS.saturating_mul(1_000)
    );
}

#[test]
fn cadence_fields_parse_and_clamp_to_uptime() {
    let site: Site = serde_yaml::from_str(
        r"
check_every_min: 15
sample_every_min: 120
refresh_secs: 45
monitors:
  - id: m
    name: M
    every_min: 30
    uptime_every_min: 10
    sample_every_min: 5
    check:
      via: tcp
      host: example.com
      port: 80
",
    )
    .expect("cadence config parses");
    assert_eq!(site.check_every_min, 15);
    assert_eq!(site.sample_every_min, 120);
    assert_eq!(site.refresh_secs, Some(45));
    assert_eq!(site.refresh_ms(), 45_000);

    let monitor = &site.monitors[0];
    assert_eq!(monitor.every_min, Some(30));
    assert_eq!(monitor.uptime_every_min, Some(10));
    assert_eq!(monitor.sample_every_min, Some(5));
    // The new field wins over the legacy alias, and sample cadence cannot
    // run faster than the probe cadence it rides on.
    assert_eq!(site.cadence_min(monitor), 10);
    assert_eq!(site.sample_cadence_min(monitor), 10);
    assert!(site.check().is_ok());
}
