//! stormnetboot-server — the boot asset service.
//!
//! Serves the boot chain over HTTP: a per-host iPXE script, the kernel and
//! initramfs projected out of the active signed boot pallet, and an ISO for
//! firmware that boots through BMC virtual media. Machines report their own
//! progress back, so the fleet's state between power-on and assimilation is
//! visible while it happens.

mod claims;
mod components;
mod config;
mod hosts;
mod http;
mod ipxe;
mod mac;
mod metrics;
mod pallet;
mod state;
mod stormsig;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser as _;
use tracing_subscriber::EnvFilter;

use crate::{
    claims::ClaimClient,
    config::Config,
    hosts::HostStore,
    http::{AppState, Shared},
    metrics::Metrics,
    pallet::{AssetStatus, PalletSource},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    cfg.validate()?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        boot = %cfg.listen,
        mgmt = %cfg.mgmt_listen,
        asset_dir = %cfg.asset_dir.display(),
        base_url = cfg.base_url(),
        registry = cfg.registry.as_deref().unwrap_or("<none>"),
        claims = cfg.claims_enabled(),
        "starting stormnetboot-server"
    );

    if cfg.allow_unsigned {
        tracing::warn!(
            "--allow-unsigned is set: this server will hand machines a kernel that \
             nobody has vouched for"
        );
    }

    let hosts = match &cfg.hosts_file {
        Some(path) => HostStore::from_file(path.clone()),
        None => HostStore::empty(),
    };

    let claim_client = if cfg.claims_enabled() {
        Some(ClaimClient::new(
            cfg.registry.clone().expect("validated"),
            cfg.golden.clone().expect("validated"),
        )?)
    } else {
        None
    };

    // Establish what we can serve before opening the door, so a machine that
    // arrives in the first second gets the same answer as one that arrives a
    // minute later.
    let pallet_source = match &cfg.registry {
        Some(registry) => Some(PalletSource::new(
            registry.clone(),
            cfg.pallet_repo.clone(),
            cfg.pallet_ref.clone(),
            cfg.asset_dir.clone(),
            cfg.trusted_keys.clone(),
            !cfg.allow_unsigned,
        )?),
        None => None,
    };

    let initial_status = initial_status(&cfg, pallet_source.as_ref()).await;
    let state: Shared = Arc::new(AppState::new(cfg, hosts, claim_client, initial_status));

    if let Some(source) = pallet_source {
        spawn_refresh_loop(state.clone(), source);
    }

    let boot_listener = tokio::net::TcpListener::bind(state.cfg.listen)
        .await
        .with_context(|| format!("binding boot surface {}", state.cfg.listen))?;
    let mgmt_listener = tokio::net::TcpListener::bind(state.cfg.mgmt_listen)
        .await
        .with_context(|| format!("binding management surface {}", state.cfg.mgmt_listen))?;

    tracing::info!(
        boot = %boot_listener.local_addr()?,
        mgmt = %mgmt_listener.local_addr()?,
        "listening"
    );

    let boot = axum::serve(boot_listener, http::boot_router(state.clone()))
        .with_graceful_shutdown(shutdown("boot"));
    let mgmt = axum::serve(mgmt_listener, http::mgmt_router(state.clone()))
        .with_graceful_shutdown(shutdown("management"));

    // Either surface failing is fatal: serving boots without observability, or
    // reporting health for a boot surface that is down, are both worse than
    // exiting and being restarted.
    tokio::try_join!(
        async { boot.await.context("serving the boot surface") },
        async { mgmt.await.context("serving the management surface") },
    )?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// What the server can serve at startup.
async fn initial_status(cfg: &Config, source: Option<&PalletSource>) -> AssetStatus {
    if let Some(source) = source {
        match source.refresh(None).await {
            Ok(refreshed) => return refreshed.status,
            Err(err) => {
                // Do not exit: a cache from a previous run may still be
                // serviceable, and a registry that is briefly down should not
                // take the boot tier with it.
                tracing::error!(%err, "could not fetch the boot pallet at startup");
            }
        }
    }

    if pallet::cache_is_complete(&cfg.asset_dir).await {
        AssetStatus {
            ready: true,
            detail: format!("directory {}", cfg.asset_dir.display()),
            version: None,
            digest: None,
            signature_verified: false,
        }
    } else {
        AssetStatus {
            ready: false,
            detail: format!("no kernel or initramfs in {}", cfg.asset_dir.display()),
            version: None,
            digest: None,
            signature_verified: false,
        }
    }
}

/// Re-check the registry for a new boot pallet digest.
///
/// A rollout is a digest change: this is how a new kernel reaches machines
/// that have not booted yet, without restarting the server.
fn spawn_refresh_loop(state: Shared, source: PalletSource) {
    let interval = std::time::Duration::from_secs(state.cfg.refresh_secs);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;

            let current = state.asset_status().digest;
            match source.refresh(current.as_deref()).await {
                Ok(refreshed) => {
                    if refreshed.changed {
                        Metrics::incr(&state.metrics.pallet_refreshes);
                        tracing::info!(
                            digest = refreshed.status.digest.as_deref().unwrap_or(""),
                            "boot pallet updated"
                        );
                    }
                    state.set_asset_status(refreshed.status);
                }
                Err(err) => {
                    Metrics::incr(&state.metrics.pallet_refresh_failures);
                    // Keep serving what we have. A failed refresh must never
                    // take a working boot tier down.
                    tracing::error!(%err, "boot pallet refresh failed; continuing to serve the current one");
                }
            }
        }
    });
}

/// Shut down on SIGTERM as well as Ctrl-C: this runs as a container under
/// stormpump, where SIGTERM is how it is asked to stop.
async fn shutdown(which: &'static str) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => tracing::error!(%err, "cannot listen for SIGTERM"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!(surface = which, "received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!(surface = which, "received SIGTERM, shutting down"),
    }
}
