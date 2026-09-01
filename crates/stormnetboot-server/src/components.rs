//! stormview component feed.
//!
//! The wire contract is stormview's `ComponentSummary`; the structs are
//! restated here rather than pulled in as a git dependency so the build stays
//! hermetic and the boot server keeps no build-time network dependency. The
//! source of truth for the shape is `stormview/src/lib.rs` — if it changes,
//! this changes with it.
//!
//! `kind` is a grouping noun, not an enum: renderers must not exhaust-match on
//! it, so adding kinds here is safe.

use serde::{Deserialize, Serialize};

use crate::{
    pallet::AssetStatus,
    state::{BootState, Phase},
};

/// Component health, in the order a viewer sorts by: broken first.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Error,
    Warn,
    Ok,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tone: Option<String>,
}

impl Metric {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            unit: None,
            tone: None,
        }
    }

    pub fn tone(mut self, tone: &str) -> Self {
        self.tone = Some(tone.to_owned());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    HasOne,
    HasMany,
    BelongsTo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub href: Option<String>,
}

impl Relation {
    pub fn belongs_to(name: &str, target: &str) -> Self {
        Self {
            name: name.to_owned(),
            kind: RelationKind::BelongsTo,
            targets: vec![target.to_owned()],
            href: None,
        }
    }

    pub fn has_many(name: &str, targets: Vec<String>) -> Self {
        Self {
            name: name.to_owned(),
            kind: RelationKind::HasMany,
            targets,
            href: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSummary {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub health: Health,
    pub detail: String,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub relations: Vec<Relation>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub method: String,
    pub path: String,
    pub enabled: bool,
    pub danger: bool,
}

/// Cap on per-host rows in the feed.
///
/// A fleet install can have thousands of machines in flight and the console
/// renders every row it is given, so the feed shows the most recently active
/// and says so rather than shipping ten thousand cards.
const MAX_HOST_ROWS: usize = 200;

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::ScriptFetched => "script fetched",
        Phase::AssetsFetched => "fetching assets",
        Phase::RootAttached => "root attached",
        Phase::Running => "running",
        Phase::Assimilating => "assimilating",
        Phase::Local => "local",
        Phase::Failed => "failed",
    }
}

fn phase_health(phase: Phase) -> Health {
    match phase {
        Phase::Failed => Health::Error,
        Phase::Local => Health::Ok,
        Phase::Running | Phase::Assimilating => Health::Ok,
        _ => Health::Idle,
    }
}

/// Build the full feed: the service's verdict on itself, what it is serving,
/// a row per phase, and the most recently active hosts.
pub fn collect(state: &BootState, assets: &AssetStatus) -> Vec<ComponentSummary> {
    let mut out = Vec::new();
    let counts = state.phase_counts();
    let total: usize = counts.iter().map(|(_, n)| *n).sum();
    let failed = counts
        .iter()
        .find(|(p, _)| *p == Phase::Failed)
        .map(|(_, n)| *n)
        .unwrap_or(0);

    // The console treats `system` as this daemon's verdict on itself.
    let (system_health, system_detail) = if !assets.ready {
        (
            Health::Error,
            format!("not serving: {}", assets.detail),
        )
    } else if failed > 0 {
        (
            Health::Warn,
            format!("serving {}; {failed} host(s) failed", assets.detail),
        )
    } else {
        (Health::Ok, format!("serving {}", assets.detail))
    };

    out.push(ComponentSummary {
        id: "system".into(),
        kind: "netboot".into(),
        label: "stormnetboot".into(),
        health: system_health,
        detail: system_detail,
        metrics: vec![
            Metric::new("hosts", total.to_string()),
            Metric::new("failed", failed.to_string())
                .tone(if failed > 0 { "error" } else { "muted" }),
        ],
        actions: Vec::new(),
        relations: vec![Relation::has_many(
            "phases",
            counts
                .iter()
                .map(|(p, _)| format!("phase:{}", phase_slug(*p)))
                .collect(),
        )],
        link: None,
    });

    out.push(ComponentSummary {
        id: "assets:boot".into(),
        kind: "pallet".into(),
        label: "boot pallet".into(),
        health: if assets.ready { Health::Ok } else { Health::Error },
        detail: assets.detail.clone(),
        metrics: {
            let mut m = vec![Metric::new(
                "version",
                assets.version.clone().unwrap_or_else(|| "-".into()),
            )];
            if let Some(digest) = &assets.digest {
                m.push(Metric::new("digest", short_digest(digest)));
            }
            m.push(
                Metric::new(
                    "signature",
                    if assets.signature_verified {
                        "verified"
                    } else {
                        "unverified"
                    },
                )
                .tone(if assets.signature_verified {
                    "ok"
                } else {
                    "warn"
                }),
            );
            m
        },
        actions: Vec::new(),
        relations: vec![Relation::belongs_to("service", "system")],
        link: None,
    });

    for (phase, count) in &counts {
        out.push(ComponentSummary {
            id: format!("phase:{}", phase_slug(*phase)),
            kind: "phase".into(),
            label: phase_label(*phase).to_owned(),
            health: phase_health(*phase),
            detail: format!("{count} host(s)"),
            metrics: vec![Metric::new("hosts", count.to_string())],
            actions: Vec::new(),
            relations: vec![Relation::belongs_to("service", "system")],
            link: None,
        });
    }

    for host in state.snapshot().into_iter().take(MAX_HOST_ROWS) {
        let mut metrics = vec![Metric::new("phase", phase_label(host.phase))];
        if let Some(version) = &host.pallet_version {
            metrics.push(Metric::new("pallet", version.clone()));
        }
        out.push(ComponentSummary {
            id: format!("host:{}", host.mac),
            kind: "host".into(),
            label: host.mac.to_string(),
            health: phase_health(host.phase),
            detail: host
                .detail
                .clone()
                .unwrap_or_else(|| phase_label(host.phase).to_owned()),
            metrics,
            actions: Vec::new(),
            relations: vec![Relation::belongs_to(
                "phase",
                &format!("phase:{}", phase_slug(host.phase)),
            )],
            link: None,
        });
    }

    out
}

fn phase_slug(phase: Phase) -> &'static str {
    match phase {
        Phase::ScriptFetched => "script-fetched",
        Phase::AssetsFetched => "assets-fetched",
        Phase::RootAttached => "root-attached",
        Phase::Running => "running",
        Phase::Assimilating => "assimilating",
        Phase::Local => "local",
        Phase::Failed => "failed",
    }
}

