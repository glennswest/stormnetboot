//! stormnetboot-server — the boot asset service.
//!
//! Phase 1 serves assets from a directory. Phase 2 replaces that with
//! projection from the active signed boot pallet, and phase 3 renders the
//! command line per host from a claimed CoW clone.

mod config;
mod http;
mod ipxe;
mod metrics;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser as _;
use tracing_subscriber::EnvFilter;

use crate::{config::Config, http::AppState, metrics::Metrics};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %cfg.listen,
        asset_dir = %cfg.asset_dir.display(),
        base_url = cfg.base_url(),
        portal = cfg.portal.as_deref().unwrap_or("<unset>"),
        "starting stormnetboot-server"
    );

    if cfg.portal.is_none() {
        tracing::warn!(
            "no NVMe/TCP portal configured; rendered boot scripts will not attach a root volume"
        );
    }
    if !tokio::fs::try_exists(&cfg.asset_dir).await.unwrap_or(false) {
        tracing::warn!(
            asset_dir = %cfg.asset_dir.display(),
            "asset directory does not exist yet; /readyz will fail until it does"
        );
    }

    let listen = cfg.listen;
    let state = Arc::new(AppState {
        cfg,
        metrics: Metrics::default(),
    });

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving HTTP")?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Shut down on SIGTERM as well as Ctrl-C: this runs as a container under
/// stormpump, where SIGTERM is how it is asked to stop.
async fn shutdown() {
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
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
