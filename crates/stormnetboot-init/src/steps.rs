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
const REQUIRED_MODULES: [&str; 6] = ["nvme_tcp", "ublk_drv", "erofs", "overlay", "ext4", "xfs"];

const DEVICE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn read_cmdline() -> std::io::Result<String> {
    std::fs::read_to_string("/proc/cmdline")
}

pub fn run(params: &BootParams, reporter: &Reporter) -> anyhow::Result<()> {
    stamp("stormnetboot-init starting");

    mount_pseudo_filesystems()?;
    load_modules();

    if params.is_network_boot() {
        // Root is on the other side of a NIC, so the NIC has to work first.
        // A local slab boot deliberately skips this: no network, no wait.
        bring_up_network(params)?;
    } else if params.slab.is_none() {
        let missing = params.missing().join(", ");
        bail!(
            "no root source on the command line: missing {missing} \
             (and no rd.stormblock.slab for a local boot)"
        );
    }

    reporter.phase("assets-fetched", None);

    let mut engine = start_engine(params).context("starting stormblock")?;
    wait_for_device(ROOT_DEV, DEVICE_TIMEOUT).inspect_err(|_| {
        // The engine's own output is the only diagnosis available here.
        let _ = engine.kill();
    })?;

    reporter.phase("root-attached", None);
    stamp("root device present");

    mount_root()?;
    write_identity(params)?;

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
fn stamp(message: &str) {
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
fn start_engine(params: &BootParams) -> anyhow::Result<std::process::Child> {
    let mut cmd = Command::new(STORMBLOCK);
    cmd.arg("boot-local");

    // A remote namespace and a local partition are the same thing to the
    // engine: both are just a slab it is handed.
    let slab = match (params.nvme_uri(), params.slab.as_deref()) {
        (Some(uri), _) => uri,
        (None, Some(slab)) => slab.to_owned(),
        (None, None) => bail!("no slab to boot from"),
    };
    cmd.arg("--slab").arg(&slab);

    if let Some(volume) = &params.volume {
        cmd.arg("--volume").arg(volume);
    }
    if let Some(disk) = &params.local_disk {
        cmd.arg("--local-disk").arg(disk);
    }

    stamp(&format!("starting engine on {slab}"));
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
    fn waiting_for_a_missing_device_names_what_was_there() {
        let err = wait_for_device("/dev/definitely-not-here", Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not appear"), "{err}");
        assert!(err.contains("ublk devices present"), "{err}");
    }
}
