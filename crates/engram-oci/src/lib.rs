//! OCI Distribution Spec client for Engram-managed artifacts.
//!
//! Two artifact families:
//!
//! 1. **Bake images** — pre-baked rootfs.ext4 + manifest.toml. Two
//!    layers, custom mediaTypes; host-agents pull and use the layer
//!    bytes directly (no Docker daemon required).
//! 2. **Harness packs** — directory of executable + sidecars, tarred
//!    into a single layer.
//!
//! Both push to and pull from any standard OCI Distribution registry
//! (GHCR, GCR, ECR, Harbor, local `registry:2`). Auth is resolved via
//! the [`RegistryAuthResolver`] trait — the coordinator wires this to
//! its encrypted Postgres registry-credential store; tests / the host
//! agent's anonymous-only path use [`AnonymousResolver`].
//!
//! ## HTTP fallback for local registries
//!
//! Standard OCI clients reject plaintext HTTP for security. We allow
//! it only when the registry host is `localhost`, `127.0.0.1`, or
//! `::1` — same heuristic Docker uses for `--insecure-registry`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use oci_client::client::{
    BlobResponse, ClientConfig, ClientProtocol, Config, ImageLayer, PushResponse,
};
use oci_client::manifest::OCI_IMAGE_MEDIA_TYPE;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod chunk_resolver;
pub mod docker_config;
pub mod media_types;

pub use chunk_resolver::{OciBlobLocator, OciChunkIndex, OciChunkResolver};
pub use docker_config::DockerConfigResolver;
pub use media_types::*;

/// Resolves auth credentials for a given registry host.
///
/// Returning `None` is the anonymous-pull / unauthenticated path —
/// works for public registries (`docker.io/library/...`) and the
/// local dev `registry:2` (no auth configured).
#[async_trait]
pub trait RegistryAuthResolver: Send + Sync {
    async fn resolve(&self, registry_host: &str) -> Result<Option<BasicCreds>, OciError>;
}

#[derive(Clone, Debug)]
pub struct BasicCreds {
    pub username: String,
    pub password: String,
}

/// Resolver that always returns `None`. Used in dev when no creds are
/// configured, in tests, and as a fallback when the metadata-store
/// resolver finds no row for the host.
pub struct AnonymousResolver;

#[async_trait]
impl RegistryAuthResolver for AnonymousResolver {
    async fn resolve(&self, _registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        Ok(None)
    }
}

/// Wraps `oci-client::Client` with Engram-specific push/pull verbs
/// and credential resolution.
#[derive(Clone)]
pub struct OciClient {
    inner: Client,
    auth: Arc<dyn RegistryAuthResolver>,
}

