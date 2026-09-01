//! Server configuration.
//!
//! Everything is settable by flag or environment variable so the same binary
//! runs from a shell, a boot.d spec, and a container without a config file.

use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "stormnetboot-server",
    version,
    about = "Serves the network boot chain over HTTP"
)]
pub struct Config {
    /// Address to serve HTTP on.
    #[arg(long, env = "STORMNETBOOT_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Directory holding the boot assets served under `/boot`.
    ///
    /// Phase 1 reads these from disk. Phase 2 projects them out of the active
    /// signed boot pallet instead, and this becomes the local cache.
    #[arg(
        long,
        env = "STORMNETBOOT_ASSET_DIR",
        default_value = "/var/lib/stormnetboot/assets"
    )]
    pub asset_dir: PathBuf,

    /// Base URL clients use to reach this server.
    ///
    /// This is what gets baked into the rendered iPXE script, so it must be
    /// the service name clients resolve, never this pod's own address.
    #[arg(
        long,
        env = "STORMNETBOOT_BASE_URL",
        default_value = "http://boot.storm.lo:8080"
    )]
    pub base_url: String,

    /// NVMe/TCP portal (host or IP) the booted node attaches its root from.
    ///
    /// Until per-host claims land in phase 3, this is a single fleet-wide
    /// value; when unset the rendered cmdline carries no portal and the node
    /// will stop in the initramfs rather than boot something wrong.
    #[arg(long, env = "STORMNETBOOT_PORTAL")]
    pub portal: Option<String>,

    /// NVMe/TCP portal port.
    #[arg(long, env = "STORMNETBOOT_PORTAL_PORT", default_value_t = 4420)]
    pub portal_port: u16,

    /// Extra kernel command line appended to every rendered boot script.
    #[arg(long, env = "STORMNETBOOT_EXTRA_CMDLINE", default_value = "")]
    pub extra_cmdline: String,
}

impl Config {
    /// Base URL without a trailing slash, safe to concatenate paths onto.
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}
