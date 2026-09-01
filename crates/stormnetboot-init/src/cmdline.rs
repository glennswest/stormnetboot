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
    /// Local slabs to attach instead of, or alongside, the network root.
    ///
    /// A list because the engine has always taken `--slab` repeatably: the
    /// system slab holds the goldens an image replaces, and a data slab holds
    /// what must outlive it.
    pub slabs: Vec<String>,
    /// The slab holding node identity and per-service data — tier-0
    /// (`/data/stormcert`: the node CA, the apiserver cert, the
    /// ServiceAccount signing key) and the `-data` volumes.
    ///
    /// Attached like any other slab, but tracked separately because it is the
    /// one thing an install must never format. Re-minting tier-0 silently
    /// invalidates every ServiceAccount token in the cluster.
    pub data_slab: Option<String>,
    /// The ESP this machine booted from, when it booted from local media
    /// rather than PXE.
    ///
    /// Naming it explicitly rather than probing is the same rule as the rest
    /// of this file: a machine that guesses which partition is its boot media
    /// can overwrite the wrong one, and unlike a failed attach that is not
    /// recoverable. Absent means the media refresh does not run at all.
    pub media_dev: Option<String>,
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
                // Comma-separated: the engine takes --slab repeatably, so the
                // command line should be able to name more than one too.
                "rd.stormblock.slab" => {
                    p.slabs
                        .extend(value.split(',').filter(|s| !s.is_empty()).map(str::to_owned));
                }
                "rd.stormblock.data-slab" => p.data_slab = Some(value.to_owned()),
                // Ours, not the engine's: the engine has no concept of the
                // media the machine booted from.
                "rd.stormnetboot.media" => p.media_dev = Some(value.to_owned()),
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

    /// Every slab to attach, in order: the root source first, then the data
    /// slab, then any others named.
    ///
    /// The data slab is attached like any other — what makes it special is
    /// only that nothing may format it.
    pub fn all_slabs(&self) -> Vec<String> {
        let mut slabs = Vec::new();
        if let Some(uri) = self.nvme_uri() {
            slabs.push(uri);
        }
        slabs.extend(self.slabs.iter().cloned());
        if let Some(data) = &self.data_slab
            && !slabs.iter().any(|s| s == data)
        {
            slabs.push(data.clone());
        }
        slabs
    }

    /// Refuse a flow-over that would destroy the data slab.
    ///
    /// `--local-disk` formats its target. Pointing it at the disk holding
    /// tier-0 would wipe the node's CA and its ServiceAccount signing key,
    /// which invalidates every token in the cluster — and it would do it
    /// silently, in the background, while the node looks healthy.
    pub fn check_local_disk(&self) -> Result<(), String> {
        let (Some(disk), Some(data)) = (&self.local_disk, &self.data_slab) else {
            return Ok(());
        };

        // Compare the underlying device, so /dev/sda2 as a data slab also
        // protects against a flow-over onto /dev/sda.
        let disk_base = device_base(disk);
        let data_base = device_base(data);
        if disk_base == data_base {
            return Err(format!(
                "refusing flow-over onto {disk}: it holds the data slab {data}, \
                 and formatting it would destroy this node's identity"
            ));
        }
        Ok(())
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

/// Reduce a partition path to its whole device, so `/dev/sda2` and `/dev/sda`
/// compare equal.
///
/// Two naming schemes, and conflating them is the trap: `sda2` is a partition
/// of `sda`, but `nvme0n1` is a *whole device* whose name simply ends in a
/// digit — its partitions are `nvme0n1p2`. Blindly trimming trailing digits
/// turns `/dev/nvme0n1` into `/dev/nvme0n`, which matches nothing, and the
/// guard that depends on it silently stops guarding.
fn device_base(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let prefix = &path[..path.len() - name.len()];

    // `nvme0n1p2`, `mmcblk0p1`: a `p` preceded by a digit and followed only by
    // digits is a partition suffix.
    if let Some(idx) = name.rfind('p') {
        let (head, tail) = name.split_at(idx);
        let digits = &tail[1..];
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && head.ends_with(|c: char| c.is_ascii_digit())
        {
            return format!("{prefix}{head}");
        }
    }

    // Names whose trailing digits belong to the device itself.
    if name.starts_with("nvme") || name.starts_with("mmcblk") || name.starts_with("loop") {
        return path.to_owned();
    }

    // `sda2` → `sda`.
    format!(
        "{prefix}{}",
        name.trim_end_matches(|c: char| c.is_ascii_digit())
    )
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
        assert_eq!(p.slabs, vec!["/dev/sda4"]);
    }

    #[test]
    fn empty_values_and_bare_tokens_are_ignored() {
        let p = BootParams::parse("quiet rd.stormblock.portal= ro rd.stormblock.port=notanumber");
        assert!(p.portal.is_none());
        assert!(p.port.is_none());
        assert_eq!(p.missing().len(), 3);
    }
    #[test]
    fn slab_accepts_a_comma_separated_list() {
        // The engine has always taken --slab repeatably; the command line
        // should be able to say so too.
        let p = BootParams::parse("rd.stormblock.slab=/dev/sda4,/dev/sda5");
        assert_eq!(p.slabs, vec!["/dev/sda4", "/dev/sda5"]);
    }

    #[test]
    fn the_data_slab_is_attached_alongside_the_root_source() {
        let p = BootParams::parse(
            "rd.stormblock.portal=h rd.stormblock.port=4431 rd.stormblock.nqn=nqn.x \
             rd.stormblock.data-slab=/dev/sdb1",
        );
        let slabs = p.all_slabs();
        assert_eq!(slabs.len(), 2);
        assert!(slabs[0].starts_with("nvme-tcp://"));
        assert_eq!(slabs[1], "/dev/sdb1");
    }

    #[test]
    fn a_data_slab_already_named_as_a_slab_is_not_attached_twice() {
        let p = BootParams::parse(
            "rd.stormblock.slab=/dev/sdb1 rd.stormblock.data-slab=/dev/sdb1",
        );
        assert_eq!(p.all_slabs(), vec!["/dev/sdb1"]);
    }

    #[test]
    fn flow_over_onto_the_data_slab_is_refused() {
        // The failure this prevents is silent and total: formatting tier-0
        // re-mints the node CA and the ServiceAccount signing key, which
        // invalidates every token in the cluster.
        let p = BootParams::parse(
            "rd.stormblock.data-slab=/dev/sda2 rd.stormblock.local-disk=/dev/sda",
        );
        let err = p.check_local_disk().unwrap_err();
        assert!(err.contains("refusing flow-over"), "{err}");
        assert!(err.contains("identity"), "{err}");
    }

    #[test]
    fn flow_over_onto_a_different_disk_is_allowed() {
        let p = BootParams::parse(
            "rd.stormblock.data-slab=/dev/sdb1 rd.stormblock.local-disk=/dev/sda",
        );
        assert!(p.check_local_disk().is_ok());
    }

    #[test]
    fn partition_suffixes_do_not_hide_the_same_device() {
        assert_eq!(device_base("/dev/sda2"), "/dev/sda");
        assert_eq!(device_base("/dev/sda"), "/dev/sda");
        assert_eq!(device_base("/dev/nvme0n1p2"), "/dev/nvme0n1");
        assert_eq!(device_base("/dev/nvme0n1"), "/dev/nvme0n1");
        assert_eq!(device_base("/dev/mmcblk0p1"), "/dev/mmcblk0");
        assert_eq!(device_base("/dev/mmcblk0"), "/dev/mmcblk0");
        // Two different NVMe namespaces must NOT collapse together, or the
        // guard would refuse a perfectly safe flow-over.
        assert_ne!(device_base("/dev/nvme0n1"), device_base("/dev/nvme0n2"));

        // nvme partition vs whole device must still be caught
        let p = BootParams::parse(
            "rd.stormblock.data-slab=/dev/nvme0n1p2 rd.stormblock.local-disk=/dev/nvme0n1",
        );
        assert!(p.check_local_disk().is_err());
    }

}
