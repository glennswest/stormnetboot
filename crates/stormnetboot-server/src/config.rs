//! Server configuration.
//!
//! Everything is settable by flag or environment variable so the same binary
//! runs from a shell, a boot.d spec, and a container without a config file.

use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

use crate::hosts::UnknownHostPolicy;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "stormnetboot-server",
    version,
    about = "Serves the network boot chain over HTTP from signed pallets"
)]
pub struct Config {
    /// Firmware-facing address: boot scripts, kernel, initramfs, ISOs.
    ///
    /// This surface is reachable by every machine that can PXE, so it carries
    /// only what a booting machine needs.
    #[arg(long, env = "STORMNETBOOT_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Management address: console feed, metrics, host administration.
    ///
    /// Split from the boot surface deliberately — the two sit on different
    /// networks in the intended deployment, and management should not be
    /// exposed to anything that merely knows how to PXE.
    #[arg(long, env = "STORMNETBOOT_MGMT_LISTEN", default_value = "0.0.0.0:9096")]
    pub mgmt_listen: SocketAddr,

    /// Directory the boot assets are served from.
    ///
    /// With `--registry` set this is a cache the boot pallet is projected
    /// into; without it, whatever is in this directory is served as-is.
    #[arg(
        long,
        env = "STORMNETBOOT_ASSET_DIR",
        default_value = "/var/lib/stormnetboot/assets"
    )]
    pub asset_dir: PathBuf,

    /// Base URL clients use to reach this server.
    ///
    /// Baked into rendered boot scripts, so it must be the service name
    /// clients resolve, never this instance's own address.
    #[arg(
        long,
        env = "STORMNETBOOT_BASE_URL",
        default_value = "http://boot.storm.lo:8080"
    )]
    pub base_url: String,

    // ---- pallet projection -------------------------------------------------
    /// sbregistry base URL, e.g. `http://registry:5100`.
    ///
    /// When unset the server serves whatever is already in `--asset-dir`,
    /// which is how a bootstrap or an air-gapped site runs.
    #[arg(long, env = "STORMNETBOOT_REGISTRY")]
    pub registry: Option<String>,

    /// Repository holding the boot pallet.
    #[arg(long, env = "STORMNETBOOT_PALLET_REPO", default_value = "stormcos/boot")]
    pub pallet_repo: String,

    /// Tag or digest of the boot pallet to serve.
    #[arg(long, env = "STORMNETBOOT_PALLET_REF", default_value = "latest")]
    pub pallet_ref: String,

    /// How often to re-check the registry for a new boot pallet digest.
    #[arg(
        long,
        env = "STORMNETBOOT_REFRESH_SECS",
        default_value_t = 60,
        value_parser = clap::value_parser!(u64).range(5..)
    )]
    pub refresh_secs: u64,

    /// Ed25519 public key (or key id) trusted to sign the boot pallet.
    /// Repeatable.
    #[arg(long = "trusted-key", env = "STORMNETBOOT_TRUSTED_KEYS", value_delimiter = ',')]
    pub trusted_keys: Vec<String>,

    /// Serve a boot pallet even when no trusted signature covers it.
    ///
    /// Off by default: what this server hands out is executed as the kernel of
    /// every machine that asks.
    #[arg(long, env = "STORMNETBOOT_ALLOW_UNSIGNED", default_value_t = false)]
    pub allow_unsigned: bool,

    // ---- per-host claims ---------------------------------------------------
    /// Golden every booting host clones its root from.
    #[arg(long, env = "STORMNETBOOT_GOLDEN")]
    pub golden: Option<String>,

    /// Claim a per-host CoW clone from sbregistry when rendering a boot script.
    ///
    /// Requires `--registry` and `--golden`.
    #[arg(long, env = "STORMNETBOOT_CLAIM", default_value_t = false)]
    pub claim: bool,

    // ---- host identity -----------------------------------------------------
    /// JSON file of host records (an array of `{mac, name, role, ...}`).
    #[arg(long, env = "STORMNETBOOT_HOSTS_FILE")]
    pub hosts_file: Option<PathBuf>,

    /// What to do when a machine with no record asks to boot.
    #[arg(long, env = "STORMNETBOOT_UNKNOWN_HOSTS", default_value = "allow")]
    pub unknown_hosts: UnknownHostPolicy,

    // ---- fallback boot target ---------------------------------------------
    /// NVMe/TCP portal used when no per-host claim is made.
    #[arg(long, env = "STORMNETBOOT_PORTAL")]
    pub portal: Option<String>,

    /// Port for that portal.
    #[arg(long, env = "STORMNETBOOT_PORTAL_PORT", default_value_t = 4420)]
    pub portal_port: u16,

    /// Extra kernel command line appended to every rendered boot script.
    #[arg(long, env = "STORMNETBOOT_EXTRA_CMDLINE", default_value = "")]
    pub extra_cmdline: String,

    /// Local disk the booted node assimilates onto (zeroboot flow-over).
    ///
    /// Passed through as `rd.stormblock.local-disk`. Destructive to that
    /// device on the target, which is why it is explicit rather than guessed.
    #[arg(long, env = "STORMNETBOOT_LOCAL_DISK")]
    pub local_disk: Option<String>,
}