impl OciClient {
    pub fn new(auth: Arc<dyn RegistryAuthResolver>) -> Self {
        // We can't know per-call which registries are HTTP vs HTTPS at
        // construction time (the user passes URIs as strings). Set
        // `Https` as the default; we re-pick the protocol per push/pull
        // by reconstructing the client config when the host looks
        // plaintext-eligible (loopback). This is mildly inefficient but
        // simple — registries are pulled from infrequently relative to
        // session creation throughput.
        let inner = Client::new(ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        });
        Self { inner, auth }
    }

    /// Build a client whose protocol matches `reference`'s registry
    /// host: `Http` for loopback, `Https` otherwise. Used per-operation
    /// because plaintext is the wrong default everywhere except dev.
    fn client_for(reference: &Reference) -> Client {
        let proto = if is_loopback_host(reference.registry()) {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        };
        Client::new(ClientConfig {
            protocol: proto,
            ..Default::default()
        })
    }

    async fn auth_for(&self, reference: &Reference) -> Result<RegistryAuth, OciError> {
        match self.auth.resolve(reference.registry()).await? {
            Some(c) => Ok(RegistryAuth::Basic(c.username, c.password)),
            None => Ok(RegistryAuth::Anonymous),
        }
    }

    /// Push a bake image artifact. Up to three layers:
    ///
    /// - layer 0: `manifest.toml` content (uncompressed bytes)
    /// - layer 1 (optional): `rootfs.ext4` content (raw bytes,
    ///   large). Skipped when `bundle_json` is `Some` — ADR 0007
    ///   chunked storage moves disk bytes into the chunk store, so
    ///   pushing the full ext4 in the OCI layer is wire-redundant
    ///   (every push would otherwise transfer the disk twice:
    ///   chunks via BlobStorage, then bytes via the registry).
    /// - layer 2 (optional): `bundle.json` — ADR 0007 chunk-manifest
    ///   pointer set (tiny). Pullers that understand the bundle
    ///   resolve disk bytes from the chunk store via its
    ///   `disk_manifest`.
    ///
    /// At least one of `rootfs_ext4` or `bundle_json` must be
    /// provided — a pure-manifest push has no consumable disk
    /// bytes, and we return a typed error so callers can't push
    /// half-formed artifacts.
    ///
    /// `config_json` is small JSON metadata (format, agent version,
    /// transport) used by the host-agent at pull time to validate
    /// compatibility before fetching the rootfs blob.
    pub async fn push_image(
        &self,
        uri: &str,
        manifest_toml: &[u8],
        rootfs_ext4: Option<&Path>,
        config_json: &[u8],
        bundle_json: Option<&[u8]>,
    ) -> Result<Digest256, OciError> {
        if rootfs_ext4.is_none() && bundle_json.is_none() {
            return Err(OciError::InvalidUri(
                "push_image: must supply rootfs_ext4 OR bundle_json".into(),
            ));
        }
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let manifest_layer = ImageLayer::new(
            manifest_toml.to_vec(),
            ENGRAM_MANIFEST_MEDIA_TYPE.to_string(),
            None,
        );
        let mut layers = vec![manifest_layer];
        if let Some(rootfs_path) = rootfs_ext4 {
            let rootfs_bytes = read_file_bytes(rootfs_path).await?;
            layers.push(ImageLayer::new(
                rootfs_bytes,
                ENGRAM_ROOTFS_EXT4_MEDIA_TYPE.to_string(),
                None,
            ));
        }
        if let Some(bytes) = bundle_json {
            layers.push(ImageLayer::new(
                bytes.to_vec(),
                ENGRAM_BUNDLE_MEDIA_TYPE.to_string(),
                None,
            ));
        }

        let config = Config::new(
            config_json.to_vec(),
            ENGRAM_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            None,
        );

        let resp: PushResponse = client
            .push(&reference, &layers, config, &auth, None)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;
        // PushResponse exposes the manifest_url; we want the digest of
        // the manifest. oci-client computes it on push and returns it
        // in the response.
        Ok(Digest256(resp.manifest_url))
    }

    /// Pull a bake image artifact. Writes
    /// `<dest>/manifest.toml` and `<dest>/rootfs.ext4`. Returns the
    /// digest of the OCI manifest (a content address — a re-pull of
    /// the same tag with unchanged layers yields the same digest).
    pub async fn pull_image(&self, uri: &str, dest: &Path) -> Result<PulledImage, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![
            ENGRAM_MANIFEST_MEDIA_TYPE,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE,
            ENGRAM_BUNDLE_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE,
            ENGRAM_CHUNKS_DISK_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE,
            ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
        ];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        tokio::fs::create_dir_all(dest)
            .await
            .map_err(OciError::Io)?;

        let mut manifest_path = None;
        let mut rootfs_path = None;
        let mut bundle_path = None;
        let mut disk_bootstrap_path = None;
        let mut disk_chunks_blob_digest = None;
        let mut memory_bootstrap_path = None;
        let mut memory_chunks_blob_digest = None;
        let mut snapshot_state_path = None;
        let mut snapshot_sidecar_path = None;
        for layer in &data.layers {
            match layer.media_type.as_str() {
                ENGRAM_MANIFEST_MEDIA_TYPE => {
                    let p = dest.join("manifest.toml");
                    write_file_bytes(&p, &layer.data).await?;
                    manifest_path = Some(p);
                }
                ENGRAM_ROOTFS_EXT4_MEDIA_TYPE => {
                    let p = dest.join("rootfs.ext4");
                    write_file_bytes(&p, &layer.data).await?;
                    rootfs_path = Some(p);
                }
                ENGRAM_BUNDLE_MEDIA_TYPE => {
                    let p = dest.join("bundle.json");
                    write_file_bytes(&p, &layer.data).await?;
                    bundle_path = Some(p);
                }
                ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE => {
                    let p = dest.join("bootstrap.disk.json");
                    write_file_bytes(&p, &layer.data).await?;
                    disk_bootstrap_path = Some(p);
                }
                ENGRAM_CHUNKS_DISK_MEDIA_TYPE => {
                    // We deliberately do *not* write the chunk blob
                    // to disk. The whole point of Nydus-shaped
                    // artifacts is that chunks are Range-GETted
                    // lazily — pulling the full blob defeats that.
                    // Record the layer digest so the resolver can
                    // address it later.
                    disk_chunks_blob_digest = Some(sha256_digest(&layer.data).0);
                }
                ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE => {
                    let p = dest.join("bootstrap.memory.json");
                    write_file_bytes(&p, &layer.data).await?;
                    memory_bootstrap_path = Some(p);
                }
                ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE => {
                    memory_chunks_blob_digest = Some(sha256_digest(&layer.data).0);
                }
                ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE => {
                    let p = dest.join("state.bin");
                    write_file_bytes(&p, &layer.data).await?;
                    snapshot_state_path = Some(p);
                }
                ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE => {
                    let p = dest.join("sidecar.json");
                    write_file_bytes(&p, &layer.data).await?;
                    snapshot_sidecar_path = Some(p);
                }
                other => {
                    tracing::debug!(media_type = %other, "skipping unrecognized layer");
                }
            }
        }
        let manifest_path = manifest_path.ok_or_else(|| {
            OciError::Distribution("pulled artifact missing engram manifest layer".into())
        })?;
        // ADR 0007 Phase 6: the rootfs.ext4 layer is optional when
        // the artifact carries a `bundle.json` (disk bytes flow
        // through the chunk store instead). Reject only when *all*
        // disk sources are missing — no rootfs, no bundle, no
        // chunked-OCI bootstrap. That's a malformed artifact.
        if rootfs_path.is_none() && bundle_path.is_none() && disk_bootstrap_path.is_none() {
            return Err(OciError::Distribution(
                "pulled artifact missing rootfs.ext4, bundle.json, and chunked-OCI bootstrap layers"
                    .into(),
            ));
        }

        Ok(PulledImage {
            manifest_path,
            rootfs_path,
            bundle_path,
            disk_bootstrap_path,
            disk_chunks_blob_digest,
            memory_bootstrap_path,
            memory_chunks_blob_digest,
            snapshot_state_path,
            snapshot_sidecar_path,
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
        })
    }

    /// Fetch the engram manifest.toml layer and the optional
    /// bundle.json layer of an engram image artifact, plus the
    /// top-level manifest digest. Skips writing the rootfs.ext4
    /// layer entirely.
    ///
    /// Used by the coordinator's `/api/enabled-images` POST handler:
    /// when an operator enables an image, we cache its parsed
    /// manifest on the row so session-create has zero registry I/O.
    /// ADR 0014 M1.11 added the bundle.json extraction so the same
    /// pull also drives the templates-cascade — when the bundle
    /// carries a `canonical_snapshot` block, the handler inserts
    /// `snapshots` + `templates` rows in one PG transaction.
    ///
    /// `oci-distribution`'s `pull` is a single round-trip for the
    /// index + all layer blobs, so we still pay one fetch for the
    /// rootfs bytes — but we don't write them anywhere. The
    /// savings vs. a full `pull_image()` are storage (no rootfs.ext4
    /// file written) and cleanup (no temp dir to manage).
    pub async fn pull_engram_metadata(&self, uri: &str) -> Result<EngramMetadataLayers, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        // Must list every media type the artifact may carry —
        // oci-client validates each pulled layer against this set and
        // errors on the first mismatch (even though we only consume
        // the manifest + bundle layers here). Keep this in sync with
        // [`Self::pull_image`]'s accepted list.
        let accepted = vec![
            ENGRAM_MANIFEST_MEDIA_TYPE,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE,
            ENGRAM_BUNDLE_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE,
            ENGRAM_CHUNKS_DISK_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE,
            ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
        ];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        let manifest_layer = data
            .layers
            .iter()
            .find(|l| l.media_type == ENGRAM_MANIFEST_MEDIA_TYPE)
            .ok_or_else(|| {
                OciError::Distribution("pulled artifact missing engram manifest layer".into())
            })?;
        let bundle_bytes = data
            .layers
            .iter()
            .find(|l| l.media_type == ENGRAM_BUNDLE_MEDIA_TYPE)
            .map(|l| l.data.clone());

        Ok(EngramMetadataLayers {
            manifest_toml: manifest_layer.data.clone(),
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
            bundle_json: bundle_bytes,
        })
    }

    /// ADR 0014 M1.11: pull every engram-canonical layer of the
    /// artifact at `uri` and return them in memory. Used by the
    /// coord's `enable_image` to materialize the artifact into the
    /// deployment's BlobStorage at canonical keys — once that lands,
    /// host-agents read everything by key, the bake's environment
    /// doesn't need to share a blob backend with prod, and the OCI
    /// artifact stays the portable unit of distribution.
    ///
    /// Differs from `pull_image` in that the chunk-blob layers are
    /// returned in-memory (caller wants the bytes to slice them
    /// into BlobStorage), and nothing is written to disk. For a
    /// typical demo image the total is a few hundred MiB — fine
    /// for the coord's RAM. For multi-GiB rootfses we'd switch to
    /// a streaming variant later.
    pub async fn pull_template_artifacts(&self, uri: &str) -> Result<TemplateArtifacts, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![
            ENGRAM_MANIFEST_MEDIA_TYPE,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE,
            ENGRAM_BUNDLE_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE,
            ENGRAM_CHUNKS_DISK_MEDIA_TYPE,
            ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE,
            ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE,
            ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
        ];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        let mut out = TemplateArtifacts {
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
            manifest_toml: Vec::new(),
            bundle_json: None,
            disk_bootstrap_json: None,
            disk_chunks_blob: None,
            memory_bootstrap_json: None,
            memory_chunks_blob: None,
            snapshot_state: None,
            snapshot_sidecar_json: None,
            snapshot_working_set_json: None,
        };
        for layer in data.layers {
            match layer.media_type.as_str() {
                ENGRAM_MANIFEST_MEDIA_TYPE => out.manifest_toml = layer.data,
                ENGRAM_BUNDLE_MEDIA_TYPE => out.bundle_json = Some(layer.data),
                ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE => out.disk_bootstrap_json = Some(layer.data),
                ENGRAM_CHUNKS_DISK_MEDIA_TYPE => out.disk_chunks_blob = Some(layer.data),
                ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE => out.memory_bootstrap_json = Some(layer.data),
                ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE => out.memory_chunks_blob = Some(layer.data),
                ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE => out.snapshot_state = Some(layer.data),
                ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE => out.snapshot_sidecar_json = Some(layer.data),
                ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE => {
                    out.snapshot_working_set_json = Some(layer.data)
                }
                _ => {}
            }
        }
        if out.manifest_toml.is_empty() {
            return Err(OciError::Distribution(
                "pulled artifact missing engram manifest layer".into(),
            ));
        }
        Ok(out)
    }

    /// Push a harness pack artifact. Tars + gzips `pack_dir` and pushes
    /// it as a single layer.
    pub async fn push_harness(&self, uri: &str, pack_dir: &Path) -> Result<Digest256, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let tar_gz = tar_gz_dir(pack_dir).await?;
        let layer = ImageLayer::new(tar_gz, ENGRAM_HARNESS_TAR_MEDIA_TYPE.to_string(), None);
        let config = Config::new(
            br#"{"kind":"engram-harness-v1"}"#.to_vec(),
            ENGRAM_HARNESS_CONFIG_MEDIA_TYPE.to_string(),
            None,
        );

        let resp = client
            .push(&reference, &[layer], config, &auth, None)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;
        Ok(Digest256(resp.manifest_url))
    }

    /// Pull a harness pack artifact and extract it into `dest`. The
    /// caller is responsible for `dest` being empty; we don't clear
    /// pre-existing files.
    pub async fn pull_harness(&self, uri: &str, dest: &Path) -> Result<PulledHarness, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![ENGRAM_HARNESS_TAR_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        tokio::fs::create_dir_all(dest)
            .await
            .map_err(OciError::Io)?;

        let layer = data
            .layers
            .iter()
            .find(|l| l.media_type == ENGRAM_HARNESS_TAR_MEDIA_TYPE)
            .ok_or_else(|| {
                OciError::Distribution("pulled artifact missing engram harness tar layer".into())
            })?;

        untar_gz_to_dir(&layer.data, dest).await?;

        Ok(PulledHarness {
            pack_dir: dest.to_path_buf(),
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
        })
    }

    /// Round-trip token for the suppress-unused-warning. Useful when
    /// embedding `OciClient` in a service that may not exercise it on
    /// some code paths.
    pub fn inner(&self) -> &Client {
        &self.inner
    }

    /// Push a Nydus-shaped chunked image artifact (ADR 0008 Phase 3).
    ///
    /// Layers pushed (in order):
    ///
    /// - `manifest.toml` (always)
    /// - `bundle.json` v2 (always — carries the bootstrap layer
    ///   digests so consumers can locate them)
    /// - `bootstrap.disk.v1+json` (always)
    /// - `chunks.disk.v1` (always)
    /// - `bootstrap.memory.v1+json` (when canonical memory was
    ///   captured at bake time)
    /// - `chunks.memory.v1` (paired with the memory bootstrap)
    ///
    /// The bake is expected to have pre-computed the bootstrap +
    /// chunk-blob bytes via `engram_chunk_store::Bootstrap::build_from_manifest`
    /// and to have written their sha256 digests into `bundle.json`
    /// before passing the bytes here. The OCI registry verifies the
    /// digests at push time.
    ///
    /// Note on memory cost: chunk-blob layers are passed as
    /// `Vec<u8>`; a 4 GiB ext4 image gives a 4 GiB allocation. v1
    /// accepts this; the obvious follow-up is a streaming push that
    /// reads from a temp file. The `oci-client` API doesn't expose
    /// streaming pushes today (`ImageLayer::new` takes owned bytes),
    /// so this is non-trivial — leave for when bakes hit memory
    /// pressure on a real builder.
    pub async fn push_chunked_image(
        &self,
        uri: &str,
        payload: ChunkedPushPayload,
    ) -> Result<Digest256, OciError> {
        // Symmetric guard: memory bootstrap and chunks must travel
        // together. A bootstrap without its chunk blob is
        // unconsumable; a chunk blob without its bootstrap is
        // un-addressable. Fail synchronously so a misconfigured
        // bake doesn't push a half-baked artifact.
        match (
            payload.memory_bootstrap_json.is_some(),
            payload.memory_chunks_blob.is_some(),
        ) {
            (true, true) | (false, false) => {}
            _ => {
                return Err(OciError::InvalidUri(
                    "push_chunked_image: memory_bootstrap and memory_chunks must be \
                     supplied together (both Some, or both None)"
                        .into(),
                ));
            }
        }
        // Same symmetry for the canonical-snapshot state+sidecar
        // pair. ADR 0014: the host's restore path needs both
        // state.bin and the FC sidecar manifest to bring the VM
        // back; one without the other strands the snapshot.
        match (
            payload.snapshot_state.is_some(),
            payload.snapshot_sidecar_json.is_some(),
        ) {
            (true, true) | (false, false) => {}
            _ => {
                return Err(OciError::InvalidUri(
                    "push_chunked_image: snapshot_state and snapshot_sidecar_json must be \
                     supplied together (both Some, or both None)"
                        .into(),
                ));
            }
        }
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let mut layers = vec![
            ImageLayer::new(
                payload.manifest_toml,
                ENGRAM_MANIFEST_MEDIA_TYPE.to_string(),
                None,
            ),
            ImageLayer::new(
                payload.bundle_json,
                ENGRAM_BUNDLE_MEDIA_TYPE.to_string(),
                None,
            ),
            ImageLayer::new(
                payload.disk_bootstrap_json,
                ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE.to_string(),
                None,
            ),
            ImageLayer::new(
                payload.disk_chunks_blob,
                ENGRAM_CHUNKS_DISK_MEDIA_TYPE.to_string(),
                None,
            ),
        ];
        if let Some(memory_bootstrap) = payload.memory_bootstrap_json {
            layers.push(ImageLayer::new(
                memory_bootstrap,
                ENGRAM_BOOTSTRAP_MEMORY_MEDIA_TYPE.to_string(),
                None,
            ));
        }
        if let Some(memory_chunks) = payload.memory_chunks_blob {
            layers.push(ImageLayer::new(
                memory_chunks,
                ENGRAM_CHUNKS_MEMORY_MEDIA_TYPE.to_string(),
                None,
            ));
        }
        if let Some(state_bin) = payload.snapshot_state {
            layers.push(ImageLayer::new(
                state_bin,
                ENGRAM_SNAPSHOT_STATE_MEDIA_TYPE.to_string(),
                None,
            ));
        }
        if let Some(sidecar) = payload.snapshot_sidecar_json {
            layers.push(ImageLayer::new(
                sidecar,
                ENGRAM_SNAPSHOT_SIDECAR_MEDIA_TYPE.to_string(),
                None,
            ));
        }
        if let Some(ws) = payload.snapshot_working_set_json {
            layers.push(ImageLayer::new(
                ws,
                ENGRAM_SNAPSHOT_WORKING_SET_MEDIA_TYPE.to_string(),
                None,
            ));
        }

        let config = Config::new(
            payload.config_json,
            ENGRAM_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            None,
        );

        let resp: PushResponse = client
            .push(&reference, &layers, config, &auth, None)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;
        Ok(Digest256(resp.manifest_url))
    }

    /// Fetch a byte range from an OCI blob via an HTTP `Range`
    /// request. Used by [`OciChunkResolver`] (ADR 0008 Phase 2) to
    /// pull a single chunk out of a chunked OCI blob layer without
    /// downloading the entire layer.
    ///
    /// `uri` identifies the registry/repo (the OCI image URI; the
    /// tag portion is ignored for blob lookup). `blob_digest` is the
    /// OCI layer digest (`sha256:<hex>`). `offset` + `length` define
    /// the byte range; the registry returns
    /// `[offset, offset+length)`.
    ///
    /// The returned bytes are **not** verified — the OCI layer
    /// digest covers the whole blob, not arbitrary ranges, so
    /// oci-client can't verify a partial response. Callers must
    /// hash-verify against their per-chunk expectation; that's
    /// exactly what `OciChunkResolver` does using the per-chunk
    /// `sha256` from the bootstrap layer.
    ///
    /// Fails with `OciError::Distribution` if the registry doesn't
    /// honor the `Range` request (returns the full blob instead) —
    /// ADR 0008's chunked-OCI fault path requires partial responses
    /// to be viable; downloading a GB-scale chunk blob per fault is
    /// not acceptable. ECR / GAR / GHCR / Harbor are known to
    /// honor Range; some self-hosted registries don't.
    pub async fn fetch_blob_range(
        &self,
        uri: &str,
        blob_digest: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, OciError> {
        if length == 0 {
            return Ok(Bytes::new());
        }
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        // Populate the token cache for this registry/repo. Without
        // this, pull_blob_stream_partial's internal `apply_auth`
        // doesn't have a bearer token to apply.
        client
            .auth(&reference, &auth, RegistryOperation::Pull)
            .await
            .map_err(|e| OciError::Distribution(format!("auth: {e}")))?;

        let response = client
            .pull_blob_stream_partial(&reference, blob_digest, offset, Some(length))
            .await
            .map_err(|e| OciError::Distribution(format!("pull_blob_stream_partial: {e}")))?;

        let stream = match response {
            BlobResponse::Partial(s) => s,
            BlobResponse::Full(_) => {
                return Err(OciError::Distribution(format!(
                    "registry returned full blob for Range request on {blob_digest}; \
                     ADR 0008 chunked-OCI fault path requires Range support \
                     (known-good: ECR, GAR, GHCR, Harbor)"
                )));
            }
        };

        // Drain the stream into a single Bytes. Pre-size to length so
        // the registry can give us a single allocation.
        let mut buf = BytesMut::with_capacity(length as usize);
        let chunks: Vec<Bytes> = stream
            .try_collect()
            .await
            .map_err(|e| OciError::Distribution(format!("stream: {e}")))?;
        for c in chunks {
            buf.extend_from_slice(&c);
        }
        Ok(buf.freeze())
    }
}

