//! iPXE script rendering.
//!
//! The script is rendered per host so the initramfs stays dumb: everything it
//! needs — where root lives, who it is, what role it will play — arrives on
//! the kernel command line. Identity is pinned here, at PXE time, which is
//! what lets day 2 be a profile change instead of a reprovision.

use crate::{config::Config, hosts::HostRecord, mac::Mac};

/// Everything needed to render one host's boot, resolved from config, the
/// host's record, and (once claims land) its attached volume.
pub struct BootPlan<'a> {
    pub mac: Option<&'a Mac>,
    pub record: Option<&'a HostRecord>,
    /// NVMe/TCP portal for this host's root, already resolved from the host
    /// record's override or the server default.
    pub portal: Option<&'a str>,
    pub portal_port: u16,
    /// Subsystem NQN of the claimed volume, when a claim has been made.
    pub nqn: Option<&'a str>,
    /// Namespace ID within that subsystem.
    pub nsid: Option<u32>,
    /// Name of the volume to boot within the attached slab.
    pub volume: Option<&'a str>,
}

impl<'a> BootPlan<'a> {
    /// Resolve a plan from config plus whatever we know about the host.
    ///
    /// A host record's portal wins over the server default: that is how a
    /// machine is steered to a nearer appliance replica.
    pub fn resolve(cfg: &'a Config, mac: Option<&'a Mac>, record: Option<&'a HostRecord>) -> Self {
        let portal = record
            .and_then(|r| r.portal.as_deref())
            .or(cfg.portal.as_deref());

        Self {
            mac,
            record,
            portal,
            portal_port: cfg.portal_port,
            nqn: None,
            nsid: None,
            volume: None,
        }
    }

    /// The kernel command line fragment describing where root comes from and
    /// who this machine is.
    ///
    /// With no portal we deliberately emit nothing rather than a guess — a
    /// node that stops in the initramfs is recoverable, a node that attaches
    /// the wrong volume is not.
    fn cmdline(&self, cfg: &Config) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(portal) = self.portal {
            parts.push(format!("rd.stormblock.portal={portal}"));
            parts.push(format!("rd.stormblock.port={}", self.portal_port));
            if let Some(nqn) = self.nqn {
                parts.push(format!("rd.stormblock.nqn={nqn}"));
            }
            if let Some(nsid) = self.nsid {
                parts.push(format!("rd.stormblock.nsid={nsid}"));
            }
            // Note the missing `rd.` — the engine's existing contract spells
            // this one without the prefix, and the initramfs parses that name.
            if let Some(volume) = self.volume {
                parts.push(format!("stormblock.volume={volume}"));
            }
            parts.push("root=/dev/ublkb0".to_owned());
        }

        // Flow-over target. Destructive to that device, so it is only ever
        // what an operator configured, never inferred.
        if let Some(disk) = &cfg.local_disk {
            parts.push(format!("rd.stormblock.local-disk={disk}"));
        }

        // Identity, pinned now and carried for the life of the node.
        if let Some(record) = self.record {
            parts.push(format!("storm.hostname={}", record.name));
            if let Some(role) = &record.role {
                parts.push(format!("storm.role={role}"));
            }
        }

        if !cfg.extra_cmdline.is_empty() {
            parts.push(cfg.extra_cmdline.clone());
        }
        if let Some(extra) = self.record.and_then(|r| r.extra_cmdline.as_deref()) {
            parts.push(extra.to_owned());
        }

        parts.join(" ")
    }
}

