//! Prometheus exposition.
//!
//! Hand-rolled counters rather than a metrics crate: the set is small, and a
//! boot server that thousands of machines hit at once should carry as little
//! machinery in that path as possible.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::{hosts::HostCounts, state::Phase};

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub boot_script_requests: AtomicU64,
    pub asset_requests: AtomicU64,
    pub not_found: AtomicU64,
    /// Boot requests refused: unknown host under a deny policy, or a MAC we
    /// could not parse.
    pub refused: AtomicU64,
    pub claim_failures: AtomicU64,
    /// Successful boot pallet refreshes that changed what we serve.
    pub pallet_refreshes: AtomicU64,
    pub pallet_refresh_failures: AtomicU64,
    /// `BootHost` status subresource patches written.
    pub status_writes: AtomicU64,
    pub status_write_failures: AtomicU64,
    /// Errors from the `BootHost` watch. The watch retries on its own, so
    /// this rising while records stay current is noise; it rising while they
    /// go stale is the alert.
    pub watch_errors: AtomicU64,
}

impl Metrics {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self, phases: &[(Phase, usize)], hosts: HostCounts) -> String {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut out = String::with_capacity(1024);

        out.push_str(concat!(
            "# HELP stormnetboot_build_info Build information.\n",
            "# TYPE stormnetboot_build_info gauge\n",
        ));
        out.push_str(&format!(
            "stormnetboot_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        ));

        for (name, help, value) in [
            (
                "stormnetboot_requests_total",
                "Total HTTP requests served.",
                load(&self.requests_total),
            ),
            (
                "stormnetboot_boot_script_requests_total",
                "Rendered iPXE boot scripts served.",
                load(&self.boot_script_requests),
            ),
            (
                "stormnetboot_asset_requests_total",
                "Boot asset requests (kernel, initramfs, ISO, iPXE binaries).",
                load(&self.asset_requests),
            ),
            (
                "stormnetboot_not_found_total",
                "Requests that matched no route or asset.",
                load(&self.not_found),
            ),
            (
                "stormnetboot_refused_total",
                "Boot requests refused (unknown host, or unparseable MAC).",
                load(&self.refused),
            ),
            (
                "stormnetboot_claim_failures_total",
                "Failures claiming a per-host root volume.",
                load(&self.claim_failures),
            ),
            (
                "stormnetboot_pallet_refreshes_total",
                "Boot pallet refreshes that changed what is served.",
                load(&self.pallet_refreshes),
            ),
            (
                "stormnetboot_pallet_refresh_failures_total",
                "Failed attempts to refresh the boot pallet.",
                load(&self.pallet_refresh_failures),
            ),
            (
                "stormnetboot_status_writes_total",
                "BootHost status subresource patches written.",
                load(&self.status_writes),
            ),
            (
                "stormnetboot_status_write_failures_total",
                "Failed BootHost status patches.",
                load(&self.status_write_failures),
            ),
            (
                "stormnetboot_watch_errors_total",
                "Errors returned by the BootHost watch.",
                load(&self.watch_errors),
            ),
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }

        // Hosts by phase: the series an operator actually alerts on during a
        // fleet install.
        out.push_str(concat!(
            "# HELP stormnetboot_hosts Hosts currently tracked, by boot phase.\n",
            "# TYPE stormnetboot_hosts gauge\n",
        ));
        for (phase, count) in phases {
            out.push_str(&format!(
                "stormnetboot_hosts{{phase=\"{}\"}} {count}\n",
                phase.slug()
            ));
        }

        // Records by layer: a cluster layer that empties out while the file
        // layer still answers is the failure that would otherwise look like
        // nothing at all going wrong.
        out.push_str(concat!(
            "# HELP stormnetboot_host_records Host records held, by source layer.\n",
            "# TYPE stormnetboot_host_records gauge\n",
        ));
        out.push_str(&format!(
            "stormnetboot_host_records{{source=\"file\"}} {}\nstormnetboot_host_records{{source=\"boothost\"}} {}\n",
            hosts.file, hosts.kube
        ));
        out.push_str(concat!(
            "# HELP stormnetboot_boothost_synced Whether the BootHost watch has listed the cluster.\n",
            "# TYPE stormnetboot_boothost_synced gauge\n",
        ));
        out.push_str(&format!(
            "stormnetboot_boothost_synced {}\n",
            u8::from(hosts.kube_synced)
        ));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts() -> HostCounts {
        HostCounts {
            file: 2,
            kube: 5,
            total: 6,
            kube_synced: true,
        }
    }

    #[test]
    fn renders_valid_exposition_including_phase_labels() {
        let m = Metrics::default();
        Metrics::incr(&m.requests_total);
        Metrics::incr(&m.refused);

        let out = m.render(&[(Phase::Running, 3), (Phase::Failed, 1)], counts());

        assert!(out.contains("stormnetboot_requests_total 1"));
        assert!(out.contains("stormnetboot_refused_total 1"));
        assert!(out.contains("stormnetboot_hosts{phase=\"running\"} 3"));
        assert!(out.contains("stormnetboot_hosts{phase=\"failed\"} 1"));
        assert!(out.contains("stormnetboot_host_records{source=\"boothost\"} 5"));
        assert!(out.contains("stormnetboot_boothost_synced 1"));

        // Every metric must be preceded by its HELP and TYPE.
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            assert!(out.contains(&format!("# TYPE {name} ")), "missing TYPE for {name}");
        }
    }

    #[test]
    fn no_phases_still_renders() {
        let out = Metrics::default().render(&[], counts());
        assert!(out.contains("stormnetboot_build_info"));
        assert!(out.contains("# TYPE stormnetboot_hosts gauge"));
    }
}
