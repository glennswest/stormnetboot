//! Prometheus exposition.
//!
//! Hand-rolled counters rather than a metrics crate: the set is small, and a
//! boot server that thousands of machines hit at once should carry as little
//! machinery in that path as possible.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::state::Phase;

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
}

impl Metrics {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render_with_phases(&self, phases: &[(Phase, usize)]) -> String {
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
                phase_label(*phase)
            ));
        }

        out
    }
}

fn phase_label(phase: Phase) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_exposition_including_phase_labels() {
        let m = Metrics::default();
        Metrics::incr(&m.requests_total);
        Metrics::incr(&m.refused);

        let out = m.render_with_phases(&[(Phase::Running, 3), (Phase::Failed, 1)]);

        assert!(out.contains("stormnetboot_requests_total 1"));
        assert!(out.contains("stormnetboot_refused_total 1"));
        assert!(out.contains("stormnetboot_hosts{phase=\"running\"} 3"));
        assert!(out.contains("stormnetboot_hosts{phase=\"failed\"} 1"));

        // Every metric must be preceded by its HELP and TYPE.
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            assert!(out.contains(&format!("# TYPE {name} ")), "missing TYPE for {name}");
        }
    }

    #[test]
    fn no_phases_still_renders() {
        let out = Metrics::default().render_with_phases(&[]);
        assert!(out.contains("stormnetboot_build_info"));
        assert!(out.contains("# TYPE stormnetboot_hosts gauge"));
    }
}