/// `sha256:...` content digest of an OCI manifest.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Digest256(pub String);

impl Digest256 {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Strip the `sha256:` prefix and return the hex bytes. Returns
    /// the original string when no recognized prefix is present.
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

/// Result of `pull_engram_metadata`. Carries the manifest.toml
/// layer bytes plus, when present, the bundle.json layer bytes —
/// both layers are small (manifest is hundreds of bytes; bundle is
/// tens of KiB). The caller (coord's `/api/enabled-images` handler)
/// decodes each as needed.
#[derive(Clone, Debug)]
pub struct EngramMetadataLayers {
    pub manifest_toml: Vec<u8>,
    pub manifest_digest: Digest256,
    /// `None` for artifacts that pre-date ADR 0014 M1.3 (no
    /// `bundle.json` layer was attached at bake). Older bakes
    /// still enable cleanly; they just don't cascade into a
    /// templates row.
    pub bundle_json: Option<Vec<u8>>,
}

/// ADR 0014 M1.11: full in-memory view of an engram OCI artifact.
/// Returned by `OciClient::pull_template_artifacts` and consumed by
/// the coord's `enable_image` materializer. Field set parallels
/// `PulledImage` minus the disk paths; chunk-blob layers are
/// `Some` only when the bake actually emitted them.
#[derive(Clone, Debug)]
pub struct TemplateArtifacts {
    pub manifest_toml: Vec<u8>,
    pub manifest_digest: Digest256,
    pub bundle_json: Option<Vec<u8>>,
    pub disk_bootstrap_json: Option<Vec<u8>>,
    pub disk_chunks_blob: Option<Vec<u8>>,
    pub memory_bootstrap_json: Option<Vec<u8>>,
    pub memory_chunks_blob: Option<Vec<u8>>,
    pub snapshot_state: Option<Vec<u8>>,
    pub snapshot_sidecar_json: Option<Vec<u8>>,
    /// ADR 0014 M1.14: bake-time working-set trace. Coord
    /// materializes to BlobStorage at `working_set_blob_key(snapshot_id)`.
    pub snapshot_working_set_json: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct PulledImage {
    pub manifest_path: PathBuf,
    /// ADR 0007: `None` when the registry artifact carried only the
    /// `bundle.json` (chunked-storage path; rootfs bytes live in
    /// the chunk store, indexed by the bundle's `disk_manifest`).
    /// `Some(path)` for legacy or no-bundle artifacts where the
    /// rootfs.ext4 layer is the only source of disk bytes.
    pub rootfs_path: Option<PathBuf>,
    /// ADR 0007: bundle.json sidecar pointing at chunk-store
    /// manifests. Present for images baked after the chunked-
    /// storage rollout, absent for older artifacts (we still
    /// accept those during the transition window).
    pub bundle_path: Option<PathBuf>,
    /// ADR 0008 Phase 3: bootstrap layer for the disk side
    /// (Nydus-shaped chunked artifact). When `Some`, the artifact
    /// carries chunks as OCI layers and the host's tiered fault
    /// path can Range-GET them from the registry directly.
    pub disk_bootstrap_path: Option<PathBuf>,
    /// ADR 0008 Phase 3: disk chunk-blob layer digest. The actual
    /// blob bytes are *not* pulled by `pull_image` — the host fetches
    /// individual chunks via Range GET on fault. We record only the
    /// layer digest here so the runtime resolver can address the
    /// blob.
    pub disk_chunks_blob_digest: Option<String>,
    /// ADR 0008 Phase 3: optional memory bootstrap. Mirrors the
    /// disk fields; only present on bakes that captured canonical
    /// memory.
    pub memory_bootstrap_path: Option<PathBuf>,
    pub memory_chunks_blob_digest: Option<String>,
    /// ADR 0014 M1.3: canonical-snapshot `state.bin` layer pulled
    /// to disk. `Some` for artifacts produced by a bake that ran
    /// `--capture-canonical-memory`. Coord's `enable_image` reads
    /// this file and uploads it to its `BlobStorage` at the
    /// canonical `state_blob_key(snapshot_id)` so host-agents can
    /// restore by key.
    pub snapshot_state_path: Option<PathBuf>,
    /// ADR 0014 M1.3: canonical-snapshot FC sidecar `manifest.json`
    /// layer pulled to disk. Paired with `snapshot_state_path`.
    pub snapshot_sidecar_path: Option<PathBuf>,
    pub manifest_digest: Digest256,
}

/// Payload for [`OciClient::push_chunked_image`]. All-bytes shape
/// keeps the call-site obvious; the image-builder constructs this
/// after running `Bootstrap::build_from_manifest` to produce the
/// per-kind bootstrap + chunk-blob bytes.
#[derive(Clone, Debug)]
pub struct ChunkedPushPayload {
    pub manifest_toml: Vec<u8>,
    pub config_json: Vec<u8>,
    /// `bundle.json` v2 — carries the disk bootstrap/chunks layer
    /// digests so the host's `image_cache` can resolve them.
    pub bundle_json: Vec<u8>,
    pub disk_bootstrap_json: Vec<u8>,
    pub disk_chunks_blob: Vec<u8>,
    /// Optional canonical-memory side. Both must be `Some` together
    /// or neither; an asymmetric payload is rejected at push time
    /// because a memory bootstrap without its chunk blob is
    /// unconsumable.
    pub memory_bootstrap_json: Option<Vec<u8>>,
    pub memory_chunks_blob: Option<Vec<u8>>,
    /// ADR 0014 M1.3: canonical-snapshot FC state.bin layer. Same
    /// "both or neither" rule as the memory pair — state.bin alone
    /// is useless without the sidecar.
    pub snapshot_state: Option<Vec<u8>>,
    pub snapshot_sidecar_json: Option<Vec<u8>>,
    /// ADR 0014 M1.14: working-set trace produced by the bake's
    /// synthetic profile pass. Optional — falls back to full-manifest
    /// prefetch when absent.
    pub snapshot_working_set_json: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct PulledHarness {
    pub pack_dir: PathBuf,
    pub manifest_digest: Digest256,
}

#[derive(Debug)]
pub enum OciError {
    InvalidUri(String),
    Distribution(String),
    Io(std::io::Error),
}

impl std::fmt::Display for OciError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUri(s) => write!(f, "invalid OCI URI: {s}"),
            Self::Distribution(s) => write!(f, "OCI distribution error: {s}"),
            Self::Io(e) => write!(f, "OCI io: {e}"),
        }
    }
}

