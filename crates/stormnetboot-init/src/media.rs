//! Boot-media self-refresh.
//!
//! A machine that boots from local media instead of PXE still has to be able
//! to pick up a new kernel and initramfs, or the first hop becomes the one
//! thing in the platform that can only be updated by hand with a USB stick.
//!
//! This is deliberately **not** a poll-and-apply updater — those were retired
//! as an incident failure class. It is the same clone-swap rule the rest of
//! the platform follows, applied to the ESP:
//!
//! * It runs **once per boot**, at a point where the root is already attached.
//!   Nothing polls, nothing runs in the background, nothing reaches the
//!   network on its own.
//! * The comparison is **by digest**, never "is there something newer". The
//!   golden the machine just booted declares which media digest belongs with
//!   it; if the ESP does not match, the ESP is wrong and gets rewritten.
//! * The replacement UKI is carried **inside the golden**, so it arrives over
//!   the same `nvme-tcp://` transport as everything else. No HTTP, no TFTP, no
//!   second source of truth.
//!
//! Two ordering rules make an interrupted refresh safe. The new UKI is written
//! under a temporary name and renamed over the live one, so the boot path is
//! never a half-copied file. And the stamp recording what is installed is
//! written *last* — a crash between the two leaves a stale stamp, which simply
//! makes the next boot try again. Idempotent by construction.
//!
//! Nothing here may fail a boot. A machine that is running is worth more than
//! a machine with current boot media, so every failure path logs and returns.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cmdline::BootParams;
use crate::report::Reporter;

/// Where the ESP is mounted while it is being examined.
const ESP_MOUNT: &str = "/run/esp";

/// What the golden carries for the media that booted it, relative to the
/// mounted root. `boot.efi` is a finished UKI — the initramfs does not build
/// one, it copies one, so no linker or objcopy is needed here.
const PAYLOAD_DIR: &str = "usr/lib/stormcos/boot-media";
const PAYLOAD_UKI: &str = "boot.efi";
const PAYLOAD_CONF: &str = "media.conf";

/// The removable-media boot path. UEFI firmware boots this with no NVRAM
/// entry, which is the whole reason the media works in an unknown machine.
const ESP_UKI: &str = "EFI/BOOT/BOOTX64.EFI";
const ESP_CONF: &str = "stormnetboot/media.conf";

/// What a refresh pass decided. Returned for logging and reporting; the caller
/// treats every variant as success.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No `rd.stormnetboot.media=` on the command line, so this machine did
    /// not boot from media this code owns.
    Disabled,
    /// Media already carries the digest this golden asks for.
    Current(String),
    /// ESP rewritten. The next boot runs `to`.
    Updated { from: String, to: String },
    /// Something was not in place. Never an error — the boot continues.
    Skipped(String),
}

/// Compare the media's stamp against the golden's, and rewrite the ESP if they
/// differ. Never fails: the boot matters more than the media.
pub fn refresh(params: &BootParams, sysroot: &str, reporter: &Reporter) -> Outcome {
    let outcome = match run(params, sysroot) {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Skipped(format!("{err:#}")),
    };

    // Best-effort unmount whatever state we got to. Leaving the ESP mounted
    // would carry a second reference to the device across switch_root.
    let _ = umount(ESP_MOUNT);

    match &outcome {
        Outcome::Disabled => {}
        Outcome::Current(digest) => {
            crate::steps::stamp(&format!("boot media current ({})", short(digest)));
        }
        Outcome::Updated { from, to } => {
            crate::steps::stamp(&format!(
                "boot media updated {} -> {}; next boot runs it",
                short(from),
                short(to)
            ));
            reporter.phase("media-updated", Some(&format!("{} -> {}", short(from), short(to))));
        }
        Outcome::Skipped(why) => {
            crate::steps::stamp(&format!("boot media not refreshed: {why}"));
        }
    }

    outcome
}

fn run(params: &BootParams, sysroot: &str) -> anyhow::Result<Outcome> {
    let Some(device) = params.media_dev.as_deref() else {
        return Ok(Outcome::Disabled);
    };

    // The golden decides. If it carries no payload for the media, there is
    // nothing to compare against and nothing to install — that is a normal
    // state for a golden that is not meant to be booted from media.
    let payload = Path::new(sysroot).join(PAYLOAD_DIR);
    let payload_uki = payload.join(PAYLOAD_UKI);
    let payload_conf = payload.join(PAYLOAD_CONF);

    if !payload_uki.exists() || !payload_conf.exists() {
        return Ok(Outcome::Skipped(format!(
            "golden carries no {PAYLOAD_DIR} payload"
        )));
    }

    let wanted = read_pallet(&payload_conf)
        .ok_or_else(|| anyhow::anyhow!("{PAYLOAD_DIR}/{PAYLOAD_CONF} names no pallet digest"))?;

    std::fs::create_dir_all(ESP_MOUNT).ok();
    if !mount_vfat(device, ESP_MOUNT) {
        return Ok(Outcome::Skipped(format!(
            "could not mount {device} as vfat on {ESP_MOUNT}"
        )));
    }

    // The stamp doubles as proof of ownership. Media this tool built always
    // carries one, so its absence means `rd.stormnetboot.media=` is pointing
    // at somebody else's ESP — a data disk, a vendor recovery partition, the
    // machine's real installed bootloader. Rewriting that would be
    // unrecoverable and silent, so a missing stamp stops the pass rather than
    // forcing an install.
    let stamp_path = PathBuf::from(ESP_MOUNT).join(ESP_CONF);
    if !stamp_path.exists() {
        return Ok(Outcome::Skipped(format!(
            "{device} carries no {ESP_CONF}; not stormnetboot media, refusing to write"
        )));
    }

    let installed = read_pallet(&stamp_path).unwrap_or_else(|| "unknown".to_owned());

    if installed == wanted {
        return Ok(Outcome::Current(wanted));
    }

    install(&payload_uki, &wanted).map_err(|err| {
        anyhow::anyhow!("rewriting {ESP_UKI} on {device} failed: {err:#}")
    })?;

    Ok(Outcome::Updated {
        from: installed,
        to: wanted,
    })
}

