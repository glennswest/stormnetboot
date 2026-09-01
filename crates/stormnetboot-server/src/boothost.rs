//! The `BootHost` custom resource.
//!
//! Shaped after Metal3's `BareMetalHost` so an OpenShift operator recognises
//! it, but scoped to the boot chain: BMC control belongs to bmh-operator-rs,
//! and this resource carries only what stormnetboot needs to answer a PXE
//! request and to say afterwards what happened.
//!
//! The types here are the definition, not a copy of it: `--print-crd` emits
//! the CRD from these structs, so the shipped manifest and the code that
//! serves it cannot drift apart. Status follows the Kubernetes conventions —
//! `Available`/`Progressing`/`Degraded`, each with a reason, a message, a
//! transition time that only moves when the status does, and the generation
//! it was computed from.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    hosts::{HostRecord, ObjectRef},
    mac::Mac,
    state::{HostState, Inventory, Phase},
};

fn online_by_default() -> bool {
    true
}

/// What a machine should boot, and as whom.
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "netboot.storm.io",
    version = "v1alpha1",
    kind = "BootHost",
    plural = "boothosts",
    shortname = "bh",
    namespaced,
    status = "BootHostStatus",
    printcolumn = r#"{"name":"MAC","type":"string","jsonPath":".spec.bootMACAddress"}"#,
    printcolumn = r#"{"name":"Hostname","type":"string","jsonPath":".spec.hostname"}"#,
    printcolumn = r#"{"name":"Role","type":"string","jsonPath":".spec.role"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Pallet","type":"string","jsonPath":".status.palletVersion"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BootHostSpec {
    /// MAC that PXE boots. The host's identity.
    #[serde(rename = "bootMACAddress")]
    pub boot_mac_address: String,
    /// Name this machine takes, for the rest of its life. Defaults to the
    /// object's own name, which is the spelling an operator already typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Day-2 profile to apply when it joins a cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Loadout to boot, e.g. `stormcos:10.20`. Defaults to the server's
    /// golden when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// NVMe/TCP portal override, to steer this host at a nearer appliance
    /// replica than the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<String>,
    /// Extra kernel command line for this host alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_cmdline: Option<String>,
    /// Whether this host is allowed to boot.
    #[serde(default = "online_by_default")]
    pub online: bool,
}

/// Hardware as the running node described itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HardwareStatus {
    pub cpus: u32,
    /// Spelled as the CRD spells it: `KB` reads as a unit, `Kb` reads as a typo.
    #[serde(rename = "memoryKB")]
    pub memory_kb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<String>,
}

impl From<&Inventory> for HardwareStatus {
    fn from(inv: &Inventory) -> Self {
        Self {
            cpus: inv.cpus,
            memory_kb: inv.memory_kb,
            product: inv.product.clone(),
            serial: inv.serial.clone(),
            disks: inv.disks.clone(),
        }
    }
}

