//! Boot state tracking.
//!
//! What a machine is doing right now is the question an operator actually
//! asks during a fleet install, and it is the thing no other component knows:
//! rustkube sees a node only once it registers, and stormblock sees a volume
//! but not the machine waiting on it. The gap between "powered on" and
//! "assimilated" is ours to report.
//!
//! State is deliberately in memory and bounded. It is observational — losing
//! it on restart costs a console panel its history, never a boot: the source
//! of truth for what a host should do is its record and its claim, both of
//! which outlive this process.

use std::{
    collections::HashMap,
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::mac::Mac;

/// Where a host is in the sequence, in the order it happens.
///
/// These are the phases stormnetboot can actually observe: it sees the script
/// fetch and the asset fetches directly, and learns the rest from the node
/// reporting in once it is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Asked for its boot script — firmware is alive and talking to us.
    ScriptFetched,
    /// Pulling kernel and initramfs.
    AssetsFetched,
    /// Reported that it attached its root over NVMe/TCP.
    RootAttached,
    /// Reported a successful switch_root — stormcos is running.
    Running,
    /// Flow-over to local disk is in progress.
    Assimilating,
    /// Mirror broken, booting locally. The network source is no longer needed.
    Local,
    /// Something went wrong; `detail` says what.
    Failed,
}

impl Phase {
    /// Whether this phase means the host no longer needs the boot server.
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Local | Phase::Failed)
    }

    /// The wire spelling, shared by the JSON feed, the metric label and the
    /// `BootHost` status — one host in one phase must not read three ways
    /// depending on where an operator looked.
    pub fn slug(self) -> &'static str {
        match self {
            Phase::ScriptFetched => "script-fetched",
            Phase::AssetsFetched => "assets-fetched",
            Phase::RootAttached => "root-attached",
            Phase::Running => "running",
            Phase::Assimilating => "assimilating",
            Phase::Local => "local",
            Phase::Failed => "failed",
        }
    }
}