/// Render the per-host boot script.
pub fn render(cfg: &Config, plan: &BootPlan<'_>) -> String {
    let base = cfg.base_url();
    let cmdline = plan.cmdline(cfg);

    let mut script = String::with_capacity(512);
    script.push_str("#!ipxe\n\n");

    match (plan.mac, plan.record) {
        (Some(mac), Some(record)) => {
            script.push_str(&format!("# {mac} -> {}\n", record.name));
        }
        (Some(mac), None) => {
            script.push_str(&format!("# {mac} (no host record; serving defaults)\n"));
        }
        (None, _) => script.push_str("# no MAC supplied; serving the default boot\n"),
    }

    if plan.portal.is_none() {
        script.push_str(
            "# WARNING: no NVMe/TCP portal for this host. The kernel will boot but\n\
             # the initramfs has nowhere to attach root from.\n",
        );
    }

    script.push_str(&format!(
        "\nset base {base}\n\
         kernel ${{base}}/boot/vmlinuz initrd=initramfs.img {cmdline}\n\
         initrd ${{base}}/boot/initramfs.img\n\
         boot\n"
    ));
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn cfg(args: &[&str]) -> Config {
        let mut argv = vec!["stormnetboot-server"];
        argv.extend_from_slice(args);
        Config::parse_from(argv)
    }

    fn record(name: &str) -> HostRecord {
        HostRecord {
            mac: Mac::parse("aa:bb:cc:dd:ee:ff").unwrap(),
            name: name.to_owned(),
            role: Some("worker".into()),
            stack: None,
            portal: None,
            extra_cmdline: None,
            online: true,
            object: None,
        }
    }

    #[test]
    fn renders_portal_when_configured() {
        let cfg = cfg(&["--portal", "10.0.0.5"]);
        let mac = Mac::parse("aa:bb:cc:dd:ee:ff").unwrap();
        let plan = BootPlan::resolve(&cfg, Some(&mac), None);
        let script = render(&cfg, &plan);

        assert!(script.starts_with("#!ipxe"));
        assert!(script.contains("rd.stormblock.portal=10.0.0.5"));
        assert!(script.contains("rd.stormblock.port=4420"));
        assert!(script.contains("root=/dev/ublkb0"));
        assert!(script.contains("no host record"));
        assert!(!script.contains("WARNING"));
    }

    #[test]
    fn warns_instead_of_guessing_without_a_portal() {
        let cfg = cfg(&[]);
        let plan = BootPlan::resolve(&cfg, None, None);
        let script = render(&cfg, &plan);
        assert!(script.contains("WARNING"));
        assert!(!script.contains("rd.stormblock.portal"));
    }

    #[test]
    fn host_record_pins_identity_on_the_cmdline() {
        let cfg = cfg(&["--portal", "10.0.0.5"]);
        let mac = Mac::parse("aa:bb:cc:dd:ee:ff").unwrap();
        let rec = record("node7");
        let plan = BootPlan::resolve(&cfg, Some(&mac), Some(&rec));
        let script = render(&cfg, &plan);

        assert!(script.contains("storm.hostname=node7"));
        assert!(script.contains("storm.role=worker"));
        assert!(script.contains("# aa:bb:cc:dd:ee:ff -> node7"));
    }

    #[test]
    fn host_record_portal_overrides_the_server_default() {
        let cfg = cfg(&["--portal", "10.0.0.5"]);
        let mut rec = record("node7");
        rec.portal = Some("10.9.9.9".into());
        let plan = BootPlan::resolve(&cfg, None, Some(&rec));
        let script = render(&cfg, &plan);

        assert!(script.contains("rd.stormblock.portal=10.9.9.9"));
        assert!(!script.contains("10.0.0.5"));
    }

    #[test]
    fn claim_details_reach_the_cmdline() {
        let cfg = cfg(&["--portal", "10.0.0.5"]);
        let mut plan = BootPlan::resolve(&cfg, None, None);
        plan.nqn = Some("nqn.2026-01.io.storm:vol-7");
        plan.nsid = Some(1);
        plan.volume = Some("clone-stormcos-18f3");
        let script = render(&cfg, &plan);

        assert!(script.contains("rd.stormblock.nqn=nqn.2026-01.io.storm:vol-7"));
        assert!(script.contains("rd.stormblock.nsid=1"));
        // The engine's existing contract spells this one without `rd.`.
        assert!(script.contains("stormblock.volume=clone-stormcos-18f3"));
        assert!(!script.contains("rd.stormblock.volume="));
    }

    #[test]
    fn flow_over_target_is_passed_through_when_configured() {
        let with_disk = cfg(&["--portal", "10.0.0.5", "--local-disk", "/dev/sda"]);
        let plan = BootPlan::resolve(&with_disk, None, None);
        assert!(render(&with_disk, &plan).contains("rd.stormblock.local-disk=/dev/sda"));

        let without = cfg(&["--portal", "10.0.0.5"]);
        let plan = BootPlan::resolve(&without, None, None);
        assert!(!render(&without, &plan).contains("local-disk"));
    }

    #[test]
    fn base_url_trailing_slash_does_not_double_up() {
        let cfg = cfg(&["--base-url", "http://boot.lo:8080/"]);
        let plan = BootPlan::resolve(&cfg, None, None);
        assert!(render(&cfg, &plan).contains("set base http://boot.lo:8080\n"));
    }

    #[test]
    fn both_server_and_host_extra_cmdline_are_appended() {
        let cfg = cfg(&["--portal", "10.0.0.5", "--extra-cmdline", "console=ttyS0"]);
        let mut rec = record("node7");
        rec.extra_cmdline = Some("debug".into());
        let plan = BootPlan::resolve(&cfg, None, Some(&rec));
        let script = render(&cfg, &plan);

        assert!(script.contains("console=ttyS0"));
        assert!(script.contains("debug"));
    }
}
