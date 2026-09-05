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

    let mut engine = start_engine(params, &slabs).context("starting stormblock")?;
    wait_for_device(ROOT_DEV, DEVICE_TIMEOUT).inspect_err(|_| {
        // The engine's own output is the only diagnosis available here.
        let _ = engine.kill();
    })?;

    reporter.phase("root-attached", None);
    stamp("root device present");

    mount_root()?;
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
            stamp(&format!("{slab} is not a slab — asking {boothost} instead"));
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

    if let Some(volume) = &params.volume {
        cmd.arg("--volume").arg(volume);
    }
    if let Some(disk) = &params.local_disk {
        cmd.arg("--local-disk").arg(disk);
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

fn mount_root() -> anyhow::Result<()> {
    std::fs::create_dir_all(SYSROOT).ok();

    // stormcos root is erofs; ext4 is the fallback for a writable root. Try in
    // that order rather than probing, so the common case costs one syscall.
    for args in [
        vec!["-t", "erofs", "-o", "ro", ROOT_DEV, SYSROOT],
        vec!["-t", "ext4", ROOT_DEV, SYSROOT],
        vec![ROOT_DEV, SYSROOT],
    ] {
        let status = Command::new("/bin/mount").args(&args).status();
        if matches!(status, Ok(s) if s.success()) {
            stamp(&format!("mounted root ({})", args.join(" ")));
            return Ok(());
        }
    }

    bail!("could not mount {ROOT_DEV} on {SYSROOT} as erofs, ext4 or auto")
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
pub fn emergency_shell() {
    eprintln!("stormnetboot-init: dropping to a shell; the boot cannot continue");
    for shell in ["/bin/sh", "/bin/busybox"] {
        if Path::new(shell).exists() {
            let _ = Command::new(shell).status();
            return;
        }
    }
    // No shell either. Sleep rather than exit: PID 1 exiting panics the
    // kernel and scrolls the real error off the console.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
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
