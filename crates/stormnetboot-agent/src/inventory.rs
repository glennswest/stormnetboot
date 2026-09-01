//! Hardware inventory, reported from the running node.
//!
//! This is what replaces Ironic's inspection boot. There is no agent ramdisk
//! and no second reboot: the machine is already running the OS it will keep,
//! so describing itself is a file read rather than a boot cycle.

use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub cpus: usize,
    pub memory_kb: u64,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub disks: Vec<String>,
}

impl Inventory {
    /// One line an operator can read in a console row.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.cpus > 0 {
            parts.push(format!("{} cpu", self.cpus));
        }
        if self.memory_kb > 0 {
            parts.push(format!("{} GiB", self.memory_kb / 1024 / 1024));
        }
        if !self.disks.is_empty() {
            parts.push(format!("{} disk(s)", self.disks.len()));
        }
        if let Some(product) = &self.product {
            parts.push(product.clone());
        }
        if parts.is_empty() {
            "inventory unavailable".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

pub fn collect() -> Inventory {
    Inventory {
        cpus: count_cpus(),
        memory_kb: total_memory_kb(),
        product: dmi("product_name"),
        serial: dmi("product_serial"),
        disks: block_devices(),
    }
}

fn count_cpus() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0)
}

fn total_memory_kb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v.to_owned()))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn dmi(field: &str) -> Option<String> {
    let path = format!("/sys/class/dmi/id/{field}");
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    // Firmware that was never programmed reports placeholders; a placeholder
    // recorded as a serial number is worse than no serial number.
    const PLACEHOLDERS: [&str; 5] = [
        "To Be Filled By O.E.M.",
        "System Serial Number",
        "Default string",
        "Not Specified",
        "None",
    ];
    if value.is_empty() || PLACEHOLDERS.iter().any(|p| p.eq_ignore_ascii_case(value)) {
        return None;
    }
    Some(value.to_owned())
}

/// Real disks only: no loop, ram, or the ublk device serving our own root.
fn block_devices() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };

    let mut disks: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            !name.starts_with("loop")
                && !name.starts_with("ram")
                && !name.starts_with("zram")
                && !name.starts_with("ublk")
                && !name.starts_with("dm-")
        })
        .collect();
    disks.sort();
    disks
}

/// MAC of the interface holding the default route — the one that PXE booted.
pub fn primary_mac() -> Option<String> {
    let route = std::fs::read_to_string("/proc/net/route").ok()?;
    let iface = route.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let iface = fields.next()?;
        let destination = fields.next()?;
        // Destination 00000000 is the default route.
        (destination == "00000000").then(|| iface.to_owned())
    })?;

    read_mac(&iface).or_else(|| {
        // No default route yet: fall back to the first real interface, which
        // on a netbooted machine is almost always the one that booted it.
        let entries = std::fs::read_dir("/sys/class/net").ok()?;
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "lo")
            .collect();
        names.sort();
        names.iter().find_map(|n| read_mac(n))
    })
}

fn read_mac(iface: &str) -> Option<String> {
    let path = format!("/sys/class/net/{iface}/address");
    if !Path::new(&path).exists() {
        return None;
    }
    let mac = std::fs::read_to_string(path).ok()?.trim().to_ascii_lowercase();
    (!mac.is_empty() && mac != "00:00:00:00:00:00").then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reads_as_a_sentence_not_a_dump() {
        let inv = Inventory {
            cpus: 64,
            memory_kb: 268_435_456,
            product: Some("PowerEdge R650".into()),
            serial: Some("ABC123".into()),
            disks: vec!["nvme0n1".into(), "sda".into()],
        };
        assert_eq!(inv.summary(), "64 cpu, 256 GiB, 2 disk(s), PowerEdge R650");
    }

    #[test]
    fn an_empty_inventory_says_so_rather_than_pretending() {
        assert_eq!(Inventory::default().summary(), "inventory unavailable");
    }

    #[test]
    fn collect_reads_this_machine_without_panicking() {
        // Runs on whatever the build box is; the point is that every reader
        // degrades to a default rather than failing.
        let inv = collect();
        assert!(inv.cpus > 0, "a running machine has at least one cpu");
        assert!(inv.memory_kb > 0, "a running machine has memory");
    }
}