fn short_digest(digest: &str) -> String {
    let hex = digest.trim_start_matches("sha256:");
    hex.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac::Mac;

    fn assets_ready() -> AssetStatus {
        AssetStatus {
            ready: true,
            detail: "stormcos boot 10.20".into(),
            version: Some("10.20".into()),
            digest: Some("sha256:abcdef0123456789".into()),
            signature_verified: true,
        }
    }

    #[test]
    fn always_emits_a_system_component_first() {
        let feed = collect(&BootState::default(), &assets_ready());
        assert_eq!(feed[0].id, "system");
        assert_eq!(feed[0].health, Health::Ok);
    }

    #[test]
    fn unready_assets_make_the_service_unhealthy() {
        let assets = AssetStatus {
            ready: false,
            detail: "no boot pallet".into(),
            version: None,
            digest: None,
            signature_verified: false,
        };
        let feed = collect(&BootState::default(), &assets);
        assert_eq!(feed[0].health, Health::Error);
        assert!(feed[0].detail.contains("not serving"));
    }

    #[test]
    fn a_failed_host_warns_the_service_without_hiding_it() {
        let state = BootState::default();
        state.observe(Mac::parse("aa:bb:cc:dd:ee:01").unwrap(), Phase::Running, None);
        state.observe(
            Mac::parse("aa:bb:cc:dd:ee:02").unwrap(),
            Phase::Failed,
            Some("attach timed out".into()),
        );

        let feed = collect(&state, &assets_ready());
        assert_eq!(feed[0].health, Health::Warn);
        assert!(feed[0].detail.contains("1 host(s) failed"));

        let failed = feed.iter().find(|c| c.id == "host:aa:bb:cc:dd:ee:02").unwrap();
        assert_eq!(failed.health, Health::Error);
        assert_eq!(failed.detail, "attach timed out");
    }

    #[test]
    fn ids_follow_the_kind_colon_name_convention_and_relations_resolve() {
        let state = BootState::default();
        state.observe(Mac::parse("aa:bb:cc:dd:ee:01").unwrap(), Phase::Running, None);
        let feed = collect(&state, &assets_ready());

        let ids: Vec<&str> = feed.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"phase:running"));
        assert!(ids.contains(&"host:aa:bb:cc:dd:ee:01"));

        // Every relation target must exist in the same feed, or the console
        // renders a dangling edge.
        for component in &feed {
            for relation in &component.relations {
                for target in &relation.targets {
                    assert!(
                        ids.contains(&target.as_str()),
                        "{} -> missing {target}",
                        component.id
                    );
                }
            }
        }
    }

    #[test]
    fn host_rows_are_capped() {
        let state = BootState::default();
        for i in 0..(MAX_HOST_ROWS + 50) {
            let mac = Mac::parse(&format!("aa:bb:cc:{:02x}:{:02x}:01", i / 256, i % 256)).unwrap();
            state.observe(mac, Phase::ScriptFetched, None);
        }
        let feed = collect(&state, &assets_ready());
        let hosts = feed.iter().filter(|c| c.kind == "host").count();
        assert_eq!(hosts, MAX_HOST_ROWS);
    }

    #[test]
    fn serialises_health_lowercase_as_the_contract_requires() {
        let json = serde_json::to_string(&Health::Ok).unwrap();
        assert_eq!(json, "\"ok\"");
        let json = serde_json::to_string(&RelationKind::BelongsTo).unwrap();
        assert_eq!(json, "\"belongs_to\"");
    }
}
