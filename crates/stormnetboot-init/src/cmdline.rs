//! Kernel command line parsing.
//!
//! The parameter names are stormblock's existing contract, not new ones. Two
//! traps live here and both are load-bearing:
//!
//! * `stormblock.volume` has **no** `rd.` prefix. Every neighbouring parameter
//!   does, so it reads like a typo and gets "corrected" into a name nothing
//!   parses.
//! * `rd.stormblock.port` defaults to 3260 in the iSCSI path. This is the
//!   NVMe/TCP path, where the port is per-export and arrives from the claim —
//!   guessing 4420 attaches the wrong thing or nothing at all.

/// Everything the initramfs needs, taken from `/proc/cmdline`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootParams {
    /// NVMe/TCP portal address (host or IP, never `host:port`).
    pub portal: Option<String>,
    /// Portal port. Per-export, so there is no safe default.
    pub port: Option<u16>,
    /// Subsystem NQN to attach.
    pub nqn: Option<String>,
    /// Namespace within that subsystem.
    pub nsid: Option<u32>,
    /// Volume to boot from within the attached slab.
    pub volume: Option<String>,
    /// Local disk to assimilate onto in the background.
    pub local_disk: Option<String>,
    /// Identity pinned at PXE time.
    pub hostname: Option<String>,
    pub role: Option<String>,
    /// Where to report boot progress.
    pub report_url: Option<String>,
    /// This machine's MAC, so reports identify it the same way the server does.
    pub mac: Option<String>,
    /// Fall back to a local slab (a device path) instead of the network.
    pub slab: Option<String>,
}

impl BootParams {
    pub fn parse(cmdline: &str) -> Self {
        let mut p = Self::default();

        for token in cmdline.split_whitespace() {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            if value.is_empty() {
                continue;
            }

            match key {
                "rd.stormblock.portal" => p.portal = Some(value.to_owned()),
                "rd.stormblock.port" => p.port = value.parse().ok(),
                "rd.stormblock.nqn" => p.nqn = Some(value.to_owned()),
                "rd.stormblock.nsid" => p.nsid = value.parse().ok(),
                // No `rd.` prefix: this is the engine's existing spelling.
                "stormblock.volume" => p.volume = Some(value.to_owned()),
                "rd.stormblock.local-disk" => p.local_disk = Some(value.to_owned()),
                "rd.stormblock.slab" => p.slab = Some(value.to_owned()),
                "storm.hostname" => p.hostname = Some(value.to_owned()),
                "storm.role" => p.role = Some(value.to_owned()),
                "storm.report" => p.report_url = Some(value.to_owned()),
                "storm.mac" | "BOOTIF" => p.mac = Some(normalise_bootif(value)),
                _ => {}
            }
        }

        p
    }

    /// The `nvme-tcp://` URI stormblock takes anywhere a device path is
    /// accepted. This is what makes a remote root an ordinary slab.
    ///
    /// Returns `None` unless portal, port and NQN are all present: a partial
    /// set can only produce a URI that attaches something unintended.
    pub fn nvme_uri(&self) -> Option<String> {
        let portal = self.portal.as_deref()?;
        let port = self.port?;
        let nqn = self.nqn.as_deref()?;
        let nsid = self.nsid.unwrap_or(1);
        Some(format!("nvme-tcp://{portal}:{port}/{nqn}?nsid={nsid}"))
    }

    /// Whether this is a network boot at all.
    pub fn is_network_boot(&self) -> bool {
        self.nvme_uri().is_some()
    }

    /// What is missing, for an error a human can act on at 3am.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.portal.is_none() {
            missing.push("rd.stormblock.portal");
        }
        if self.port.is_none() {
            missing.push("rd.stormblock.port");
        }
        if self.nqn.is_none() {
            missing.push("rd.stormblock.nqn");
        }
        missing
    }
}

/// PXE's `BOOTIF` arrives as `01-aa-bb-cc-dd-ee-ff`: a hardware-type prefix
/// then dash separators. Strip the prefix so it matches everywhere else.
fn normalise_bootif(value: &str) -> String {
    let trimmed = value.strip_prefix("01-").unwrap_or(value);
    trimmed.replace('-', ":").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "BOOT_IMAGE=/vmlinuz rd.stormblock.portal=192.168.8.20 \
        rd.stormblock.port=4431 rd.stormblock.nqn=nqn.2026-08.lo.gt:img-abc-c1 \
        rd.stormblock.nsid=1 stormblock.volume=clone-stormcos-18f3 \
        rd.stormblock.local-disk=/dev/sda storm.hostname=node7 storm.role=worker \
        storm.report=http://boot.storm.lo:8080 root=/dev/ublkb0 console=ttyS0";

    #[test]
    fn parses_a_full_netboot_cmdline() {
        let p = BootParams::parse(FULL);
        assert_eq!(p.portal.as_deref(), Some("192.168.8.20"));
        assert_eq!(p.port, Some(4431));
        assert_eq!(p.nqn.as_deref(), Some("nqn.2026-08.lo.gt:img-abc-c1"));
        assert_eq!(p.nsid, Some(1));
        assert_eq!(p.volume.as_deref(), Some("clone-stormcos-18f3"));
        assert_eq!(p.local_disk.as_deref(), Some("/dev/sda"));
        assert_eq!(p.hostname.as_deref(), Some("node7"));
        assert_eq!(p.role.as_deref(), Some("worker"));
        assert!(p.is_network_boot());
    }

    #[test]
    fn builds_the_uri_stormblock_accepts() {
        let p = BootParams::parse(FULL);
        assert_eq!(
            p.nvme_uri().unwrap(),
            "nvme-tcp://192.168.8.20:4431/nqn.2026-08.lo.gt:img-abc-c1?nsid=1"
        );
    }

    #[test]
    fn nsid_defaults_to_one_but_the_port_never_defaults() {
        let p = BootParams::parse("rd.stormblock.portal=h rd.stormblock.port=4420 rd.stormblock.nqn=nqn.x");
        assert_eq!(p.nvme_uri().unwrap(), "nvme-tcp://h:4420/nqn.x?nsid=1");

        // No port: refuse rather than guess 4420, which is the shared
        // subsystem and not this export.
        let p = BootParams::parse("rd.stormblock.portal=h rd.stormblock.nqn=nqn.x");
        assert!(p.nvme_uri().is_none());
        assert_eq!(p.missing(), vec!["rd.stormblock.port"]);
    }

    #[test]
    fn volume_is_spelled_without_the_rd_prefix() {
        // The prefixed spelling is not the contract and must not be honoured,
        // or a typo would silently boot the wrong volume.
        let p = BootParams::parse("rd.stormblock.volume=wrong stormblock.volume=right");
        assert_eq!(p.volume.as_deref(), Some("right"));
    }

    #[test]
    fn bootif_is_normalised_to_a_plain_mac() {
        let p = BootParams::parse("BOOTIF=01-AA-BB-CC-DD-EE-FF");
        assert_eq!(p.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn a_local_slab_boot_is_not_a_network_boot() {
        let p = BootParams::parse("rd.stormblock.slab=/dev/sda4 stormblock.volume=stormpump");
        assert!(!p.is_network_boot());
        assert_eq!(p.slab.as_deref(), Some("/dev/sda4"));
    }

    #[test]
    fn empty_values_and_bare_tokens_are_ignored() {
        let p = BootParams::parse("quiet rd.stormblock.portal= ro rd.stormblock.port=notanumber");
        assert!(p.portal.is_none());
        assert!(p.port.is_none());
        assert_eq!(p.missing().len(), 3);
    }
}
