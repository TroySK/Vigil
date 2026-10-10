//! `site.yaml` parsing: embedded example plus defaults and tagged probes.

use vigil_core::config::{Probe, Site};

#[test]
fn example_yaml_matches_documented_site() {
    let site: Site = serde_yaml::from_str(vigil::SITE_YAML).expect("site.yaml parses");
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
