//! HTTP surfaces.
//!
//! Two of them, deliberately. The **boot** surface is what firmware touches —
//! UEFI HTTP Boot, a BMC attaching an ISO over virtual media, or iPXE
//! chainloading — and carries only what a booting machine needs. The
//! **management** surface carries the console feed, metrics and host
//! administration, and belongs on the other side of the network split.

use std::{collections::HashMap, sync::Arc, sync::RwLock};

use axum::{
    Router,
    extract::{Query, Request, State, ws::WebSocketUpgrade},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{
    claims::{ClaimClient, ClaimResponse},
    components,
    config::Config,
    hosts::{HostStore, UnknownHostPolicy},
    ipxe::{self, BootPlan},
    mac::Mac,
    metrics::Metrics,
    pallet::AssetStatus,
    state::{BootState, Inventory, Phase},
};

pub struct AppState {
    pub cfg: Config,
    pub metrics: Metrics,
    pub hosts: HostStore,
    pub boot: BootState,
    pub assets: RwLock<AssetStatus>,
    pub claim_client: Option<ClaimClient>,
    /// Claims already made, by host.
    ///
    /// Firmware retries — a machine that times out fetching a kernel comes
    /// back and asks for its script again. Without this, every retry would
    /// mint another clone and leak it.
    claims: tokio::sync::Mutex<HashMap<Mac, ClaimResponse>>,
}

impl AppState {
    pub fn new(
        cfg: Config,
        hosts: HostStore,
        claim_client: Option<ClaimClient>,
        assets: AssetStatus,
    ) -> Self {
        Self {
            cfg,
            metrics: Metrics::default(),
            hosts,
            boot: BootState::default(),
            assets: RwLock::new(assets),
            claim_client,
            claims: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn asset_status(&self) -> AssetStatus {
        self.assets
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_asset_status(&self, status: AssetStatus) {
        *self.assets.write().unwrap_or_else(|p| p.into_inner()) = status;
    }

    /// The clone this host is booting from, for the record's status.
    pub async fn claim_id(&self, mac: &Mac) -> Option<String> {
        self.claims.lock().await.get(mac).map(|c| c.id.clone())
    }
}

pub type Shared = Arc<AppState>;

/// Assets a machine cannot boot without.
const REQUIRED_ASSETS: [&str; 2] = ["vmlinuz", "initramfs.img"];

/// The firmware-facing surface.
pub fn boot_router(state: Shared) -> Router {
    let assets = ServeDir::new(state.cfg.asset_dir.clone());

    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readyz))
        .route("/boot.ipxe", get(boot_script))
        .route("/boot.json", get(boot_listing))
        // Booting nodes report their own progress here; they are on this
        // network, not the management one.
        .route("/api/v1/report", post(report))
        // And what it is made of. This is the whole of "inspection": a
        // running machine reading its own /sys and /proc, not a second boot
        // into an agent ramdisk.
        .route("/api/v1/inventory", post(inventory))
        .nest_service("/boot", assets)
        .layer(middleware::from_fn_with_state(state.clone(), count_requests))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The management surface.
pub fn mgmt_router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/api/v1/components", get(components_feed))
        .route("/ws/components", get(ws_components))
        .route("/api/v1/state", get(state_dump))
        .route("/api/v1/hosts", get(hosts_list))
        .route("/api/v1/hosts/reload", post(hosts_reload))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn count_requests(State(state): State<Shared>, req: Request, next: Next) -> Response {
    Metrics::incr(&state.metrics.requests_total);
    if req.uri().path().starts_with("/boot/") {
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
    let phases = state.boot.phase_counts();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(&phases, state.hosts.counts()),
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

    let mac = match q.mac.as_deref().map(Mac::parse) {
        Some(Ok(mac)) => Some(mac),
        Some(Err(err)) => {
            // A MAC we cannot parse is a client bug or a probe; refuse rather
            // than silently serving a default boot to an unidentified machine.
            Metrics::incr(&state.metrics.refused);
            return (
                StatusCode::BAD_REQUEST,
                format!("#!ipxe\n# malformed mac: {err}\nshell\n"),
            )
                .into_response();
        }
        None => None,
    };

    let record = mac.as_ref().and_then(|m| state.hosts.lookup(m));

    if record.is_none()
        && state.cfg.unknown_hosts == UnknownHostPolicy::Deny
    {
        Metrics::incr(&state.metrics.refused);
        tracing::warn!(
            mac = mac.as_ref().map(|m| m.to_string()).unwrap_or_default(),
            "refusing unknown host"
        );
        return (
            StatusCode::FORBIDDEN,
            "#!ipxe\n# this machine has no host record; refusing to boot it\nshell\n".to_string(),
        )
            .into_response();
    }

    // A parked machine keeps its identity and does not boot. Refusing here
    // rather than deleting the record is what lets a host come back from
    // repair as itself.
    if let Some(record) = &record
        && !record.online
    {
        Metrics::incr(&state.metrics.refused);
        tracing::warn!(host = %record.name, "refusing host marked offline");
        return (
            StatusCode::FORBIDDEN,
            format!(
                "#!ipxe\n# {} is marked offline (spec.online=false); not booting it\nshell\n",
                record.name
            ),
        )
            .into_response();
    }

    if let Some(mac) = &mac {
        state.boot.observe(mac.clone(), Phase::ScriptFetched, None);
    }

    // Claim a per-host root volume, reusing an existing claim on retry.
    let claim = if state.cfg.claims_enabled() {
        match claim_for(&state, mac.as_ref(), record.as_ref().and_then(|r| r.stack.as_deref())).await
        {
            Ok(claim) => Some(claim),
            Err(err) => {
                Metrics::incr(&state.metrics.claim_failures);
                tracing::error!(%err, "claim failed");
                if let Some(mac) = &mac {
                    state
                        .boot
                        .observe(mac.clone(), Phase::Failed, Some(format!("claim failed: {err}")));
                }
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("#!ipxe\n# could not claim a root volume: {err}\nshell\n"),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let mut plan = BootPlan::resolve(&state.cfg, mac.as_ref(), record.as_ref());
    if let Some(attach) = claim.as_ref().and_then(|c| c.attach.as_ref()) {
        plan.portal = Some(&attach.address);
        plan.portal_port = attach.port;
        plan.nqn = Some(&attach.target);
        plan.nsid = attach.nsid;
    }
    if let Some(volume) = claim.as_ref().map(|c| c.volume_name.as_str()) {
        plan.volume = Some(volume);
    }

    let script = ipxe::render(&state.cfg, &plan);

    if let Some(mac) = &mac
        && let Some(version) = state.asset_status().version
    {
        state.boot.set_pallet_version(mac, &version);
    }

    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
    )
        .into_response()
}

async fn claim_for(
    state: &Shared,
    mac: Option<&Mac>,
    golden: Option<&str>,
) -> anyhow::Result<ClaimResponse> {
    let Some(client) = &state.claim_client else {
        anyhow::bail!("claims are not configured");
    };

    let mut claims = state.claims.lock().await;
    if let Some(mac) = mac
        && let Some(existing) = claims.get(mac)
    {
        tracing::debug!(%mac, claim = %existing.id, "reusing existing claim");
        return Ok(existing.clone());
    }

    let claim = client.claim(mac, golden).await?;
    if let Some(mac) = mac {
        claims.insert(mac.clone(), claim.clone());
    }
    Ok(claim)
}

#[derive(Debug, Serialize)]
struct Listing {
    assets: std::collections::BTreeMap<String, u64>,
}

async fn boot_listing(State(state): State<Shared>) -> Response {
    let mut assets = std::collections::BTreeMap::new();
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

/// A booting node telling us where it got to.
///
/// This is how the phases past the asset fetch become visible at all: nothing
/// else can see a machine between `switch_root` and joining a cluster.
#[derive(Debug, Deserialize)]
pub struct ReportRequest {
    mac: String,
    phase: String,
    #[serde(default)]
    detail: Option<String>,
}

async fn report(State(state): State<Shared>, axum::Json(req): axum::Json<ReportRequest>) -> Response {
    let mac = match Mac::parse(&req.mac) {
        Ok(mac) => mac,
        Err(err) => return (StatusCode::BAD_REQUEST, format!("bad mac: {err}\n")).into_response(),
    };

    let Some(phase) = parse_phase(&req.phase) else {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown phase {:?}\n", req.phase),
        )
            .into_response();
    };

    tracing::info!(%mac, ?phase, detail = ?req.detail, "node reported");
    state.boot.observe(mac, phase, req.detail);
    (StatusCode::ACCEPTED, "recorded\n").into_response()
}

/// A node describing its own hardware.
#[derive(Debug, Deserialize)]
pub struct InventoryRequest {
    mac: String,
    #[serde(flatten)]
    hardware: Inventory,
}

async fn inventory(
    State(state): State<Shared>,
    axum::Json(req): axum::Json<InventoryRequest>,
) -> Response {
    let mac = match Mac::parse(&req.mac) {
        Ok(mac) => mac,
        Err(err) => return (StatusCode::BAD_REQUEST, format!("bad mac: {err}\n")).into_response(),
    };

    tracing::info!(
        %mac,
        cpus = req.hardware.cpus,
        memory_kb = req.hardware.memory_kb,
        product = req.hardware.product.as_deref().unwrap_or(""),
        "node reported inventory"
    );
    state.boot.set_inventory(&mac, req.hardware);
    (StatusCode::ACCEPTED, "recorded\n").into_response()
}

fn parse_phase(raw: &str) -> Option<Phase> {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "script-fetched" => Some(Phase::ScriptFetched),
        "assets-fetched" => Some(Phase::AssetsFetched),
        "root-attached" => Some(Phase::RootAttached),
        "running" => Some(Phase::Running),
        "assimilating" => Some(Phase::Assimilating),
        "local" => Some(Phase::Local),
        "failed" => Some(Phase::Failed),
        _ => None,
    }
}

async fn components_feed(State(state): State<Shared>) -> Response {
    let feed = components::collect(&state.boot, &state.asset_status());
    axum::Json(feed).into_response()
}

/// Full-snapshot pushes, stormd-style: every 2 s, send when changed.
async fn ws_components(ws: WebSocketUpgrade, State(state): State<Shared>) -> Response {
    ws.on_upgrade(move |mut sock| async move {
        let mut last = String::new();
        loop {
            let feed = components::collect(&state.boot, &state.asset_status());
            let json = serde_json::to_string(&feed).unwrap_or_default();
            if json != last {
                if sock
                    .send(axum::extract::ws::Message::Text(json.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                last = json;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
}

async fn state_dump(State(state): State<Shared>) -> Response {
    axum::Json(state.boot.snapshot()).into_response()
}

async fn hosts_list(State(state): State<Shared>) -> Response {
    let counts = state.hosts.counts();
    axum::Json(serde_json::json!({
        "counts": {
            "file": counts.file,
            "boothost": counts.kube,
            "total": counts.total,
            "boothostSynced": counts.kube_synced,
        },
        "records": state.hosts.records(),
    }))
    .into_response()
}

async fn hosts_reload(State(state): State<Shared>) -> Response {
    match state.hosts.reload() {
        Ok(count) => axum::Json(serde_json::json!({ "records": count })).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reload failed: {err}\n"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_names_parse_in_the_spellings_a_client_might_send() {
        assert_eq!(parse_phase("root-attached"), Some(Phase::RootAttached));
        assert_eq!(parse_phase("root_attached"), Some(Phase::RootAttached));
        assert_eq!(parse_phase("  RUNNING "), Some(Phase::Running));
        assert_eq!(parse_phase("assimilating"), Some(Phase::Assimilating));
        assert_eq!(parse_phase("nonsense"), None);
    }
}
