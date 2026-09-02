//! Where to attach, and the volume this binary booted from.
//!
//! Nothing about a portal belongs in a compiled binary. Over one afternoon the
//! target here moved host (dev.g8.lo → forge.g16.lo) and then changed NQN
//! (`…lo.g8:stormcos` → `…lo.g16:stormcos`), and each time a compiled-in value
//! meant a machine that could not boot until someone rebuilt and rewrote a
//! stick. Configuration lives next to the binary instead, so the fix is a text
//! edit on the media.
//!
//! The volume is found through `EFI_LOADED_IMAGE_PROTOCOL`, which hands back
//! the device this image was loaded from. That is exact — no probing for
//! "something that looks like our ESP", no risk of writing to a partition that
//! belongs to somebody else, and it is the same handle the self-update path
//! needs later.
//!
//! Resolution order, first hit wins:
//!
//!   1. `\stormboot\stormboot.conf` on the volume we booted from
//!   2. compiled-in defaults
//!
//! DNS-based discovery belongs above (2) and is not here yet; see the module
//! note in `main.rs`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use uefi::boot::{self, ScopedProtocol};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileAttribute, FileMode, FileType, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CStr16, Handle};

/// Path on the ESP holding both the config and the update stamp.
pub const CONF_PATH: &str = r"\stormboot\stormboot.conf";

/// What the extension needs in order to attach.
#[derive(Debug, Clone)]
pub struct Config {
    pub portal: [u8; 4],
    pub port: u16,
    pub nqn: String,
    pub nsid: u32,
    /// Digest of the `BOOTX64.EFI` currently on this media, if it has been
    /// stamped. Absent on a stick written by `dd` and never updated.
    pub stamp: Option<String>,
    /// Where the config came from, for the console line.
    pub source: &'static str,
}

pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.trim().split('.');
    for slot in out.iter_mut() {
        *slot = parts.next()?.trim().parse::<u8>().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The handle of the volume this image was loaded from.
///
/// Public because the self-update path writes back to exactly this volume and
/// must not go looking for it a second time by a different route.
pub fn boot_volume() -> Option<Handle> {
    let li = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?;
    li.device()
}

fn open_fs(handle: Handle) -> Option<ScopedProtocol<SimpleFileSystem>> {
    boot::open_protocol_exclusive::<SimpleFileSystem>(handle).ok()
}

/// Read a file from the boot volume as text.
pub fn read_file(path: &str) -> Option<String> {
    let handle = boot_volume()?;
    let mut fs = open_fs(handle)?;
    let mut root = fs.open_volume().ok()?;

    let mut buf = [0u16; 256];
    let path16 = CStr16::from_str_with_buf(path, &mut buf).ok()?;
    let file = root
        .open(path16, FileMode::Read, FileAttribute::empty())
        .ok()?;
    let mut file: RegularFile = match file.into_type().ok()? {
        FileType::Regular(f) => f,
        FileType::Dir(_) => return None,
    };

    // Config files here are a few hundred bytes; refuse anything that is not,
    // rather than allocating whatever a corrupt directory entry claims.
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = file.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() > 64 * 1024 {
            return None;
        }
    }
    String::from_utf8(out).ok()
}

/// Pull `key = value` out of a flat config file, ignoring comments.
fn field(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Not a let-chain: this crate is edition 2021, matching stormuefi.
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Resolve where to attach.
///
/// Never fails: a stick with no config still boots against the compiled
/// defaults, which is what makes a blank `dd`-written stick useful before
/// anyone has edited anything onto it.
pub fn resolve(
    default_portal: [u8; 4],
    default_port: u16,
    default_nqn: &str,
    default_nsid: u32,
) -> Config {
    let mut cfg = Config {
        portal: default_portal,
        port: default_port,
        nqn: default_nqn.to_string(),
        nsid: default_nsid,
        stamp: None,
        source: "compiled defaults",
    };

    let Some(text) = read_file(CONF_PATH) else {
        return cfg;
    };

    // A config that exists but is unreadable in part should still contribute
    // what it does carry: a typo in `nsid` must not silently move the portal
    // back to a compiled value the operator thought they had replaced.
    if let Some(p) = field(&text, "portal").and_then(|s| parse_ipv4(&s)) {
        cfg.portal = p;
    }
    if let Some(p) = field(&text, "port").and_then(|s| s.parse().ok()) {
        cfg.port = p;
    }
    if let Some(n) = field(&text, "nqn") {
        cfg.nqn = n;
    }
    if let Some(n) = field(&text, "nsid").and_then(|s| s.parse().ok()) {
        cfg.nsid = n;
    }
    cfg.stamp = field(&text, "stamp");
    cfg.source = CONF_PATH;
    cfg
}

/// Render a config file, preserving the attach settings and recording a new
/// stamp. Used by the self-update path after it replaces `BOOTX64.EFI`.
pub fn render(cfg: &Config, stamp: &str) -> String {
    let [a, b, c, d] = cfg.portal;
    format!(
        "# stormbootx — edit this rather than rebuilding the binary.\n\
         # Written back by the self-update path; the stamp is the digest of\n\
         # the BOOTX64.EFI currently on this media.\n\
         portal = {a}.{b}.{c}.{d}\n\
         port   = {}\n\
         nqn    = {}\n\
         nsid   = {}\n\
         stamp  = {stamp}\n",
        cfg.port, cfg.nqn, cfg.nsid
    )
}

/// Write a file to the boot volume, replacing what is there.
pub fn write_file(path: &str, body: &[u8]) -> Result<(), String> {
    let handle = boot_volume().ok_or("no boot volume (LoadedImage has no device)")?;
    let mut fs = open_fs(handle).ok_or("boot volume has no SimpleFileSystem")?;
    let mut root = fs.open_volume().map_err(|e| format!("open_volume: {e:?}"))?;

    // Create the directory if this is the first write to a stick that was
    // only ever dd'd.
    let mut dbuf = [0u16; 64];
    if let Ok(dir16) = CStr16::from_str_with_buf(r"\stormboot", &mut dbuf) {
        let _ = root.open(dir16, FileMode::CreateReadWrite, FileAttribute::DIRECTORY);
    }

    let mut buf = [0u16; 256];
    let path16 = CStr16::from_str_with_buf(path, &mut buf).map_err(|_| "path too long")?;
    let file = root
        .open(path16, FileMode::CreateReadWrite, FileAttribute::empty())
        .map_err(|e| format!("open for write: {e:?}"))?;
    let mut file: RegularFile = match file.into_type().map_err(|e| format!("{e:?}"))? {
        FileType::Regular(f) => f,
        FileType::Dir(_) => return Err("path is a directory".into()),
    };

    // Truncate: an update that shrinks the file must not leave the tail of the
    // previous one behind, which would still parse and could still be valid.
    file.set_position(0).ok();
    file.write(body).map_err(|e| format!("write: {e:?}"))?;
    let end = body.len() as u64;
    let _ = file.set_position(end);
    file.flush().map_err(|e| format!("flush: {e:?}"))?;
    Ok(())
}

/// Unused today; kept because the update path needs a device path to name the
/// volume in a message an operator can act on.
#[allow(dead_code)]
pub fn boot_volume_path() -> Option<ScopedProtocol<DevicePath>> {
    boot::open_protocol_exclusive::<DevicePath>(boot_volume()?).ok()
}
