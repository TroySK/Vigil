//! Example Vigil site config, compiled into the Worker.
//!
//! Edit [`site()`] to define monitors; keep ids stable — D1 history keys off them.

use std::collections::BTreeMap;
use vigil_core::config::{Gate, Hook, Monitor, Notify, Probe, Site, Upkeep};

/// Edit this file: it is compiled into the Worker.
/// Keep ids stable — history keys off them.
#[must_use]
pub fn site() -> Site {
    let mut headers = BTreeMap::new();
    headers.insert("User-Agent".into(), "vigil/0.1".into());
    Site {
        title: "Example Status".into(),
        links: BTreeMap::from([("Home".into(), "https://example.com".into())]),
        groups: BTreeMap::new(),
        monitors: vec![
            Monitor {
                id: "site".into(),
                name: "Example Site".into(),
                check: Probe::Http {
                    url: "https://example.com".into(),
                    method: "GET".into(),
                    headers,
                    body: None,
                },
                expect: vec![200],
                timeout_ms: 10_000,
                has: None,
                lacks: None,
                tip: Some("Homepage".into()),
                href: Some("https://example.com".into()),
                quiet_chart: false,
            },
            Monitor {
                id: "dns-tcp".into(),
                name: "DNS (TCP 53)".into(),
                check: Probe::Tcp {
                    host: "1.1.1.1".into(),
                    port: 53,
                },
                expect: vec![],
                timeout_ms: 5_000,
                has: None,
                lacks: None,
                tip: None,
                href: None,
                quiet_chart: true,
            },
        ],
        notify: Notify {
            hooks: vec![],
            grace_min: 2,
            mute: vec![],
        },
        upkeep: vec![Upkeep {
            title: "Example upkeep".into(),
            body: "Replace with real maintenance windows.".into(),
            from: "2030-01-01T00:00:00Z".into(),
            until: None,
            only: vec![],
        }],
        auth: None,
    }
}

#[allow(dead_code)]
fn example_hook() -> Hook {
    Hook {
        url: "https://hooks.example/notify".into(),
        method: "POST".into(),
        mode: "json".into(),
        text: "[$STATUS] $NAME: $REASON ($TIME)".into(),
    }
}

#[allow(dead_code)]
fn example_gate() -> Gate {
    Gate {
        user: "admin".into(),
        pass: None,
        pass_env: Some("STATUS_PASSWORD".into()),
    }
}
