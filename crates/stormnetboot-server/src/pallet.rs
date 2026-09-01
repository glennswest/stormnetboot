//! Boot pallet projection.
//!
//! The kernel and initramfs a machine executes come from the active signed
//! boot pallet in sbregistry, fetched by digest and verified before a single
//! byte is served. Nothing is ever served from a tag alone: a tag is only how
//! we discover which digest to pin.
//!
//! Members are materialised into a cache directory and served from there.
//! Every host pulls the same two files, so a cached file is a page-cache read
//! for the whole fleet — fetching per request would turn a boot storm into a
//! registry storm for no benefit.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::stormsig::{self, SIGNATURE_ARTIFACT_TYPE, SignatureDoc};

/// artifactType of a pallet manifest.
pub const PALLET_ARTIFACT_TYPE: &str = "application/vnd.stormblock.pallet.v1+json";
/// Media type of the pallet spec carried as the manifest's config blob.
pub const PALLET_CONFIG_TYPE: &str = "application/vnd.stormblock.pallet.config.v1+json";
/// Media type prefix shared by every member layer; the suffix is the role.
pub const MEMBER_MEDIA_PREFIX: &str = "application/vnd.stormblock.member.";

const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

const ANNOTATION_ROLE: &str = "org.stormblock.member.role";
const ANNOTATION_NAME: &str = "org.stormblock.member.name";

/// What the server is currently able to serve, for health and the console.
#[derive(Debug, Clone)]
pub struct AssetStatus {
    pub ready: bool,
    pub detail: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub signature_verified: bool,
}