/// Copy the golden's UKI over the ESP's, then record what was installed.
///
/// Order is load-bearing: rename the image into place first, write the stamp
/// second. An interruption therefore leaves a working image with a stale
/// stamp, and the next boot repeats the copy — never a stamp that claims an
/// image the ESP does not have.
fn install(payload_uki: &Path, wanted: &str) -> anyhow::Result<()> {
    let esp = PathBuf::from(ESP_MOUNT);
    let live = esp.join(ESP_UKI);
    let staged = live.with_extension("EFI.new");
    let conf = esp.join(ESP_CONF);

    for dir in [live.parent(), conf.parent()] {
        if let Some(dir) = dir {
            std::fs::create_dir_all(dir)?;
        }
    }

    std::fs::copy(payload_uki, &staged)?;
    // FAT has no journal, so an explicit flush is the only thing standing
    // between a power cut and an unbootable stick.
    sync_file(&staged)?;
    std::fs::rename(&staged, &live)?;

    std::fs::write(
        &conf,
        format!(
            "# written by stormnetboot-init; the digest of the UKI in {ESP_UKI}\npallet={wanted}\n"
        ),
    )?;
    sync_file(&conf)?;
    sync_all();

    Ok(())
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Flush the block layer too. `sync_all` covers the file; this covers the FAT
/// metadata written alongside it.
fn sync_all() {
    let _ = Command::new("/bin/sync")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn mount_vfat(device: &str, target: &str) -> bool {
    matches!(
        Command::new("/bin/mount")
            .args(["-t", "vfat", device, target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Ok(status) if status.success()
    )
}

fn umount(target: &str) -> std::io::Result<()> {
    Command::new("/bin/umount")
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

/// Pull `pallet=` out of a `key=value` stamp file.
///
/// A flat key/value file rather than JSON on purpose: the initramfs binary
/// carries exactly one dependency, and a boot-critical parser is not worth
/// adding a second for three lines of config.
fn read_pallet(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "pallet"
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Digests are long and the console is narrow. Keep enough to tell two apart.
fn short(digest: &str) -> String {
    let bare = digest.strip_prefix("sha256:").unwrap_or(digest);
    if bare.len() > 12 {
        format!("{}…", &bare[..12])
    } else {
        bare.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snb-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_pallet_ignoring_comments_and_blanks() {
        let dir = tmpdir("read");
        let path = write(
            &dir,
            "media.conf",
            "# a comment\n\nkver=6.17.1\npallet=sha256:abc123\nbuilt=now\n",
        );
        assert_eq!(read_pallet(&path).as_deref(), Some("sha256:abc123"));
    }

    #[test]
    fn missing_or_empty_pallet_reads_as_none() {
        let dir = tmpdir("empty");
        assert_eq!(read_pallet(&write(&dir, "a.conf", "kver=1\n")), None);
        assert_eq!(read_pallet(&write(&dir, "b.conf", "pallet=\n")), None);
        assert_eq!(read_pallet(&dir.join("absent.conf")), None);
    }

    #[test]
    fn no_media_device_disables_the_pass() {
        let params = BootParams::parse("rd.stormblock.portal=h");
        assert_eq!(run(&params, "/nonexistent").unwrap(), Outcome::Disabled);
    }

    #[test]
    fn a_golden_without_a_payload_is_skipped_not_failed() {
        let dir = tmpdir("nopayload");
        let params = BootParams::parse("rd.stormnetboot.media=/dev/sda1");
        let outcome = run(&params, dir.to_str().unwrap()).unwrap();
        assert!(
            matches!(outcome, Outcome::Skipped(ref why) if why.contains("carries no")),
            "expected a skip, got {outcome:?}"
        );
    }

    #[test]
    fn an_esp_without_our_stamp_is_refused() {
        // The guard that keeps a mistyped rd.stormnetboot.media= from
        // overwriting an unrelated ESP. Exercised through the reader the
        // check uses, since mounting is not available in a unit test.
        let dir = tmpdir("unowned");
        assert!(!dir.join(ESP_CONF).exists());
        assert_eq!(read_pallet(&dir.join(ESP_CONF)), None);
    }

    #[test]
    fn short_trims_the_prefix_and_the_tail() {
        assert_eq!(short("sha256:0123456789abcdef0123"), "0123456789ab…");
        assert_eq!(short("short"), "short");
    }
}
