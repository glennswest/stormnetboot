//! stormbootx — a UEFI NVMe/TCP boot extension.
//!
//! Boots a machine from an image that lives in sbregistry, with no kernel, no
//! initramfs and no local media beyond the binary itself. The sequence is:
//!
//!   service tag (SMBIOS)  ->  claim from sbregistry  ->  attach nvme-tcp://
//!     ->  publish EFI_BLOCK_IO_PROTOCOL  ->  firmware boots it
//!
//! Once BlockIO is installed the firmware's own machinery does the rest: the
//! partition driver reads the GPT, the FAT driver mounts the ESP, and the boot
//! manager loads a bootloader from a disk that is not in this chassis.
//!
//! Identity is the **service tag**, not a MAC. NICs get swapped and added to,
//! and then a MAC-keyed boot server thinks it is looking at a different
//! machine. The service tag is the chassis, it needs no network to read, and
//! it is what is printed on the pull-out tab when someone has to find the box.
//!
//! Deliberately not used: EFI_HTTP (a driver stack firmware may not carry, when
//! one HTTP request over the TCP4 we already need is a hundred lines), and PXE
//! or TFTP anywhere at all.

#![no_main]
#![no_std]

extern crate alloc;

mod blockio;
mod config;
mod nvme;
mod registry;
mod smbios;
mod tcp4;

use alloc::format;
use alloc::string::String;

use uefi::prelude::*;

/// Where sbregistry lives, for the claim path.
const REGISTRY_IP: [u8; 4] = [192, 168, 200, 22];
const REGISTRY_PORT: u16 = 5100;
const REGISTRY_HOST: &str = "sbregistry.gt.lo:5100";

/// The golden to claim when this machine has no clone yet.
const GOLDEN: &str = "stormcos-edge";

/// Attach a fixed target instead of asking the registry.
///
/// The registry claim is the model — a CoW clone per service tag, bound to the
/// machine that holds it. This exists because the two halves have to be proven
/// separately: a direct attach exercises SMBIOS, TCP4, the NVMe/TCP handshake
/// and BlockIO with nothing else in the path, so a failure here is a failure in
/// *this* binary rather than in a claim that returned the wrong thing. Set
/// `USE_REGISTRY` once the volume being served is a per-machine clone.
const USE_REGISTRY: bool = false;
const DIRECT_PORTAL: [u8; 4] = [192, 168, 31, 202]; // forge.g16.lo, eth1 (MTU 9000)
const DIRECT_PORT: u16 = 4420;
const DIRECT_NQN: &str = "nqn.2026-09.lo.g16:stormcos";
const DIRECT_NSID: u32 = 2; // drives[1] = stormcos-sno-10.21.img

fn banner(line: &str) {
    uefi::println!("{line}");
}

fn run() -> Result<(), String> {
    banner("");
    banner("stormbootx — NVMe/TCP boot extension");
    banner("============================================================");

    // 1. Who am I? No network, no configuration, no BMC.
    let tag = smbios::service_tag().ok_or("SMBIOS carries no system serial number")?;
    uefi::println!("service tag : {tag}");

    // 2. Is there a usable TCP stack? Presence of SNP is not enough — the
    //    layered IP4/TCP4 drivers are a separate build option in firmware.
    if !tcp4::available() {
        return Err(
            "EFI_TCP4 is not present. Enable network boot / the NIC's UEFI PXE stack \
             in setup so the firmware loads its TCP/IP drivers."
                .into(),
        );
    }
    uefi::println!("tcp4        : available");

    // 3. What should I boot?
    let attach = if USE_REGISTRY {
        // Reuse a clone this machine already holds, so a reboot reattaches the
        // same volume rather than minting another.
        uefi::println!("registry    : {REGISTRY_HOST}");
        match registry::existing(REGISTRY_IP, REGISTRY_PORT, REGISTRY_HOST, &tag)? {
            Some(a) => {
                uefi::println!("  reattaching the clone already bound to {tag}");
                a
            }
            None => {
                uefi::println!("  no clone for {tag}; claiming from golden {GOLDEN}");
                registry::claim(REGISTRY_IP, REGISTRY_PORT, REGISTRY_HOST, GOLDEN, &tag)?
            }
        }
    } else {
        // Compiled values are only the floor. \stormboot\stormboot.conf on the
        // media overrides them, so a portal that moves is a text edit rather
        // than a rebuild — which it has been twice already.
        let cfg = config::resolve(DIRECT_PORTAL, DIRECT_PORT, DIRECT_NQN, DIRECT_NSID);
        uefi::println!("config      : {}", cfg.source);
        registry::Attach {
            address: cfg.portal,
            port: cfg.port,
            nqn: cfg.nqn,
            nsid: cfg.nsid,
        }
    };

    let [a, b, c, d] = attach.address;
    uefi::println!(
        "  portal    : {a}.{b}.{c}.{d}:{}  nsid {}",
        attach.port,
        attach.nsid
    );
    uefi::println!("  nqn       : {}", attach.nqn);

    // 4. Attach. The host NQN is derived from the service tag so the target
    //    sees a stable initiator identity across reboots.
    let hostnqn = format!("nqn.2026-09.lo.storm:host-{tag}");
    uefi::println!("attaching   : {hostnqn}");

    let ns = nvme::Namespace::attach(
        attach.address,
        attach.port,
        &attach.nqn,
        attach.nsid,
        &hostnqn,
    )?;

    let g = ns.geometry;
    let gib = (g.blocks.saturating_mul(g.block_size as u64)) / (1024 * 1024 * 1024);
    uefi::println!(
        "  namespace : {} blocks x {} bytes  ({gib} GiB)",
        g.blocks,
        g.block_size
    );

    // 5. Hand it to the firmware as an ordinary disk.
    let handle = blockio::publish(ns)?;
    uefi::println!("blockio     : published on handle {handle:p}");

    banner("");
    banner("RESULT: remote image is a local disk. Firmware can boot it.");
    banner("============================================================");
    Ok(())
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    match run() {
        Ok(()) => {
            // Stay resident briefly so the console is readable, then return so
            // the boot manager proceeds to the disk just published.
            uefi::boot::stall(core::time::Duration::from_secs(5));
            Status::SUCCESS
        }
        Err(err) => {
            uefi::println!("");
            uefi::println!("RESULT: FAILED — {err}");
            uefi::println!("============================================================");
            // Long enough to read over a serial console before the firmware
            // moves on to the next boot option.
            uefi::boot::stall(core::time::Duration::from_secs(30));
            Status::ABORTED
        }
    }
}