impl Default for AssetStatus {
    fn default() -> Self {
        Self {
            ready: false,
            detail: "no boot pallet fetched yet".into(),
            version: None,
            digest: None,
            signature_verified: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default, rename = "artifactType")]
    artifact_type: Option<String>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default, rename = "artifactType")]
    artifact_type: Option<String>,
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Index {
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

/// The pallet's config blob — the spec describing the pallet.
#[derive(Debug, Default, Deserialize)]
struct PalletSpec {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    version_label: String,
    #[serde(default)]
    members: Vec<MemberDef>,
}

/// A member as described by the spec.
///
/// The `source` tag matters: a `blob` member's bytes are fetched by digest,
/// but an `inline` member's content is carried in this spec itself and there
/// is no blob to fetch. Treating them alike is how a cmdline turns into a 404.
#[derive(Debug, Deserialize)]
struct MemberDef {
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// Which member roles this server serves, and what it calls them on disk.
///
/// Firmware asks for these filenames, so the mapping from a pallet's roles to
/// them lives in one place.
fn filename_for_role(role: &str) -> Option<&'static str> {
    match role {
        "kernel" => Some("vmlinuz"),
        "initramfs" => Some("initramfs.img"),
        "cmdline" | "bootconfig" => Some("cmdline"),
        _ => None,
    }
}

pub struct PalletSource {
    client: reqwest::Client,
    /// Base URL of sbregistry, e.g. `http://registry:5100`.
    registry: String,
    /// Repository holding the boot pallet, e.g. `stormcos/boot`.
    repo: String,
    /// Tag or digest to resolve.
    reference: String,
    cache_dir: PathBuf,
    trusted_keys: Vec<String>,
    /// Refuse to serve a pallet whose signature does not verify.
    require_signature: bool,
}

pub struct Refreshed {
    pub status: AssetStatus,
    /// True when the pinned digest changed and files were rewritten.
    pub changed: bool,
}

impl PalletSource {
    pub fn new(
        registry: String,
        repo: String,
        reference: String,
        cache_dir: PathBuf,
        trusted_keys: Vec<String>,
        require_signature: bool,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("stormnetboot-server/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            client,
            registry: registry.trim_end_matches('/').to_owned(),
            repo,
            reference,
            cache_dir,
            trusted_keys,
            require_signature,
        })
    }

    /// Fetch the pallet if its digest changed, verify it, and materialise its
    /// members into the cache directory.
    pub async fn refresh(&self, current_digest: Option<&str>) -> anyhow::Result<Refreshed> {
        let (manifest_bytes, digest) = self.fetch_manifest().await?;

        if Some(digest.as_str()) == current_digest {
            // Same pallet we already serve. Nothing to fetch, nothing to verify
            // again: the bytes on disk were verified when they were written.
            return Ok(Refreshed {
                status: self.status_for(&manifest_bytes, &digest, true, true)?,
                changed: false,
            });
        }

        tracing::info!(%digest, "new boot pallet digest; fetching");

        let signature_verified = self.verify_signature(&digest).await?;
        if self.require_signature && !signature_verified {
            bail!(
                "refusing to serve pallet {digest}: no trusted signature (set \
                 --allow-unsigned only where that is genuinely acceptable)"
            );
        }

        let manifest: Manifest =
            serde_json::from_slice(&manifest_bytes).context("parsing pallet manifest")?;

        // A pallet announces itself either way; accept both rather than
        // rejecting a valid pallet on a technicality.
        let is_pallet = manifest.artifact_type.as_deref() == Some(PALLET_ARTIFACT_TYPE)
            || manifest.config.media_type == PALLET_CONFIG_TYPE;
        if !is_pallet {
            bail!(
                "{}:{} is not a pallet (artifactType {:?}, config {})",
                self.repo,
                self.reference,
                manifest.artifact_type,
                manifest.config.media_type
            );
        }

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .with_context(|| format!("creating {}", self.cache_dir.display()))?;

        // The spec carries inline members whose bytes exist nowhere else.
        let spec: PalletSpec = match self.fetch_blob_or_manifest(&manifest.config.digest, false).await
        {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(%err, "could not read pallet spec; inline members unavailable");
                PalletSpec::default()
            }
        };

        let mut served = Vec::new();
        for layer in &manifest.layers {
            let Some(role) = member_role(layer) else {
                continue;
            };
            let Some(filename) = filename_for_role(&role) else {
                tracing::debug!(role, "member role not served by this server; skipping");
                continue;
            };

            // An inline member's content is in the spec, not behind a blob.
            if let Some(text) = inline_text(&spec, &role) {
                let path = self.cache_dir.join(filename);
                tokio::fs::write(&path, text.as_bytes())
                    .await
                    .with_context(|| format!("writing inline member {filename}"))?;
                tracing::info!(filename, bytes = text.len(), "inline member written");
                served.push(filename);
                continue;
            }

            self.fetch_member(layer, filename).await.with_context(|| {
                format!("fetching member {role} ({})", layer.digest)
            })?;
            served.push(filename);
        }

        if !served.contains(&"vmlinuz") || !served.contains(&"initramfs.img") {
            bail!(
                "boot pallet {digest} is missing a kernel or initramfs (served: {served:?})"
            );
        }

        tracing::info!(%digest, members = ?served, signature_verified, "boot pallet ready");

        Ok(Refreshed {
            status: self.status_for(&manifest_bytes, &digest, true, signature_verified)?,
            changed: true,
        })
    }

    fn status_for(
        &self,
        manifest_bytes: &[u8],
        digest: &str,
        ready: bool,
        signature_verified: bool,
    ) -> anyhow::Result<AssetStatus> {
        let manifest: Manifest = serde_json::from_slice(manifest_bytes)?;
        let version = manifest
            .annotations
            .get("org.stormblock.pallet.version_label")
            .or_else(|| manifest.annotations.get("org.stormblock.pallet.version"))
            .cloned()
            .or_else(|| Some(self.reference.clone()));

        Ok(AssetStatus {
            ready,
            detail: format!("{}:{}", self.repo, self.reference),
            version,
            digest: Some(digest.to_owned()),
            signature_verified,
        })
    }

    async fn fetch_manifest(&self) -> anyhow::Result<(Vec<u8>, String)> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            self.registry, self.repo, self.reference
        );
        let resp = self
            .client
            .get(&url)
            .header("Accept", format!("{OCI_MANIFEST_TYPE}, {OCI_INDEX_TYPE}"))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if !resp.status().is_success() {
            bail!("GET {url} returned {}", resp.status());
        }

        // Prefer the registry's own digest header; fall back to hashing the
        // bytes we actually received, which is what we would verify against
        // anyway.
        let header_digest = resp
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        let bytes = resp.bytes().await.context("reading manifest body")?.to_vec();
        let computed = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));

        if let Some(header) = &header_digest
            && header.to_ascii_lowercase() != computed
        {
            bail!("manifest digest mismatch: registry said {header}, bytes hash to {computed}");
        }

        Ok((bytes, header_digest.unwrap_or(computed)))
    }

    /// Find and check a trusted signature over this manifest digest.
    ///
    /// Returns false when no signature exists; errors only when one exists and
    /// is bad, because "signed by someone we do not trust" and "not signed"
    /// are different situations for the operator.
    async fn verify_signature(&self, subject_digest: &str) -> anyhow::Result<bool> {
        if self.trusted_keys.is_empty() {
            tracing::warn!("no trusted signing keys configured; cannot verify the boot pallet");
            return Ok(false);
        }

        let url = format!(
            "{}/v2/{}/referrers/{}?artifactType={}",
            self.registry, self.repo, subject_digest, SIGNATURE_ARTIFACT_TYPE
        );
        let resp = self.client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            tracing::warn!(status = %resp.status(), "referrers query failed; treating as unsigned");
            return Ok(false);
        }

        let index: Index = resp.json().await.context("parsing referrers index")?;
        let mut last_error = None;

        for referrer in &index.manifests {
            if referrer.artifact_type.as_deref() != Some(SIGNATURE_ARTIFACT_TYPE) {
                continue;
            }

            // The signature document is the referrer's *config blob* — no
            // layers, so this is one extra fetch, not two.
            let manifest_bytes = self.fetch_blob_or_manifest(&referrer.digest, true).await?;
            let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
                Ok(m) => m,
                Err(err) => {
                    last_error = Some(format!("signature manifest unparseable: {err}"));
                    continue;
                }
            };

            let doc_bytes = self.fetch_blob_or_manifest(&manifest.config.digest, false).await?;
            let doc: SignatureDoc = match serde_json::from_slice(&doc_bytes) {
                Ok(d) => d,
                Err(err) => {
                    last_error = Some(format!("signature document unparseable: {err}"));
                    continue;
                }
            };

            match stormsig::verify(&doc, subject_digest, &self.trusted_keys) {
                Ok(statement) => {
                    tracing::info!(
                        key_id = %doc.key_id,
                        signed_at = statement.signed_at,
                        "boot pallet signature verified"
                    );
                    return Ok(true);
                }
                Err(err) => {
                    tracing::warn!(key_id = %doc.key_id, %err, "signature rejected");
                    last_error = Some(err.to_string());
                }
            }
        }

        if let Some(err) = last_error {
            bail!("no acceptable signature for {subject_digest}: {err}");
        }
        Ok(false)
    }

    async fn fetch_blob_or_manifest(
        &self,
        digest: &str,
        is_manifest: bool,
    ) -> anyhow::Result<Vec<u8>> {
        let kind = if is_manifest { "manifests" } else { "blobs" };
        let url = format!("{}/v2/{}/{kind}/{digest}", self.registry, self.repo);
        let resp = self
            .client
            .get(&url)
            .header("Accept", OCI_MANIFEST_TYPE)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url} returned {}", resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Download one member into the cache, verifying its digest before it can
    /// be served. Written to a temporary name and renamed, so a torn download
    /// can never be handed to a booting machine.
    async fn fetch_member(&self, layer: &Descriptor, filename: &str) -> anyhow::Result<()> {
        let url = format!("{}/v2/{}/blobs/{}", self.registry, self.repo, layer.digest);
        let resp = self.client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url} returned {}", resp.status());
        }

        let bytes = resp.bytes().await.context("reading member body")?;
        let computed = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        if computed != layer.digest.to_ascii_lowercase() {
            bail!(
                "member {filename} digest mismatch: expected {}, got {computed}",
                layer.digest
            );
        }
        if layer.size != 0 && layer.size != bytes.len() as u64 {
            bail!(
                "member {filename} size mismatch: manifest says {}, got {}",
                layer.size,
                bytes.len()
            );
        }

        let final_path = self.cache_dir.join(filename);
        let tmp_path = self.cache_dir.join(format!(".{filename}.tmp"));
        tokio::fs::write(&tmp_path, &bytes)
            .await
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .with_context(|| format!("renaming into {}", final_path.display()))?;

        tracing::info!(filename, bytes = bytes.len(), "member cached");
        Ok(())
    }
}

