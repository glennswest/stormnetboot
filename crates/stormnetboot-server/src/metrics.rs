//! Prometheus exposition.
//!
//! Hand-rolled counters rather than a metrics crate: the set is small, and a
//! boot server that thousands of machines hit at once should carry as little
//! machinery in that path as possible.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub boot_script_requests: AtomicU64,
    pub asset_requests: AtomicU64,
    pub not_found: AtomicU64,
}

impl Metrics {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut out = String::with_capacity(512);
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
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }
        out
    }
}
