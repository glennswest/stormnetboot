//! HTTP surface.
//!
//! Everything a booting machine touches is here, and it is all HTTP: firmware
//! that can UEFI HTTP Boot, a BMC attaching an ISO over virtual media, and
//! iPXE chainloading all pull from these routes.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Router,
    extract::{Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{config::Config, metrics::Metrics};

pub struct AppState {
    pub cfg: Config,
    pub metrics: Metrics,
}

pub type Shared = Arc<AppState>;

/// Assets a node needs before it can talk to storage. Readiness is defined as
/// being able to serve these, because a boot server that answers but cannot
/// deliver a kernel is worse than one that is plainly down.
const REQUIRED_ASSETS: [&str; 2] = ["vmlinuz", "initramfs.img"];

pub fn router(state: Shared) -> Router {
    let assets = ServeDir::new(state.cfg.asset_dir.clone());

    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/boot.ipxe", get(boot_script))
        .route("/boot.json", get(boot_listing))
        .nest_service("/boot", assets)
        .layer(middleware::from_fn_with_state(state.clone(), count_requests))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn count_requests(State(state): State<Shared>, req: Request, next: Next) -> Response {
    Metrics::incr(&state.metrics.requests_total);
    let is_asset = req.uri().path().starts_with("/boot/");
    if is_asset {
        Metrics::incr(&state.metrics.asset_requests);
    }

    let response = next.run(req).await;
    if response.status() == StatusCode::NOT_FOUND {
        Metrics::incr(&state.metrics.not_found);
    }
    response
}

async fn health() -> &'static str {
    "ok\n"
}

async fn readyz(State(state): State<Shared>) -> Response {
    let mut missing = Vec::new();
    for name in REQUIRED_ASSETS {
        if !tokio::fs::try_exists(state.cfg.asset_dir.join(name))
            .await
            .unwrap_or(false)
        {
            missing.push(name);
        }
    }

    if missing.is_empty() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("missing boot assets: {}\n", missing.join(", ")),
        )
            .into_response()
    }
}

async fn metrics(State(state): State<Shared>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct BootQuery {
    /// MAC of the booting host, as iPXE's `${net0/mac}`.
    mac: Option<String>,
}

async fn boot_script(State(state): State<Shared>, Query(q): Query<BootQuery>) -> Response {
    Metrics::incr(&state.metrics.boot_script_requests);
    let script = crate::ipxe::render(&state.cfg, q.mac.as_deref());
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
    )
        .into_response()
}

#[derive(Debug, Serialize)]
struct Listing {
    /// Asset name to size in bytes, so a human or a script can see at a glance
    /// what this server would hand out.
    assets: BTreeMap<String, u64>,
}

async fn boot_listing(State(state): State<Shared>) -> Response {
    let mut assets = BTreeMap::new();
    match tokio::fs::read_dir(&state.cfg.asset_dir).await {
        Ok(mut entries) => {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Ok(meta) = entry.metadata().await
                    && meta.is_file()
                {
                    assets.insert(entry.file_name().to_string_lossy().into_owned(), meta.len());
                }
            }
            axum::Json(Listing { assets }).into_response()
        }
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("asset directory unreadable: {err}\n"),
        )
            .into_response(),
    }
}