/// Inline content for a role, if the spec carries it.
fn inline_text<'a>(spec: &'a PalletSpec, role: &str) -> Option<&'a str> {
    spec.members
        .iter()
        .find(|m| m.source == "inline" && (m.role == role || m.name == role))
        .and_then(|m| m.text.as_deref())
}

/// A member's role, from its annotation or its media type suffix.
fn member_role(layer: &Descriptor) -> Option<String> {
    if let Some(role) = layer.annotations.get(ANNOTATION_ROLE) {
        return Some(role.clone());
    }
    if let Some(name) = layer.annotations.get(ANNOTATION_NAME) {
        return Some(name.clone());
    }
    layer
        .media_type
        .strip_prefix(MEMBER_MEDIA_PREFIX)
        .map(|s| s.to_owned())
}

/// Whether the cache holds everything a machine needs to boot.
pub async fn cache_is_complete(dir: &Path) -> bool {
    for name in ["vmlinuz", "initramfs.img"] {
        if !tokio::fs::try_exists(dir.join(name)).await.unwrap_or(false) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(media: &str, role: Option<&str>) -> Descriptor {
        let mut annotations = std::collections::BTreeMap::new();
        if let Some(role) = role {
            annotations.insert(ANNOTATION_ROLE.to_owned(), role.to_owned());
        }
        Descriptor {
            media_type: media.to_owned(),
            digest: "sha256:00".into(),
            size: 0,
            artifact_type: None,
            annotations,
        }
    }

    #[test]
    fn role_comes_from_the_annotation_first() {
        let l = layer("application/vnd.stormblock.member.kernel", Some("initramfs"));
        assert_eq!(member_role(&l).as_deref(), Some("initramfs"));
    }

    #[test]
    fn role_falls_back_to_the_media_type_suffix() {
        let l = layer("application/vnd.stormblock.member.kernel", None);
        assert_eq!(member_role(&l).as_deref(), Some("kernel"));
    }

    #[test]
    fn unknown_media_types_have_no_role() {
        let l = layer("application/octet-stream", None);
        assert_eq!(member_role(&l), None);
    }

    #[test]
    fn only_boot_relevant_roles_map_to_filenames() {
        assert_eq!(filename_for_role("kernel"), Some("vmlinuz"));
        assert_eq!(filename_for_role("initramfs"), Some("initramfs.img"));
        assert_eq!(filename_for_role("cmdline"), Some("cmdline"));
        assert_eq!(filename_for_role("rootfs"), None);
    }

    #[test]
    fn default_status_is_not_ready() {
        let status = AssetStatus::default();
        assert!(!status.ready);
        assert!(!status.signature_verified);
        assert!(status.digest.is_none());
    }
}
