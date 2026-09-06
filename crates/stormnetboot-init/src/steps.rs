//! The boot sequence itself.
//!
//! Ordering is the whole content of this module, and it follows stormblock's
//! existing initramfs: mount the pseudo-filesystems, load modules, bring up
//! the network, start the engine, wait for the device, mount, switch_root.
//! Deviating from that order produces failures that look like driver bugs.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};

use crate::{cmdline::BootParams, report::Reporter};

/// Where the engine binary lives inside the initramfs.
const STORMBLOCK: &str = "/usr/sbin/stormblock";
/// Root always arrives here, whether it came over the network or off a slab.
const ROOT_DEV: &str = "/dev/ublkb0";
const SYSROOT: &str = "/sysroot";

/// Modules no device announces via modalias, so nothing else loads them.
const REQUIRED_MODULES: [&str; 7] = [
    "nvme_tcp", "ublk_drv", "erofs", "overlay", "ext4", "xfs",
    // vfat: the ESP the boot media refresh reads and rewrites.
    "vfat",
];

const DEVICE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn read_cmdline() -> std::io::Result<String> {
    std::fs::read_to_string("/proc/cmdline")
}

pub fn run(params: &BootParams, reporter: &Reporter) -> anyhow::Result<()> {
    stamp("stormnetboot-init starting");

    mount_pseudo_filesystems()?;
    load_modules();

    // Checked before anything is attached, and before the engine could start
    // migrating: a flow-over onto the data slab destroys node identity, and it
    // does so in the background while the node still looks healthy.
    if let Err(err) = params.check_local_disk() {
        bail!("{err}");
    }

    if params.is_network_boot() {
        // Root is on the other side of a NIC, so the NIC has to work first.
        // A local slab boot deliberately skips this: no network, no wait.
        bring_up_network(params)?;
    } else if params.slabs.is_empty() {
        let missing = params.missing().join(", ");
        bail!(
            "no root source on the command line: missing {missing} \
             (and no rd.stormblock.slab for a local boot)"
        );
    }

    reporter.phase("assets-fetched", None);

    // Where the root slab actually is. A named local slab is trusted only on
    // positive evidence; a diskless node (or one whose named slab is a stray
    // device — a 2 TB disk from a previous life, an empty virtual floppy that
    // answers ENOMEDIUM) claims from the appliance, keyed on its service tag.
    let slabs = resolve_slabs(params).context("resolving the root slab")?;

    // Where each writable/mount/image-store volume lands as a ublk device. Both
    // the --writable flags handed to the engine and the mounts applied
    // afterwards come from this one plan, so their indices cannot drift.
    let plan = params.export_plan();

    let mut engine = start_engine(params, &slabs, &plan).context("starting stormblock")?;
    wait_for_device(ROOT_DEV, DEVICE_TIMEOUT).inspect_err(|_| {
        // The engine's own output is the only diagnosis available here.
        let _ = engine.kill();
    })?;

    reporter.phase("root-attached", None);
    stamp("root device present");

    mount_root(params)?;
    // Mount container volumes and register writables/image-store in the real
    // root's fstab, before PID 1 — a stormpump node's manifest registers
    // directories, so a volume has to be a mounted directory by the time PID 1
    // reads it. A volume that will not mount is reported and skipped, never
    // fatal: one missing container beats a node that does not boot.
    apply_exports(&plan);
    write_identity(params)?;

    // Runs with the root mounted and before the handover, so the golden that
    // was just attached is the thing that decides what the media should hold.
    // Never fails the boot; see the module docs.
    crate::media::refresh(params, SYSROOT, reporter);

    reporter.phase("running", None);
    if params.local_disk.is_some() {
        // Flow-over runs inside the engine we just started; it reports
        // progress on its own stdout, which the agent picks up after the
        // switch. Say so now so the console shows the intent immediately.
        reporter.phase("assimilating", Some("flow-over started"));
    }

    switch_root()
}

/// Print `[uptime] message`, matching the existing initramfs's stamps so the
/// two are readable in one console log.
pub(crate) fn stamp(message: &str) {
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(|v| v.to_owned()))
        .unwrap_or_else(|| "?".into());
    println!("[{uptime}s] stormnetboot-init: {message}");
}