impl std::error::Error for OciError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OciError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---- helpers ----

fn is_loopback_host(host: &str) -> bool {
    // The registry portion may include a port. Hostnames and IPv4
    // addresses use `host:port`; IPv6 uses `[addr]:port`. Strip the
    // port if present, then compare.
    let stripped = if let Some(rest) = host.strip_prefix('[') {
        // [::1] or [::1]:5000
        rest.split(']').next().unwrap_or(rest)
    } else if host.matches(':').count() == 1 {
        // host:port (hostname or IPv4) — split off the port
        host.split(':').next().unwrap_or(host)
    } else {
        // bare IPv6 address with no port (e.g. "::1")
        host
    };
    matches!(stripped, "localhost" | "127.0.0.1" | "::1")
}

async fn read_file_bytes(path: &Path) -> Result<Vec<u8>, OciError> {
    let mut f = tokio::fs::File::open(path).await.map_err(OciError::Io)?;
    let meta = f.metadata().await.map_err(OciError::Io)?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut buf).await.map_err(OciError::Io)?;
    Ok(buf)
}

async fn write_file_bytes(path: &Path, bytes: &[u8]) -> Result<(), OciError> {
    let mut f = tokio::fs::File::create(path).await.map_err(OciError::Io)?;
    f.write_all(bytes).await.map_err(OciError::Io)?;
    f.flush().await.map_err(OciError::Io)?;
    Ok(())
}