/// Hardware as the running node described itself.
///
/// This is what removes the inspection boot: there is no agent ramdisk and no
/// second reboot, because the machine is already running the OS it will keep
/// and reporting its own inventory is a file read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    pub cpus: u32,
    #[serde(default)]
    pub memory_kb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostState {
    pub mac: Mac,
    pub phase: Phase,
    /// Free-text detail for the current phase, e.g. a failure reason or the
    /// percentage of extents migrated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Boot pallet version this host was served, so a console can show which
    /// hosts are on which version mid-rollout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pallet_version: Option<String>,
    /// What the node said it is made of, once it was up to say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<Inventory>,
    pub first_seen_unix: u64,
    pub updated_unix: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Upper bound on tracked hosts.
///
/// A fleet install is exactly when memory must not grow without limit, so the
/// map is capped and evicts hosts that have finished — a host that reached
/// `Local` has nothing left to watch.
const MAX_TRACKED: usize = 20_000;

#[derive(Debug, Default)]
pub struct BootState {
    hosts: RwLock<HashMap<Mac, HostState>>,
}

impl BootState {
    /// Record a phase transition, creating the entry if this is the first we
    /// have heard of the host.
    pub fn observe(&self, mac: Mac, phase: Phase, detail: Option<String>) {
        let mut hosts = match self.hosts.write() {
            Ok(guard) => guard,
            // A poisoned lock means a previous writer panicked. The data is
            // observational, so recovering and carrying on beats taking the
            // boot server down over a stale report.
            Err(poisoned) => poisoned.into_inner(),
        };

        if hosts.len() >= MAX_TRACKED && !hosts.contains_key(&mac) {
            Self::evict_finished(&mut hosts);
        }

        let now = now_unix();
        hosts
            .entry(mac.clone())
            .and_modify(|state| {
                // Phases only move forward, so a late-arriving duplicate of an
                // earlier fetch cannot drag a running host back to "script
                // fetched". Failure is the exception: it can arrive at any time.
                if phase >= state.phase || phase == Phase::Failed {
                    state.phase = phase;
                    state.detail = detail.clone();
                    state.updated_unix = now;
                }
            })
            .or_insert_with(|| HostState {
                mac,
                phase,
                detail,
                pallet_version: None,
                hardware: None,
                first_seen_unix: now,
                updated_unix: now,
            });
    }

    /// Note which boot pallet version a host was served.
    pub fn set_pallet_version(&self, mac: &Mac, version: &str) {
        let mut hosts = match self.hosts.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(state) = hosts.get_mut(mac) {
            state.pallet_version = Some(version.to_owned());
        }
    }

    /// Record the inventory a running node reported about itself.
    ///
    /// A node can report inventory before its `running` report lands — the two
    /// are separate requests — so this creates the entry if it has to, at the
    /// phase that a machine able to describe itself is necessarily in.
    pub fn set_inventory(&self, mac: &Mac, hardware: Inventory) {
        let mut hosts = match self.hosts.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if hosts.len() >= MAX_TRACKED && !hosts.contains_key(mac) {
            Self::evict_finished(&mut hosts);
        }

        let now = now_unix();
        hosts
            .entry(mac.clone())
            .and_modify(|state| {
                state.hardware = Some(hardware.clone());
                state.updated_unix = now;
            })
            .or_insert_with(|| HostState {
                mac: mac.clone(),
                phase: Phase::Running,
                detail: None,
                pallet_version: None,
                hardware: Some(hardware),
                first_seen_unix: now,
                updated_unix: now,
            });
    }

    /// One host, for the status writer.
    pub fn get(&self, mac: &Mac) -> Option<HostState> {
        match self.hosts.read() {
            Ok(guard) => guard.get(mac).cloned(),
            Err(poisoned) => poisoned.into_inner().get(mac).cloned(),
        }
    }

    fn evict_finished(hosts: &mut HashMap<Mac, HostState>) {
        hosts.retain(|_, state| !state.phase.is_terminal());
        // Still full of in-flight hosts: drop the least recently updated so the
        // map stays bounded even in the pathological case.
        if hosts.len() >= MAX_TRACKED
            && let Some(oldest) = hosts
                .values()
                .min_by_key(|s| s.updated_unix)
                .map(|s| s.mac.clone())
        {
            hosts.remove(&oldest);
        }
    }

    pub fn snapshot(&self) -> Vec<HostState> {
        let hosts = match self.hosts.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut all: Vec<_> = hosts.values().cloned().collect();
        all.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then(a.mac.cmp(&b.mac)));
        all
    }

    /// Count of hosts in each phase, for the console summary and metrics.
    pub fn phase_counts(&self) -> Vec<(Phase, usize)> {
        let hosts = match self.hosts.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut counts: HashMap<Phase, usize> = HashMap::new();
        for state in hosts.values() {
            *counts.entry(state.phase).or_default() += 1;
        }
        let mut out: Vec<_> = counts.into_iter().collect();
        out.sort_by_key(|(phase, _)| *phase);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(last: &str) -> Mac {
        Mac::parse(&format!("aa:bb:cc:dd:ee:{last}")).unwrap()
    }

    #[test]
    fn phases_only_move_forward() {
        let state = BootState::default();
        state.observe(mac("01"), Phase::ScriptFetched, None);
        state.observe(mac("01"), Phase::Running, None);
        // A duplicate asset fetch arriving late must not rewind a running host.
        state.observe(mac("01"), Phase::AssetsFetched, None);

        let snap = state.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].phase, Phase::Running);
    }

    #[test]
    fn failure_can_arrive_at_any_time() {
        let state = BootState::default();
        state.observe(mac("02"), Phase::Running, None);
        state.observe(mac("02"), Phase::Failed, Some("nvme attach timed out".into()));

        let snap = state.snapshot();
        assert_eq!(snap[0].phase, Phase::Failed);
        assert_eq!(snap[0].detail.as_deref(), Some("nvme attach timed out"));
    }

    #[test]
    fn counts_group_by_phase() {
        let state = BootState::default();
        state.observe(mac("01"), Phase::ScriptFetched, None);
        state.observe(mac("02"), Phase::ScriptFetched, None);
        state.observe(mac("03"), Phase::Local, None);

        let counts = state.phase_counts();
        assert_eq!(counts, vec![(Phase::ScriptFetched, 2), (Phase::Local, 1)]);
    }

    #[test]
    fn finished_hosts_are_terminal() {
        assert!(Phase::Local.is_terminal());
        assert!(Phase::Failed.is_terminal());
        assert!(!Phase::Assimilating.is_terminal());
    }

    #[test]
    fn inventory_arriving_first_still_creates_the_host() {
        let state = BootState::default();
        state.set_inventory(
            &mac("05"),
            Inventory {
                cpus: 64,
                memory_kb: 268_435_456,
                product: Some("PowerEdge R650".into()),
                serial: Some("ABC123".into()),
                disks: vec!["nvme0n1".into()],
            },
        );

        let host = state.get(&mac("05")).unwrap();
        // Only a machine that is up can describe itself.
        assert_eq!(host.phase, Phase::Running);
        assert_eq!(host.hardware.unwrap().cpus, 64);
    }

    #[test]
    fn inventory_does_not_disturb_the_phase_of_a_host_that_moved_on() {
        let state = BootState::default();
        state.observe(mac("06"), Phase::Assimilating, None);
        state.set_inventory(&mac("06"), Inventory { cpus: 8, ..Default::default() });
        assert_eq!(state.get(&mac("06")).unwrap().phase, Phase::Assimilating);
    }

    #[test]
    fn every_phase_has_a_distinct_slug() {
        let phases = [
            Phase::ScriptFetched,
            Phase::AssetsFetched,
            Phase::RootAttached,
            Phase::Running,
            Phase::Assimilating,
            Phase::Local,
            Phase::Failed,
        ];
        let mut slugs: Vec<&str> = phases.iter().map(|p| p.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), phases.len());
        // The slug and the serde spelling are the same string, deliberately.
        for phase in phases {
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.slug())
            );
        }
    }

    #[test]
    fn pallet_version_is_recorded() {
        let state = BootState::default();
        state.observe(mac("04"), Phase::ScriptFetched, None);
        state.set_pallet_version(&mac("04"), "10.20");
        assert_eq!(state.snapshot()[0].pallet_version.as_deref(), Some("10.20"));
    }
}