/// A standard Kubernetes condition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    /// `"True"`, `"False"` or `"Unknown"`, as strings, as the convention has it.
    pub status: String,
    pub reason: String,
    pub message: String,
    /// Moves only when `status` moves. An operator reading "3 hours" here has
    /// to be reading how long the host has been stuck, not how long ago the
    /// controller last woke up.
    pub last_transition_time: String,
    pub observed_generation: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootHostStatus {
    /// Where the host is, in the same spelling the feed and the metrics use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pallet_version: Option<String>,
    /// sbregistry clone claimed for this host's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// Inventory reported by the running node. There is no inspection boot;
    /// the machine describes itself once it is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<HardwareStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

impl BootHost {
    /// The host record this object describes.
    ///
    /// `None` when the MAC will not parse: a `BootHost` nobody can match a
    /// booting machine against is a configuration error, and serving defaults
    /// to an unidentified machine because a record was malformed is worse
    /// than not serving it.
    pub fn to_record(&self) -> Option<HostRecord> {
        let mac = match Mac::parse(&self.spec.boot_mac_address) {
            Ok(mac) => mac,
            Err(err) => {
                tracing::warn!(
                    object = %self.object_ref_or_unnamed(),
                    mac = %self.spec.boot_mac_address,
                    %err,
                    "BootHost has an unusable bootMACAddress; ignoring it"
                );
                return None;
            }
        };

        let name = self
            .spec
            .hostname
            .clone()
            .or_else(|| self.metadata.name.clone())?;

        Some(HostRecord {
            mac,
            name,
            role: self.spec.role.clone(),
            stack: self.spec.stack.clone(),
            portal: self.spec.portal.clone(),
            extra_cmdline: self.spec.extra_cmdline.clone(),
            online: self.spec.online,
            object: self.object_ref(),
        })
    }

    pub fn object_ref(&self) -> Option<ObjectRef> {
        Some(ObjectRef {
            namespace: self.metadata.namespace.clone()?,
            name: self.metadata.name.clone()?,
            generation: self.metadata.generation.unwrap_or_default(),
        })
    }

    fn object_ref_or_unnamed(&self) -> String {
        self.object_ref()
            .map(|o| o.to_string())
            .unwrap_or_else(|| "<unnamed>".to_owned())
    }
}

/// One condition before its transition time is resolved against the previous
/// status.
struct Proposed {
    type_: &'static str,
    status: bool,
    reason: &'static str,
    message: String,
}

/// The status this host should have, given what we have observed.
///
/// Pure and level-triggered: the same observation produces the same status,
/// so the writer can compare and skip, and a restart re-derives rather than
/// remembering.
pub fn desired_status(
    observed: Option<&HostState>,
    online: bool,
    claim_id: Option<String>,
    generation: i64,
    previous: Option<&BootHostStatus>,
    now: &str,
) -> BootHostStatus {
    let proposed = match observed.map(|s| s.phase) {
        Some(Phase::Failed) => {
            let detail = observed
                .and_then(|s| s.detail.clone())
                .unwrap_or_else(|| "boot failed".to_owned());
            vec![
                Proposed {
                    type_: "Available",
                    status: false,
                    reason: "BootFailed",
                    message: detail.clone(),
                },
                Proposed {
                    type_: "Progressing",
                    status: false,
                    reason: "BootFailed",
                    message: detail.clone(),
                },
                Proposed {
                    type_: "Degraded",
                    status: true,
                    reason: "BootFailed",
                    message: detail,
                },
            ]
        }
        Some(Phase::Local) => vec![
            Proposed {
                type_: "Available",
                status: true,
                reason: "BootedLocally",
                message: "assimilated; booting from local disk".to_owned(),
            },
            Proposed {
                type_: "Progressing",
                status: false,
                reason: "BootedLocally",
                message: "nothing left to do".to_owned(),
            },
            Proposed {
                type_: "Degraded",
                status: false,
                reason: "BootedLocally",
                message: String::new(),
            },
        ],
        Some(phase) => {
            let running = matches!(phase, Phase::Running | Phase::Assimilating);
            let detail = observed
                .and_then(|s| s.detail.clone())
                .unwrap_or_else(|| phase.slug().to_owned());
            vec![
                Proposed {
                    type_: "Available",
                    status: running,
                    reason: if running { "Running" } else { "Booting" },
                    message: detail.clone(),
                },
                Proposed {
                    type_: "Progressing",
                    status: true,
                    reason: "Booting",
                    message: detail,
                },
                Proposed {
                    type_: "Degraded",
                    status: false,
                    reason: "Booting",
                    message: String::new(),
                },
            ]
        }
        None => {
            let (reason, message) = if online {
                (
                    "NotSeen",
                    "the boot server has not been asked to boot this machine".to_owned(),
                )
            } else {
                (
                    "Offline",
                    "spec.online is false; the boot server will refuse this machine".to_owned(),
                )
            };
            vec![
                Proposed {
                    type_: "Available",
                    status: false,
                    reason,
                    message: message.clone(),
                },
                Proposed {
                    type_: "Progressing",
                    status: false,
                    reason,
                    message,
                },
                Proposed {
                    type_: "Degraded",
                    status: false,
                    reason,
                    message: String::new(),
                },
            ]
        }
    };

    let conditions = proposed
        .into_iter()
        .map(|p| {
            let status = if p.status { "True" } else { "False" };
            // The transition time belongs to the status, not to this pass:
            // carry the old one forward whenever the status has not moved.
            let last_transition_time = previous
                .and_then(|prev| prev.conditions.iter().find(|c| c.type_ == p.type_))
                .filter(|c| c.status == status)
                .map(|c| c.last_transition_time.clone())
                .unwrap_or_else(|| now.to_owned());

            Condition {
                type_: p.type_.to_owned(),
                status: status.to_owned(),
                reason: p.reason.to_owned(),
                message: p.message,
                last_transition_time,
                observed_generation: generation,
            }
        })
        .collect();

    BootHostStatus {
        phase: observed.map(|s| s.phase.slug().to_owned()),
        pallet_version: observed.and_then(|s| s.pallet_version.clone()),
        claim_id,
        hardware: observed
            .and_then(|s| s.hardware.as_ref())
            .map(HardwareStatus::from),
        conditions,
    }
}

/// The CRD, as JSON. `kubectl apply -f -` takes JSON as happily as YAML, and
/// emitting it from the types is what keeps `deploy/manifests` honest.
pub fn crd_json() -> anyhow::Result<String> {
    use kube::CustomResourceExt as _;
    Ok(serde_json::to_string_pretty(&BootHost::crd())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boothost(mac: &str) -> BootHost {
        let mut bh = BootHost::new(
            "node7",
            BootHostSpec {
                boot_mac_address: mac.to_owned(),
                hostname: None,
                role: Some("worker".into()),
                stack: None,
                portal: None,
                extra_cmdline: None,
                online: true,
            },
        );
        bh.metadata.namespace = Some("storm-system".into());
        bh.metadata.generation = Some(3);
        bh
    }

    fn host_state(phase: Phase, detail: Option<&str>) -> HostState {
        HostState {
            mac: Mac::parse("aa:bb:cc:dd:ee:ff").unwrap(),
            phase,
            detail: detail.map(str::to_owned),
            pallet_version: Some("10.20".into()),
            hardware: None,
            first_seen_unix: 1,
            updated_unix: 2,
        }
    }

    fn condition<'a>(status: &'a BootHostStatus, type_: &str) -> &'a Condition {
        status.conditions.iter().find(|c| c.type_ == type_).unwrap()
    }

    #[test]
    fn a_boothost_becomes_a_host_record_with_its_object_attached() {
        let record = boothost("AA-BB-CC-DD-EE-FF").to_record().unwrap();
        assert_eq!(record.mac.to_string(), "aa:bb:cc:dd:ee:ff");
        // No explicit hostname: the object's own name is what an operator typed.
        assert_eq!(record.name, "node7");
        assert_eq!(record.role.as_deref(), Some("worker"));
        assert!(record.online);
        let object = record.object.unwrap();
        assert_eq!(object.to_string(), "storm-system/node7");
        assert_eq!(object.generation, 3);
    }

    #[test]
    fn an_unparseable_mac_is_dropped_rather_than_matched_loosely() {
        assert!(boothost("not-a-mac").to_record().is_none());
    }

    #[test]
    fn an_explicit_hostname_wins_over_the_object_name() {
        let mut bh = boothost("aa:bb:cc:dd:ee:ff");
        bh.spec.hostname = Some("db-3".into());
        assert_eq!(bh.to_record().unwrap().name, "db-3");
    }

    #[test]
    fn an_unseen_host_reads_as_not_seen_not_as_broken() {
        let status = desired_status(None, true, None, 3, None, "T0");
        assert!(status.phase.is_none());
        assert_eq!(condition(&status, "Available").status, "False");
        assert_eq!(condition(&status, "Available").reason, "NotSeen");
        assert_eq!(condition(&status, "Degraded").status, "False");
    }

    #[test]
    fn an_offline_host_says_why_it_will_not_boot() {
        let status = desired_status(None, false, None, 3, None, "T0");
        assert_eq!(condition(&status, "Available").reason, "Offline");
        assert!(condition(&status, "Available").message.contains("refuse"));
    }

    #[test]
    fn a_booting_host_is_progressing_but_not_yet_available() {
        let state = host_state(Phase::RootAttached, None);
        let status = desired_status(Some(&state), true, Some("claim-1".into()), 3, None, "T0");

        assert_eq!(status.phase.as_deref(), Some("root-attached"));
        assert_eq!(status.pallet_version.as_deref(), Some("10.20"));
        assert_eq!(status.claim_id.as_deref(), Some("claim-1"));
        assert_eq!(condition(&status, "Progressing").status, "True");
        assert_eq!(condition(&status, "Available").status, "False");
    }

    #[test]
    fn a_running_host_is_available_while_it_is_still_assimilating() {
        for phase in [Phase::Running, Phase::Assimilating] {
            let state = host_state(phase, None);
            let status = desired_status(Some(&state), true, None, 3, None, "T0");
            assert_eq!(condition(&status, "Available").status, "True", "{phase:?}");
            assert_eq!(condition(&status, "Progressing").status, "True", "{phase:?}");
        }
    }

    #[test]
    fn a_local_host_is_available_and_finished() {
        let state = host_state(Phase::Local, None);
        let status = desired_status(Some(&state), true, None, 3, None, "T0");
        assert_eq!(condition(&status, "Available").status, "True");
        assert_eq!(condition(&status, "Progressing").status, "False");
        assert_eq!(condition(&status, "Degraded").status, "False");
    }

    #[test]
    fn a_failure_degrades_and_carries_the_reason_the_node_gave() {
        let state = host_state(Phase::Failed, Some("nvme attach timed out"));
        let status = desired_status(Some(&state), true, None, 3, None, "T0");
        let degraded = condition(&status, "Degraded");
        assert_eq!(degraded.status, "True");
        assert_eq!(degraded.message, "nvme attach timed out");
        assert_eq!(condition(&status, "Available").status, "False");
    }

    #[test]
    fn transition_times_move_only_when_the_status_does() {
        let booting = desired_status(
            Some(&host_state(Phase::RootAttached, None)),
            true,
            None,
            3,
            None,
            "T0",
        );

        // Same status, later pass: the clock must not restart.
        let still_booting = desired_status(
            Some(&host_state(Phase::Running, None)),
            true,
            None,
            3,
            Some(&booting),
            "T1",
        );
        assert_eq!(condition(&still_booting, "Progressing").last_transition_time, "T0");
        // Available did flip, so that one moves.
        assert_eq!(condition(&still_booting, "Available").last_transition_time, "T1");
    }

    #[test]
    fn hardware_reaches_the_status_the_way_the_crd_spells_it() {
        let mut state = host_state(Phase::Running, None);
        state.hardware = Some(Inventory {
            cpus: 64,
            memory_kb: 268_435_456,
            product: Some("PowerEdge R650".into()),
            serial: Some("ABC123".into()),
            disks: vec!["nvme0n1".into()],
        });

        let status = desired_status(Some(&state), true, None, 3, None, "T0");
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["hardware"]["cpus"], 64);
        assert_eq!(json["hardware"]["memoryKB"], 268_435_456u64);
        assert_eq!(json["hardware"]["product"], "PowerEdge R650");
    }

    #[test]
    fn the_generated_crd_matches_the_shipped_manifest() {
        let crd: serde_json::Value = serde_json::from_str(&crd_json().unwrap()).unwrap();
        assert_eq!(crd["metadata"]["name"], "boothosts.netboot.storm.io");
        assert_eq!(crd["spec"]["group"], "netboot.storm.io");
        assert_eq!(crd["spec"]["names"]["kind"], "BootHost");
        assert_eq!(crd["spec"]["scope"], "Namespaced");

        let version = &crd["spec"]["versions"][0];
        assert_eq!(version["name"], "v1alpha1");
        assert!(version["subresources"]["status"].is_object());

        let columns: Vec<&str> = version["additionalPrinterColumns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(columns, ["MAC", "Hostname", "Role", "Phase", "Pallet", "Age"]);

        let required = version["schema"]["openAPIV3Schema"]["properties"]["spec"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|r| r == "bootMACAddress"));
    }

    #[test]
    fn the_wire_spelling_is_the_one_the_crd_declares() {
        let bh = boothost("aa:bb:cc:dd:ee:ff");
        let json = serde_json::to_value(&bh.spec).unwrap();
        assert!(json.get("bootMACAddress").is_some(), "not bootMacAddress");
        assert_eq!(json["online"], true);

        let mut with_extra = bh.spec.clone();
        with_extra.extra_cmdline = Some("console=ttyS0".into());
        let json = serde_json::to_value(&with_extra).unwrap();
        assert_eq!(json["extraCmdline"], "console=ttyS0");
    }
}
