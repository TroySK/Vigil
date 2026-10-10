//! `site.yaml` parsing: fixture mechanics plus live-file validation.
//!
//! The live `config/site.yaml` is user-owned config, so tests pin only an
//! inline fixture — never the live file's title or monitor list. The live
//! file gets a parse + `check()` gate so CI still catches authoring errors.

use vigil_core::config::{Probe, Site};

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
fn live_site_yaml_parses_and_validates() {
    let site: Site = serde_yaml::from_str(vigil::SITE_YAML).expect("site.yaml parses");
    assert!(site.check().is_ok(), "site.yaml must pass validation");
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
}