async fn tar_gz_dir(dir: &Path) -> Result<Vec<u8>, OciError> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, std::io::Error> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut buf, Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.append_dir_all(".", &dir)?;
            tar.finish()?;
        }
        Ok(buf)
    })
    .await
    .map_err(|e| OciError::Distribution(format!("join error: {e}")))?
    .map_err(OciError::Io)
}

async fn untar_gz_to_dir(bytes: &[u8], dest: &Path) -> Result<(), OciError> {
    let bytes = bytes.to_vec();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        use flate2::read::GzDecoder;
        let dec = GzDecoder::new(std::io::Cursor::new(bytes));
        let mut tar = tar::Archive::new(dec);
        tar.set_preserve_permissions(true);
        tar.unpack(&dest)
    })
    .await
    .map_err(|e| OciError::Distribution(format!("join error: {e}")))?
    .map_err(OciError::Io)
}

/// Compute the sha256 digest of `bytes` and format as `sha256:hex`.
pub fn sha256_digest(bytes: &[u8]) -> Digest256 {
    let mut h = Sha256::new();
    h.update(bytes);
    Digest256(format!("sha256:{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_host_recognized() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("localhost:5000"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.1:5000"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("[::1]:5000"));
        assert!(!is_loopback_host("gcr.io"));
        assert!(!is_loopback_host("registry.example.com:5000"));
        assert!(!is_loopback_host("10.0.0.5:5000"));
    }

    #[test]
    fn sha256_digest_is_stable() {
        let d1 = sha256_digest(b"hello");
        let d2 = sha256_digest(b"hello");
        assert_eq!(d1, d2);
        assert!(d1.0.starts_with("sha256:"));
        assert_eq!(d1.hex().len(), 64);
    }

    #[tokio::test]
    async fn anonymous_resolver_returns_none() {
        let r = AnonymousResolver;
        assert!(r.resolve("anything").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn push_image_rejects_neither_rootfs_nor_bundle() {
        // The OCI push must always carry consumable disk bytes —
        // pushing just a manifest is a malformed artifact. Asserts
        // that the guard fires synchronously before any network I/O,
        // so a bad caller in CI doesn't depend on an unreachable
        // registry to surface the error.
        let client = OciClient::new(std::sync::Arc::new(AnonymousResolver));
        let err = client
            .push_image(
                "localhost:5000/test:t1",
                b"manifest = 'toml'",
                None,
                b"{}",
                None,
            )
            .await
            .expect_err("must reject neither-source push");
        match err {
            OciError::InvalidUri(msg) => {
                assert!(
                    msg.contains("rootfs_ext4 OR bundle_json"),
                    "expected explanatory error, got: {msg}",
                );
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_chunked_image_rejects_asymmetric_memory_payload() {
        // ADR 0008 Phase 3: a memory bootstrap without its paired
        // chunk blob (or vice versa) is malformed. Reject
        // synchronously before any registry I/O.
        let client = OciClient::new(std::sync::Arc::new(AnonymousResolver));
        let base = ChunkedPushPayload {
            manifest_toml: b"manifest = 'toml'".to_vec(),
            config_json: b"{}".to_vec(),
            bundle_json: b"{}".to_vec(),
            disk_bootstrap_json: b"{}".to_vec(),
            disk_chunks_blob: b"data".to_vec(),
            memory_bootstrap_json: None,
            memory_chunks_blob: None,
            snapshot_state: None,
            snapshot_sidecar_json: None,
            snapshot_working_set_json: None,
        };

        // Bootstrap but no blob → reject.
        let p = ChunkedPushPayload {
            memory_bootstrap_json: Some(b"{}".to_vec()),
            ..base.clone()
        };
        match client.push_chunked_image("localhost:5000/test:t1", p).await {
            Err(OciError::InvalidUri(msg)) => {
                assert!(msg.contains("memory_bootstrap"), "got: {msg}");
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }

        // Blob but no bootstrap → reject.
        let p = ChunkedPushPayload {
            memory_chunks_blob: Some(b"data".to_vec()),
            ..base
        };
        match client.push_chunked_image("localhost:5000/test:t1", p).await {
            Err(OciError::InvalidUri(msg)) => {
                assert!(msg.contains("memory_bootstrap"), "got: {msg}");
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_chunked_image_rejects_asymmetric_snapshot_payload() {
        // ADR 0014 M1.3: state.bin and sidecar.json must travel as
        // a pair. One without the other is not restorable on the
        // receiver, so reject before the registry I/O.
        let client = OciClient::new(std::sync::Arc::new(AnonymousResolver));
        let base = ChunkedPushPayload {
            manifest_toml: b"manifest = 'toml'".to_vec(),
            config_json: b"{}".to_vec(),
            bundle_json: b"{}".to_vec(),
            disk_bootstrap_json: b"{}".to_vec(),
            disk_chunks_blob: b"data".to_vec(),
            memory_bootstrap_json: None,
            memory_chunks_blob: None,
            snapshot_state: None,
            snapshot_sidecar_json: None,
            snapshot_working_set_json: None,
        };

        let p = ChunkedPushPayload {
            snapshot_state: Some(b"state".to_vec()),
            ..base.clone()
        };
        match client.push_chunked_image("localhost:5000/test:t1", p).await {
            Err(OciError::InvalidUri(msg)) => {
                assert!(msg.contains("snapshot_state"), "got: {msg}");
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }

        let p = ChunkedPushPayload {
            snapshot_sidecar_json: Some(b"{}".to_vec()),
            ..base
        };
        match client.push_chunked_image("localhost:5000/test:t1", p).await {
            Err(OciError::InvalidUri(msg)) => {
                assert!(msg.contains("snapshot_state"), "got: {msg}");
            }
            other => panic!("expected InvalidUri, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tar_gz_round_trip_preserves_files() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("harness"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(src.path().join("README"), b"a harness pack").unwrap();

        let tar_bytes = tar_gz_dir(src.path()).await.unwrap();
        let dst = tempfile::tempdir().unwrap();
        untar_gz_to_dir(&tar_bytes, dst.path()).await.unwrap();

        assert_eq!(
            std::fs::read(dst.path().join("harness")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            std::fs::read(dst.path().join("README")).unwrap(),
            b"a harness pack"
        );
    }
}
