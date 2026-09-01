//! Per-host root volume claims.
//!
//! Each booting machine gets its own copy-on-write clone of one golden, so a
//! thousand hosts cost the appliance metadata rather than a thousand copies.
//! The claim is made here, server-side at script-render time, which is what
//! keeps the initramfs dumb: it is handed a portal, a target and a namespace
//! on the kernel command line and never has to negotiate anything.
//!
//! Field names below are sbregistry's, not the engine's. sbregistry flattens
//! the engine's attach block into `address` / `port` / `target`, and the port
//! is **per-export** — assuming the shared 4420 attaches the wrong thing or
//! nothing at all.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::mac::Mac;

#[derive(Debug, Serialize)]
struct ClaimRequest<'a> {
    golden: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    consumer: Option<String>,
}

/// Attach details as sbregistry reports them.
#[derive(Debug, Clone, Deserialize)]
pub struct Attach {
    /// e.g. `nvme-tcp`. Read, never assumed.
    #[serde(default)]
    pub protocol: String,
    /// Host or IP only — never `host:port`.
    #[serde(default)]
    pub address: String,
    /// Per-export port, not the subsystem's shared listener.
    #[serde(default)]
    pub port: u16,
    /// The NQN (or IQN for an iSCSI export). There is no `nqn` key.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub nsid: Option<u32>,
    /// Informational RouterOS `/disk add` line, if the registry emitted one.
    #[serde(default)]
    pub disk_add: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaimResponse {
    pub id: String,
    #[serde(default)]
    pub golden: String,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub volume_name: String,
    /// Null until the registry has an export for the clone.
    pub attach: Option<Attach>,
}

impl ClaimResponse {
    /// Whether this claim can actually be booted from.
    pub fn is_attachable(&self) -> bool {
        self.attach
            .as_ref()
            .is_some_and(|a| !a.address.is_empty() && !a.target.is_empty() && a.port != 0)
    }
}

pub struct ClaimClient {
    client: reqwest::Client,
    registry: String,
    /// Golden every booting host clones from, unless its record names another.
    default_golden: String,
}

impl ClaimClient {
    pub fn new(registry: String, default_golden: String) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("stormnetboot-server/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building claim HTTP client")?;
        Ok(Self {
            client,
            registry: registry.trim_end_matches('/').to_owned(),
            default_golden,
        })
    }

    /// Claim a clone for one host.
    ///
    /// The MAC is passed as the consumer so the registry's own view names the
    /// machine that holds it: an operator looking at `GET /v1/clones` should
    /// see which host has what without cross-referencing anything.
    pub async fn claim(
        &self,
        mac: Option<&Mac>,
        golden: Option<&str>,
    ) -> anyhow::Result<ClaimResponse> {
        let golden = golden.unwrap_or(&self.default_golden);
        let url = format!("{}/v1/clones/claim", self.registry);
        let body = ClaimRequest {
            golden,
            consumer: mac.map(|m| m.to_string()),
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The registry's error text is the useful part — it names the
            // golden it could not find.
            bail!("claim for golden {golden} failed ({status}): {}", text.trim());
        }

        let claim: ClaimResponse =
            serde_json::from_str(&text).with_context(|| format!("parsing claim response: {text}"))?;

        if !claim.is_attachable() {
            bail!(
                "claim {} for golden {golden} has no usable attach info (state {})",
                claim.id,
                claim.state
            );
        }

        tracing::info!(
            claim = %claim.id,
            volume = %claim.volume_name,
            target = %claim.attach.as_ref().map(|a| a.target.as_str()).unwrap_or(""),
            "claimed root volume"
        );
        Ok(claim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response shape sbregistry documents, verbatim in structure.
    const SAMPLE: &str = r#"{
        "id": "18f3c2a91b4",
        "golden": "stormcos-boot",
        "template": "img-abcdef012345",
        "state": "bound",
        "consumer": "aa:bb:cc:dd:ee:ff",
        "volume_name": "clone-stormcos-18f3c2a91b4",
        "attach": {
            "protocol": "nvme-tcp",
            "address": "192.168.200.21",
            "port": 4431,
            "target": "nqn.2026-08.lo.gt:img-abcdef012345-c1",
            "nsid": 1,
            "disk_add": "/disk add type=nvme-tcp ..."
        },
        "mounts": [{"dst": "/usr", "rel_src": "golden/usr"}]
    }"#;

    #[test]
    fn parses_the_registry_response_shape() {
        let claim: ClaimResponse = serde_json::from_str(SAMPLE).unwrap();
        let attach = claim.attach.as_ref().unwrap();

        assert_eq!(claim.id, "18f3c2a91b4");
        assert_eq!(claim.volume_name, "clone-stormcos-18f3c2a91b4");
        assert_eq!(attach.address, "192.168.200.21");
        // Per-export port, not the shared 4420.
        assert_eq!(attach.port, 4431);
        assert_eq!(attach.target, "nqn.2026-08.lo.gt:img-abcdef012345-c1");
        assert_eq!(attach.nsid, Some(1));
        assert!(claim.is_attachable());
    }

    #[test]
    fn a_claim_without_an_export_is_not_attachable() {
        let claim: ClaimResponse = serde_json::from_str(
            r#"{"id":"x","volume_name":"v","state":"claimed","attach":null}"#,
        )
        .unwrap();
        assert!(!claim.is_attachable());
    }

    #[test]
    fn an_empty_target_is_not_attachable() {
        let claim: ClaimResponse = serde_json::from_str(
            r#"{"id":"x","volume_name":"v","state":"bound",
                "attach":{"protocol":"nvme-tcp","address":"10.0.0.1","port":4431,"target":""}}"#,
        )
        .unwrap();
        assert!(!claim.is_attachable());
    }

    #[test]
    fn a_zero_port_is_not_attachable() {
        let claim: ClaimResponse = serde_json::from_str(
            r#"{"id":"x","volume_name":"v","state":"bound",
                "attach":{"protocol":"nvme-tcp","address":"10.0.0.1","port":0,"target":"nqn.x"}}"#,
        )
        .unwrap();
        assert!(!claim.is_attachable());
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // The registry may grow fields; a boot server must not break on them.
        let claim: ClaimResponse = serde_json::from_str(
            r#"{"id":"x","volume_name":"v","state":"bound","future_field":42,
                "attach":{"protocol":"nvme-tcp","address":"10.0.0.1","port":1,
                          "target":"nqn.x","brand_new":"ignored"}}"#,
        )
        .unwrap();
        assert!(claim.is_attachable());
    }
}