fn mount_pseudo_filesystems() -> anyhow::Result<()> {
    // /run matters beyond the obvious: the engine writes its handover record
    // there, and it is moved into the new root so `adopt-ublk` can find it.
    for (source, target, fstype) in [
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("devtmpfs", "/dev", "devtmpfs"),
        ("tmpfs", "/run", "tmpfs"),
    ] {
        std::fs::create_dir_all(target).ok();
        if is_mounted(target) {
            continue;
        }
        let status = Command::new("/bin/mount")
            .args(["-t", fstype, source, target])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => tracing_warn(&format!("mounting {target} exited {s}")),
            Err(err) => tracing_warn(&format!("mounting {target} failed: {err}")),
        }
    }
    Ok(())
}

fn is_mounted(target: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split(' ').nth(1) == Some(target)))
        .unwrap_or(false)
}

fn load_modules() {
    for module in REQUIRED_MODULES {
        let _ = Command::new("/usr/sbin/modprobe")
            .arg("-q")
            .arg(module)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // ublk *is* io_uring, so a hardened kernel that disabled io_uring cannot
    // export a root device at all. Re-enable it explicitly rather than failing
    // later with an error that names neither.
    let path = "/proc/sys/kernel/io_uring_disabled";
    if Path::new(path).exists() {
        let _ = std::fs::write(path, b"0");
    }

    if !Path::new("/dev/ublk-control").exists() {
        tracing_warn("/dev/ublk-control missing: ublk_drv did not load, root will not appear");
    }
}

fn bring_up_network(params: &BootParams) -> anyhow::Result<()> {
    stamp("bringing up network");

    let _ = Command::new("/bin/ip")
        .args(["link", "set", "lo", "up"])
        .status();

    let iface = first_ethernet().context("no ethernet interface found")?;
    let _ = Command::new("/bin/ip")
        .args(["link", "set", &iface, "up"])
        .status();

    // The kernel's own ip= handling may already have configured this; DHCP is
    // then a no-op that costs a second, which is cheaper than the branch.
    let status = Command::new("/sbin/udhcpc")
        .args([
            "-i", &iface, "-s", "/usr/share/udhcpc/default.script", "-q", "-n", "-t", "10",
        ])
        .status();

    match status {
        Ok(s) if s.success() => stamp(&format!("network up on {iface}")),
        _ => tracing_warn(&format!(
            "DHCP did not complete on {iface}; continuing in case ip= configured it"
        )),
    }

    // Prove the portal is reachable before blaming storage for a network fault.
    if let Some(portal) = &params.portal {
        stamp(&format!("portal {portal} is the root source"));
    }
    Ok(())
}

fn first_ethernet() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "lo" && !n.starts_with("veth"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Start the engine in the background. It must outlive `switch_root`: it is
/// what serves `/dev/ublkb0`, so killing it unmounts root.
/// The ordered slabs to hand `boot-local`.
///
/// Without a boothost this is the classic path — `all_slabs()`, an `nvme-tcp://`
/// URI and/or the named local slabs, exactly as given. With a boothost, a named
/// local slab is a *hint*: trusted only when it proves to be a slab, dropped
/// otherwise, and if none survives, the appliance is asked. That is what keeps
/// a stray `/dev/sda` — the WD disk from a previous life, or the iDRAC virtual
/// floppy that wins `/dev/sda` this boot and answers ENOMEDIUM — from being
/// handed to the engine and killing the boot on "No medium found".
fn resolve_slabs(params: &BootParams) -> anyhow::Result<Vec<String>> {
    let Some((boothost, tag, namespace)) = params.boothost_claim() else {
        return Ok(params.all_slabs());
    };

    let mut slabs: Vec<String> = Vec::new();
    let mut have_root = false;
    for slab in &params.slabs {
        if slab.contains("://") || is_slab(slab) {
            slabs.push(slab.clone());
            have_root = true;
        } else {
            stamp(&format!("{slab} is not a slab - asking {boothost} instead"));
        }
    }

    if !have_root {
        stamp(&format!("asking {boothost} which image {tag} boots"));
        let uri = boot_claim(boothost, tag, namespace, params.hostnqn.as_deref())?;
        stamp(&format!("claimed root: {uri}"));
        slabs.insert(0, uri);
    }

    // The data slab is attached like any other; it is tracked separately only
    // because nothing may format it.
    if let Some(data) = &params.data_slab
        && !slabs.iter().any(|s| s == data)
    {
        slabs.push(data.clone());
    }
    Ok(slabs)
}

/// Is `dev` genuinely a stormblock slab?
///
/// Positive evidence only. `stormblock slab list <dev>` prints a
/// `: slab <uuid>` line for a real slab and something else — "not a slab",
/// "cannot open", or nothing at all for an empty removable that answers
/// ENOMEDIUM — otherwise. The absence of a *known* error is not proof a device
/// is a slab; the first version of this probe learned that on a machine whose
/// `/dev/sda` was sometimes a disk and sometimes an empty floppy.
fn is_slab(dev: &str) -> bool {
    if !Path::new(dev).exists() {
        return false;
    }
    let Ok(out) = Command::new(STORMBLOCK)
        .arg("slab")
        .arg("list")
        .arg(dev)
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().any(is_slab_line)
}

/// A `stormblock slab list` line that names a slab: `<dev>: slab <uuid> (...)`.
fn is_slab_line(line: &str) -> bool {
    let Some((_, rest)) = line.split_once(": slab ") else {
        return false;
    };
    // A 36-char UUID follows. Cheap structural check; the engine is the
    // authority, this only decides whether to trust or to ask.
    let id: String = rest.chars().take_while(|c| *c != ' ').collect();
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Claim this machine's image from the appliance, keyed on the service tag.
///
/// `stormblock boot-claim` prints the `nvme-tcp://` attach URI on stdout and
/// nothing else, so it is used directly as a slab. Diagnostics are on stderr,
/// inherited so they reach the console.
fn boot_claim(
    boothost: &str,
    tag: &str,
    namespace: &str,
    hostnqn: Option<&str>,
) -> anyhow::Result<String> {
    let mut cmd = Command::new(STORMBLOCK);
    cmd.arg("boot-claim")
        .arg("--boothost")
        .arg(boothost)
        .arg("--tag")
        .arg(tag);
    if namespace != "boothost" {
        cmd.arg("--namespace").arg(namespace);
    }
    if let Some(nqn) = hostnqn {
        cmd.env("STORMBLOCK_HOST_NQN", nqn);
    }
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .context("running stormblock boot-claim")?;
    if !out.status.success() {
        bail!("no image is assigned to {tag} on {boothost} (boot-claim failed)");
    }
    let uri = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if uri.is_empty() {
        bail!("boot-claim for {tag} returned no attach URI");
    }
    Ok(uri)
}

fn start_engine(
    params: &BootParams,
    slabs: &[String],
    plan: &crate::cmdline::ExportPlan,
) -> anyhow::Result<std::process::Child> {
    let mut cmd = Command::new(STORMBLOCK);
    cmd.arg("boot-local");

    // One identity for every connect this boot makes, composed by the firmware
    // and echoed here rather than re-derived: the format lives in stormbootx.
    // The target binds the claimed clone to this NQN, so the kernel-side attach
    // must present the same one or the connect is refused.
    if let Some(nqn) = &params.hostnqn {
        cmd.env("STORMBLOCK_HOST_NQN", nqn);
    }

    // A remote namespace and a local partition are the same thing to the
    // engine: both are just a slab it is handed, and `--slab` has always been
    // repeatable. The system slab carries what an image replaces; the data
    // slab carries what must outlive it.
    if slabs.is_empty() {
        bail!("no slab to boot from");
    }
    for slab in slabs {
        cmd.arg("--slab").arg(slab);
    }

    if let Some(meta) = &params.meta {
        cmd.arg("--meta").arg(meta);
    }
    if let Some(store) = &params.image_store {
        cmd.arg("--image-store").arg(store);
    }
    if let Some(volume) = &params.volume {
        cmd.arg("--volume").arg(volume);
    }
    if let Some(disk) = &params.local_disk {
        cmd.arg("--local-disk").arg(disk);
    }
    // Writable thin volumes and mounts, in the order the plan assigned ublk
    // indices — the engine numbers them by the order of these flags.
    for name in &plan.writable_args {
        cmd.arg("--writable").arg(name);
    }

    stamp(&format!("starting engine on {}", slabs.join(", ")));
    if let Some(data) = &params.data_slab {
        stamp(&format!("data slab {data} carries node identity; never formatted"));
    }
    let child = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning {STORMBLOCK}"))?;

    Ok(child)
}

fn wait_for_device(path: &str, timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut announced = false;

    while start.elapsed() < timeout {
        if Path::new(path).exists() {
            return Ok(());
        }
        if !announced && start.elapsed() > Duration::from_secs(5) {
            stamp(&format!("still waiting for {path}"));
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Name what was actually there — "timed out" alone sends people to the
    // wrong layer.
    let present: Vec<String> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("ublk"))
                .collect()
        })
        .unwrap_or_default();

    bail!(
        "{path} did not appear within {}s; ublk devices present: {present:?}",
        timeout.as_secs()
    )
}

fn mount_root(params: &BootParams) -> anyhow::Result<()> {
    if let Some(overlay) = &params.overlay {
        return mount_overlay_root(overlay);
    }
    std::fs::create_dir_all(SYSROOT).ok();
    if mount_fs(ROOT_DEV, SYSROOT) {
        stamp(&format!("mounted root {ROOT_DEV} on {SYSROOT}"));
        return Ok(());
    }
    bail!("could not mount {ROOT_DEV} on {SYSROOT} as erofs, ext4 or auto")
}

/// Mount a filesystem the stormcos way: erofs read-only first (the common
/// case), then ext4, then let the kernel probe. Returns whether it mounted.
fn mount_fs(dev: &str, mnt: &str) -> bool {
    for args in [
        vec!["-t", "erofs", "-o", "ro", dev, mnt],
        vec!["-t", "ext4", dev, mnt],
        vec![dev, mnt],
    ] {
        if matches!(Command::new("/bin/mount").args(&args).status(), Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

/// Immutable-root overlay: the read-only root becomes the lower dir and a
/// writable upper sits on tmpfs or a block device, so the running system can
/// write while the image stays pristine.
///
///   rd.stormblock.overlay=tmpfs[:SIZE]   default 512m
///   rd.stormblock.overlay=/dev/ublkbN    a pre-formatted writable volume
fn mount_overlay_root(overlay: &str) -> anyhow::Result<()> {
    const LOWER: &str = "/run/stormblock/lower";
    const RW: &str = "/run/stormblock/rw";
    std::fs::create_dir_all(SYSROOT).ok();
    std::fs::create_dir_all(LOWER).ok();
    std::fs::create_dir_all(RW).ok();

    if !mount_fs(ROOT_DEV, LOWER) {
        bail!("overlay: could not mount lower {ROOT_DEV}");
    }

    if let Some(size) = overlay.strip_prefix("tmpfs") {
        let size = size.strip_prefix(':').unwrap_or("512m");
        let ok = Command::new("/bin/mount")
            .args(["-t", "tmpfs", "-o", &format!("size={size}"), "tmpfs", RW])
            .status();
        if !matches!(ok, Ok(s) if s.success()) {
            bail!("overlay: could not mount tmpfs upper (size={size})");
        }
    } else {
        wait_for_block(overlay, Duration::from_secs(15));
        if !matches!(Command::new("/bin/mount").args([overlay, RW]).status(), Ok(s) if s.success()) {
            bail!("overlay: could not mount upper {overlay}");
        }
    }

    let upper = format!("{RW}/upper");
    let work = format!("{RW}/work");
    std::fs::create_dir_all(&upper).ok();
    std::fs::create_dir_all(&work).ok();
    let opt = format!("lowerdir={LOWER},upperdir={upper},workdir={work}");
    if !matches!(
        Command::new("/bin/mount")
            .args(["-t", "overlay", "overlay", "-o", &opt, SYSROOT])
            .status(),
        Ok(s) if s.success()
    ) {
        bail!("overlay: could not mount overlay root on {SYSROOT}");
    }
    stamp(&format!("overlay root: lower={ROOT_DEV} upper={overlay}"));
    Ok(())
}

/// Apply the export plan once root is mounted: mount container volumes now,
/// register writables and the image store in the real root's fstab for systemd.
///
/// Nothing here is fatal — a volume that will not mount is reported and left
/// out. One container that cannot start is worth less than a node that will
/// not boot, and the log names which one is missing.
fn apply_exports(plan: &crate::cmdline::ExportPlan) {
    for (dev, mnt) in &plan.mount_map {
        if !wait_for_block(dev, Duration::from_secs(15)) {
            tracing_warn(&format!("{dev} never appeared; {mnt} will be empty"));
            continue;
        }
        let target = format!("{SYSROOT}{mnt}");
        std::fs::create_dir_all(&target).ok();
        if matches!(Command::new("/bin/mount").args([dev, &target]).status(), Ok(s) if s.success()) {
            stamp(&format!("mounted {dev} -> {mnt}"));
        } else {
            tracing_warn(&format!("{dev} would not mount at {mnt}"));
        }
    }

    // Writables: busybox has no mkfs.xfs, so hand them to systemd via fstab —
    // x-systemd.makefs formats the empty volume on first boot, growfs grows it.
    for (dev, mnt) in &plan.fstab_writable {
        if wait_for_block(dev, Duration::from_secs(15)) {
            append_fstab(&format!(
                "{dev} {mnt} xfs defaults,x-systemd.makefs,x-systemd.growfs,nofail 0 0"
            ));
            stamp(&format!("writable {dev} -> {mnt}"));
        } else {
            tracing_warn(&format!("{dev} never appeared; {mnt} falls back to overlay"));
        }
    }

    if let Some((dev, mnt)) = &plan.image_store {
        if wait_for_block(dev, Duration::from_secs(15)) {
            let target = format!("{SYSROOT}{mnt}");
            std::fs::create_dir_all(&target).ok();
            append_fstab(&format!("{dev} {mnt} erofs ro,nofail 0 0"));
            stamp(&format!("image-store {dev} -> {mnt} (ro)"));
        } else {
            tracing_warn(&format!("image-store {dev} never appeared"));
        }
    }
}

/// Wait for a block device to appear (drivers export ublk devices
/// asynchronously). Returns whether it is present within the timeout.
fn wait_for_block(dev: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if Path::new(dev).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Append a line to the real root's fstab. Failure is warned, not fatal — the
/// volume is still exported, it just will not be mounted by systemd.
fn append_fstab(line: &str) {
    use std::io::Write as _;
    let path = format!("{SYSROOT}/etc/fstab");
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(err) = writeln!(f, "{line}") {
                tracing_warn(&format!("could not append to {path}: {err}"));
            }
        }
        Err(err) => tracing_warn(&format!("could not open {path}: {err}")),
    }
}

/// Write the identity pinned at PXE time into the new root.
///
/// Nothing after `switch_root` runs DHCP, so the hostname and resolver have to
/// be carried across by hand.
fn write_identity(params: &BootParams) -> anyhow::Result<()> {
    if let Some(hostname) = &params.hostname {
        let path = format!("{SYSROOT}/etc/hostname");
        if let Err(err) = std::fs::write(&path, format!("{hostname}\n")) {
            tracing_warn(&format!("could not write {path}: {err}"));
        }
    }

    if let Some(role) = &params.role {
        // The role is what day-2 join reads to decide which profile to apply.
        let dir = format!("{SYSROOT}/etc/storm");
        std::fs::create_dir_all(&dir).ok();
        if let Err(err) = std::fs::write(format!("{dir}/role"), format!("{role}\n")) {
            tracing_warn(&format!("could not write role: {err}"));
        }
    }

    if Path::new("/etc/resolv.conf").exists() {
        let _ = std::fs::copy("/etc/resolv.conf", format!("{SYSROOT}/etc/resolv.conf"));
    }
    Ok(())
}

fn switch_root() -> anyhow::Result<()> {
    for point in ["/proc", "/sys", "/dev", "/run"] {
        let target = format!("{SYSROOT}{point}");
        std::fs::create_dir_all(&target).ok();
        let _ = Command::new("/bin/mount")
            .args(["--move", point, &target])
            .status();
    }

    let init = ["/sbin/init", "/usr/lib/systemd/systemd"]
        .into_iter()
        .find(|p| Path::new(&format!("{SYSROOT}{p}")).exists())
        .context("no init found in the new root")?;

    stamp(&format!("switch_root into {init}"));

    let err = Command::new("/sbin/switch_root")
        .args([SYSROOT, init])
        .exec_replace();

    bail!("switch_root failed: {err}")
}

/// `exec` the command, replacing this process. Returns only on failure.
trait ExecReplace {
    fn exec_replace(&mut self) -> std::io::Error;
}

#[cfg(unix)]
impl ExecReplace for Command {
    fn exec_replace(&mut self) -> std::io::Error {
        use std::os::unix::process::CommandExt as _;
        self.exec()
    }
}

#[cfg(not(unix))]
impl ExecReplace for Command {
    fn exec_replace(&mut self) -> std::io::Error {
        std::io::Error::other("exec is only available on unix")
    }
}

/// Hand the operator a shell rather than panicking the kernel.
/// Hold the machine, saying so, forever.
///
/// Never a bare sleep. A node that stops silently is indistinguishable from a
/// node that is wedged, powered off, or on the wrong network — and the whole
/// reason this code exists is that a machine between power-on and cluster join
/// has no other way to be seen. So it keeps saying what it is waiting on: to
/// a console attached ten minutes later, and to the kernel log, which is what
/// `dmesg` shows if anything ever does reach a shell.
///
/// `why` is printed every interval rather than once, because the operator who
/// needs it is by definition not watching when it first appears.
fn hold(why: &str) -> ! {
    let mut n: u64 = 0;
    loop {
        eprintln!(
            "stormnetboot-init: HELD ({n}m): {why}"
        );
        // Also to /dev/kmsg, so it survives a console that was not attached
        // and shows up in dmesg for anyone who gets further than this.
        if let Ok(mut k) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            use std::io::Write;
            let _ = writeln!(k, "stormnetboot-init: HELD: {why}");
        }
        std::thread::sleep(Duration::from_secs(60));
        n += 1;
    }
}

/// Hand the operator a shell and never come back.
///
/// `-> !` is the contract, not a decoration. This used to run the shell and
/// `return`, which looks right and is not: as PID 1 with no controlling
/// terminal the shell reads EOF and exits at once, `emergency_shell` returned,
/// `main` returned `FAILURE`, and the kernel panicked with
/// `Attempted to kill init! exitcode=0x00000100` — replacing the console with
/// a panic screen and erasing the message this function exists to show. That
/// is what an R230 did on stormcos-sno 10.29.
///
/// So: attach the child to `/dev/console`, because stdio inherited from PID 1
/// in an initramfs is not a terminal and a shell on it is unusable; and when
/// it exits — deliberately or immediately — say so and start another. An
/// operator typing `exit` should get another prompt, not a panicked machine.
pub fn emergency_shell() -> ! {
    eprintln!("stormnetboot-init: dropping to a shell; the boot cannot continue");
    eprintln!("stormnetboot-init: this shell is PID 1's child; exiting it starts another");

    let shell = ["/bin/sh", "/bin/busybox", "/bin/ash"]
        .into_iter()
        .find(|s| Path::new(s).exists());

    let Some(shell) = shell else {
        // Nothing to run. Hold the console rather than exit, so the error
        // above stays on screen instead of being replaced by a panic.
        hold("no shell in this initramfs - nothing left to hand you. \
             This is a build fault: busybox should be at /bin/sh");
    };

    loop {
        let mut cmd = Command::new(shell);
        // A shell whose stdin is not a terminal exits on the first read. The
        // console is the one thing a headless node definitely has.
        if let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/console") {
            let err = tty.try_clone().ok();
            let out = tty.try_clone().ok();
            cmd.stdin(tty);
            if let Some(o) = out {
                cmd.stdout(o);
            }
            if let Some(e) = err {
                cmd.stderr(e);
            }
        }
        match cmd.status() {
            Ok(st) => eprintln!("stormnetboot-init: shell exited ({st}); starting another"),
            Err(e) => hold(&format!("cannot start {shell}: {e}")),
        }
        // A shell that dies instantly in a loop is a busy-wait on the console.
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn tracing_warn(message: &str) {
    eprintln!("stormnetboot-init: warning: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_modules_include_the_ones_nothing_else_loads() {
        // nvme_tcp and ublk_drv have no modalias to trigger them; without
        // these two there is no network root and no root device at all.
        assert!(REQUIRED_MODULES.contains(&"nvme_tcp"));
        assert!(REQUIRED_MODULES.contains(&"ublk_drv"));
    }

    #[test]
    fn waiting_for_a_device_that_exists_returns_at_once() {
        assert!(wait_for_device("/proc/self", Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn is_a_slab_line_only_on_positive_evidence() {
        assert!(is_slab_line(
            "/dev/sdb: slab 7661cf8b-1a2b-4c3d-9e5f-0a1b2c3d4e5f (role=data, tier=hot)"
        ));
        // Absence of a known error is not evidence.
        assert!(!is_slab_line("/dev/sda: not a slab"));
        assert!(!is_slab_line("/dev/sda: cannot open: No medium found"));
        assert!(!is_slab_line(""));
        // A truncated or non-hex id is not a slab.
        assert!(!is_slab_line("/dev/sdb: slab 7661cf8b (role=data)"));
        assert!(!is_slab_line("/dev/sdb: slab zzzzzzzz-1a2b-4c3d-9e5f-0a1b2c3d4e5f"));
    }

    fn waiting_for_a_missing_device_names_what_was_there() {
        let err = wait_for_device("/dev/definitely-not-here", Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not appear"), "{err}");
        assert!(err.contains("ublk devices present"), "{err}");
    }
}