impl Config {
    /// Base URL without a trailing slash, safe to concatenate paths onto.
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Whether pallet projection is configured.
    pub fn projects_pallets(&self) -> bool {
        self.registry.is_some()
    }

    /// Whether per-host claims can actually be made.
    pub fn claims_enabled(&self) -> bool {
        self.claim && self.registry.is_some() && self.golden.is_some()
    }

    /// Reject combinations that would fail confusingly at run time.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.claim && self.registry.is_none() {
            anyhow::bail!("--claim needs --registry to claim clones from");
        }
        if self.claim && self.golden.is_none() {
            anyhow::bail!("--claim needs --golden to say what to clone");
        }
        if self.projects_pallets() && self.trusted_keys.is_empty() && !self.allow_unsigned {
            anyhow::bail!(
                "no --trusted-key configured: pass one, or --allow-unsigned to serve \
                 a boot pallet nobody has vouched for"
            );
        }
        if self.listen == self.mgmt_listen {
            anyhow::bail!(
                "--listen and --mgmt-listen are both {}; the boot surface and the \
                 management surface must not share a socket",
                self.listen
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(args: &[&str]) -> Config {
        let mut argv = vec!["stormnetboot-server"];
        argv.extend_from_slice(args);
        Config::parse_from(argv)
    }

    #[test]
    fn defaults_are_a_directory_server_with_no_claims() {
        let c = cfg(&[]);
        assert!(!c.projects_pallets());
        assert!(!c.claims_enabled());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn claiming_requires_a_registry_and_a_golden() {
        assert!(cfg(&["--claim"]).validate().is_err());
        assert!(
            cfg(&["--claim", "--registry", "http://r:5100"])
                .validate()
                .is_err()
        );
        let c = cfg(&[
            "--claim",
            "--registry",
            "http://r:5100",
            "--golden",
            "stormcos",
            "--trusted-key",
            "ab",
        ]);
        assert!(c.validate().is_ok());
        assert!(c.claims_enabled());
    }

    #[test]
    fn projecting_pallets_demands_a_key_or_an_explicit_opt_out() {
        let c = cfg(&["--registry", "http://r:5100"]);
        assert!(c.validate().is_err(), "unsigned should not be the default");

        assert!(
            cfg(&["--registry", "http://r:5100", "--allow-unsigned"])
                .validate()
                .is_ok()
        );
        assert!(
            cfg(&["--registry", "http://r:5100", "--trusted-key", "deadbeef"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn the_two_surfaces_may_not_share_a_socket() {
        let c = cfg(&["--listen", "0.0.0.0:8080", "--mgmt-listen", "0.0.0.0:8080"]);
        assert!(c.validate().is_err());
    }

    #[test]
    fn trusted_keys_accept_a_comma_separated_list() {
        let c = cfg(&["--trusted-key", "aa,bb,cc"]);
        assert_eq!(c.trusted_keys, vec!["aa", "bb", "cc"]);
    }

    #[test]
    fn refresh_interval_has_a_floor() {
        // A one-second poll against the registry from every boot server in a
        // fleet is a denial of service with extra steps.
        let parsed = Config::try_parse_from(["stormnetboot-server", "--refresh-secs", "1"]);
        assert!(parsed.is_err());
    }
}
