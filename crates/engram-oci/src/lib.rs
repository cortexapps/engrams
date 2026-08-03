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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer, PushResponse};
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest, OCI_IMAGE_MEDIA_TYPE};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod chunk_resolver;
pub mod docker_config;
pub mod docker_image;
pub mod media_types;

pub use chunk_resolver::{OciBlobLocator, OciChunkIndex, OciChunkResolver};
pub use docker_config::DockerConfigResolver;
pub use docker_image::{DockerBlobRef, DockerImageManifest};
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
    /// Client for HTTPS registries (everything except loopback).
    inner: Client,
    /// Client for plaintext loopback registries (`localhost:5001`).
    inner_http: Client,
    /// Plain HTTP client for the blob-existence HEAD probe (the one
    /// OCI Distribution verb `oci-client` doesn't expose).
    http: reqwest::Client,
    /// Bearer tokens for [`Self::blob_exists`]'s HEAD probes, keyed
    /// by `(registry, repository)`. Independent of `oci-client`'s
    /// internal token cache (which it doesn't expose).
    head_tokens: Arc<Mutex<HashMap<(String, String), String>>>,
    auth: Arc<dyn RegistryAuthResolver>,
    /// Per-blob transient-retry + Range-resume policy for streaming
    /// large layer blobs to disk (see
    /// [`OciClient::pull_docker_blob_to_file`]). Production default;
    /// tests dial the backoff down via [`OciClient::with_blob_retry`].
    blob_retry: BlobRetryConfig,
}

/// Transient-retry policy for streaming ONE (potentially multi-GB) blob
/// to disk with HTTP `Range` resume — see
/// [`OciClient::pull_docker_blob_to_file`]. A single flaky GB-layer
/// stream (GKE Cloud NAT resets a long single-stream download mid-body:
/// reqwest surfaces `error decoding response body`) must not fail the
/// whole image pull, so each blob retries in place, resuming from the
/// bytes already on disk rather than restarting the layer — let alone
/// the image.
#[derive(Clone, Copy, Debug)]
pub struct BlobRetryConfig {
    /// Max total attempts for one blob before giving up. A 401 re-auth
    /// does NOT consume an attempt.
    pub max_attempts: u32,
    /// Delay before the first retry; doubled each subsequent retry
    /// (1s, 2s, 4s, 8s, …), capped.
    pub base_backoff: Duration,
    /// Floor applied when the registry answers 429 — rate limiting backs
    /// off harder than a plain transport blip.
    pub rate_limit_backoff: Duration,
}

impl Default for BlobRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_backoff: Duration::from_secs(1),
            rate_limit_backoff: Duration::from_secs(5),
        }
    }
}

impl BlobRetryConfig {
    /// Backoff before the retry that follows the `attempt`-th failure
    /// (1-based). Exponential on the base; a rate-limited (429) failure
    /// never waits less than `rate_limit_backoff`.
    pub(crate) fn backoff(&self, attempt: u32, rate_limited: bool) -> Duration {
        let shift = attempt.saturating_sub(1).min(6);
        let delay = self.base_backoff.saturating_mul(1u32 << shift);
        if rate_limited {
            delay.max(self.rate_limit_backoff)
        } else {
            delay
        }
    }
}

impl OciClient {
    pub fn new(auth: Arc<dyn RegistryAuthResolver>) -> Self {
        // Two cached inner clients, one per protocol. They MUST be
        // cached on the struct rather than rebuilt per call: the
        // bearer-token cache lives inside `Client`, so a fresh client
        // per operation re-mints a token from the registry's token
        // endpoint on every call. ADR 0036: that per-chunk token
        // minting (~625 extra requests per enable) is what tripped
        // GitHub's secondary rate limiting during image enables.
        let inner = Client::new(ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        });
        let inner_http = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        Self {
            inner,
            inner_http,
            http: reqwest::Client::new(),
            head_tokens: Arc::new(Mutex::new(HashMap::new())),
            auth,
            blob_retry: BlobRetryConfig::default(),
        }
    }

    /// Override the per-blob transient-retry policy (default: 5 attempts,
    /// 1s exponential backoff, 5s floor on 429). The materializer never
    /// sets this — production runs on the default; tests dial it down so
    /// the exhaustion path doesn't sleep for real seconds.
    pub fn with_blob_retry(mut self, cfg: BlobRetryConfig) -> Self {
        self.blob_retry = cfg;
        self
    }

    /// The active per-blob retry policy (used by `docker_image.rs`).
    pub(crate) fn blob_retry(&self) -> BlobRetryConfig {
        self.blob_retry
    }

    /// The cached client whose protocol matches `reference`'s registry
    /// host: `Http` for loopback, `Https` otherwise. Plaintext is the
    /// wrong default everywhere except dev.
    fn client_for(&self, reference: &Reference) -> &Client {
        if is_loopback_host(reference.registry()) {
            &self.inner_http
        } else {
            &self.inner
        }
    }

    async fn auth_for(&self, reference: &Reference) -> Result<RegistryAuth, OciError> {
        match self.auth.resolve(reference.registry()).await? {
            Some(c) => Ok(RegistryAuth::Basic(c.username, c.password)),
            None => Ok(RegistryAuth::Anonymous),
        }
    }

    /// Push a bake image artifact. Layers (ADR 0080: no manifest.toml —
    /// the bake carries no runtime config; the config blob carries the
    /// Dockerfile-derived `runtime_defaults`):
    ///
    /// - layer 0 (optional): `rootfs.ext4` content (raw bytes,
    ///   large). Skipped when `bundle_json` is `Some` — ADR 0007
    ///   chunked storage moves disk bytes into the chunk store, so
    ///   pushing the full ext4 in the OCI layer is wire-redundant
    ///   (every push would otherwise transfer the disk twice:
    ///   chunks via BlobStorage, then bytes via the registry).
    /// - layer 1 (optional): `bundle.json` — ADR 0007 chunk-manifest
    ///   pointer set (tiny). Pullers that understand the bundle
    ///   resolve disk bytes from the chunk store via its
    ///   `disk_manifest`.
    ///
    /// At least one of `rootfs_ext4` or `bundle_json` must be
    /// provided — a layer-less push has no consumable disk
    /// bytes, and we return a typed error so callers can't push
    /// half-formed artifacts.
    ///
    /// `config_json` is small JSON metadata (format, repo/tag, and — ADR
    /// 0080 — `runtime_defaults`, the Dockerfile ENV/WORKDIR the enable
    /// pipeline persists).
    pub async fn push_image(
        &self,
        uri: &str,
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
        let client = self.client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let mut layers = Vec::new();
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

    /// Pull a bake image artifact's *metadata* layers to disk:
    /// `<dest>/manifest.toml`, plus `bundle.json` /
    /// `bootstrap.disk.json` / `rootfs.ext4` when the artifact
    /// carries them. Returns the digest of the OCI manifest (a
    /// content address — a re-pull of the same tag with unchanged
    /// layers yields the same digest).
    ///
    /// ADR 0036: descriptor-driven. Chunk layers
    /// ([`ENGRAM_CHUNK_MEDIA_TYPE`]) are **never** downloaded here —
    /// a chunked image carries one OCI blob per 16 MiB chunk and the
    /// runtime fetches individual chunks on fault via
    /// [`Self::pull_chunk`]. The legacy `rootfs.ext4` layer (dev-only
    /// non-chunked bakes) streams straight to disk rather than
    /// through memory.
    pub async fn pull_image(&self, uri: &str, dest: &Path) -> Result<PulledImage, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        // Manifest only — layer descriptors, no bodies. Also primes
        // the client's token cache for the per-layer pulls below.
        let (manifest, manifest_digest) = client
            .pull_image_manifest(&reference, &auth)
            .await
            .map_err(|e| OciError::Distribution(format!("pull manifest for {uri}: {e}")))?;

        tokio::fs::create_dir_all(dest)
            .await
            .map_err(OciError::Io)?;

        let mut rootfs_path = None;
        let mut bundle_path = None;
        let mut disk_bootstrap_path = None;
        for desc in &manifest.layers {
            match desc.media_type.as_str() {
                ENGRAM_ROOTFS_EXT4_MEDIA_TYPE => {
                    let p = dest.join("rootfs.ext4");
                    pull_layer_to_file(client, &reference, desc, &p).await?;
                    rootfs_path = Some(p);
                }
                ENGRAM_BUNDLE_MEDIA_TYPE => {
                    let p = dest.join("bundle.json");
                    pull_layer_to_file(client, &reference, desc, &p).await?;
                    bundle_path = Some(p);
                }
                ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE => {
                    let p = dest.join("bootstrap.disk.json");
                    pull_layer_to_file(client, &reference, desc, &p).await?;
                    disk_bootstrap_path = Some(p);
                }
                ENGRAM_CHUNK_MEDIA_TYPE => {
                    // Chunks are fetched individually on fault
                    // (tiered resolver) or at enable-time materialize
                    // — never as part of a metadata pull.
                }
                other => {
                    tracing::debug!(media_type = %other, "skipping unrecognized layer");
                }
            }
        }
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
            rootfs_path,
            bundle_path,
            disk_bootstrap_path,
            manifest_digest: Digest256(manifest_digest),
        })
    }

    // ADR 0080 phase 3b: `pull_template_metadata` (the coordinator's
    // engram-artifact metadata pull) retired with the enable pipeline's
    // switch to host-side materialization of STANDARD docker images —
    // see `pull_docker_manifest` (docker_image.rs). The artifact PUSH
    // verbs below survive until phase 4 retires `engram-cli image build`.

    /// Push a harness pack artifact. Tars + gzips `pack_dir` and pushes
    /// it as a single layer.
    pub async fn push_harness(&self, uri: &str, pack_dir: &Path) -> Result<Digest256, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
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
        let client = self.client_for(&reference);
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

    /// Prime the cached client's bearer-token cache for **push**
    /// operations against `uri`'s registry/repo. Call once per push
    /// run before [`Self::blob_exists`] / [`Self::push_chunk_blob`] /
    /// [`Self::push_chunked_image_manifest`] — `oci-client`'s
    /// `apply_auth` only *reads* its token cache; it never mints.
    pub async fn auth_for_push(&self, uri: &str) -> Result<(), OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        let auth = self.auth_for(&reference).await?;
        client
            .auth(&reference, &auth, RegistryOperation::Push)
            .await
            .map_err(|e| OciError::Distribution(format!("auth (push): {e}")))?;
        Ok(())
    }

    /// HEAD `/v2/<repo>/blobs/<digest>` — does the registry already
    /// have this blob? The delta-push primitive (ADR 0036): the bake
    /// probes every chunk digest and uploads only the missing ones,
    /// exactly like `docker push` skips layers the registry has.
    ///
    /// `oci-client` doesn't expose HEAD-blob, so this speaks the
    /// token dance directly: HEAD → 401 + `WWW-Authenticate` → mint
    /// a bearer token (Basic creds from the resolver when present) →
    /// retry. Tokens are cached per `(registry, repo)` on the client,
    /// so a 625-chunk sweep costs one mint.
    pub async fn blob_exists(&self, uri: &str, digest: &str) -> Result<bool, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let registry = reference.registry().to_string();
        let repo = reference.repository().to_string();
        let scheme = if is_loopback_host(&registry) {
            "http"
        } else {
            "https"
        };
        let url = format!("{scheme}://{registry}/v2/{repo}/blobs/{digest}");

        let cached = self
            .head_tokens
            .lock()
            .get(&(registry.clone(), repo.clone()))
            .cloned();
        let mut req = self.http.head(&url);
        if let Some(tok) = &cached {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OciError::Distribution(format!("HEAD {url}: {e}")))?;

        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    OciError::Distribution(format!("HEAD {url}: 401 without WWW-Authenticate"))
                })?
                .to_string();
            let token = self.mint_head_token(&registry, &challenge).await?;
            self.head_tokens
                .lock()
                .insert((registry.clone(), repo.clone()), token.clone());
            self.http
                .head(&url)
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| OciError::Distribution(format!("HEAD {url} (authed): {e}")))?
        } else {
            resp
        };

        match resp.status() {
            s if s.is_success() => Ok(true),
            reqwest::StatusCode::NOT_FOUND => Ok(false),
            s => Err(OciError::Distribution(format!(
                "HEAD {url}: unexpected status {s}"
            ))),
        }
    }

    /// Mint a bearer token from the realm advertised in a
    /// `WWW-Authenticate: Bearer realm="…",service="…",scope="…"`
    /// challenge, using Basic creds from the resolver when present.
    async fn mint_head_token(&self, registry: &str, challenge: &str) -> Result<String, OciError> {
        let params = parse_bearer_challenge(challenge).ok_or_else(|| {
            OciError::Distribution(format!(
                "unparseable WWW-Authenticate challenge from {registry}: {challenge}"
            ))
        })?;
        let mut req = self.http.get(&params.realm);
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(service) = &params.service {
            query.push(("service", service));
        }
        if let Some(scope) = &params.scope {
            query.push(("scope", scope));
        }
        req = req.query(&query);
        if let Some(creds) = self.auth.resolve(registry).await? {
            req = req.basic_auth(creds.username, Some(creds.password));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OciError::Distribution(format!("token mint from {}: {e}", params.realm)))?
            .error_for_status()
            .map_err(|e| {
                OciError::Distribution(format!("token mint from {}: {e}", params.realm))
            })?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| OciError::Distribution(format!("token body: {e}")))?;
        body.get("token")
            .or_else(|| body.get("access_token"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                OciError::Distribution(format!(
                    "token response from {} has neither `token` nor `access_token`",
                    params.realm
                ))
            })
    }

    /// Push one content-addressed chunk blob (ADR 0036). `digest`
    /// MUST be `sha256:` over `bytes` — the registry verifies it at
    /// upload, which is exactly the property that makes the chunk
    /// hash and the OCI digest the same address.
    ///
    /// Call [`Self::auth_for_push`] once before a push run to avoid
    /// a thundering herd of first-401 re-auths from concurrent
    /// pushes; mid-run token expiry (GHCR bearer tokens live ~5 min;
    /// big pushes run longer) is handled here by a re-auth + retry.
    /// Skip-if-present is the caller's job via [`Self::blob_exists`].
    pub async fn push_chunk_blob(
        &self,
        uri: &str,
        digest: &str,
        bytes: &[u8],
    ) -> Result<(), OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        self.push_blob_reauth(client, &reference, bytes, digest)
            .await
    }

    /// `push_blob` with one re-auth + retry on 401 — the bearer
    /// token minted at the start of a push run expires mid-run for
    /// large images. `oci-client`'s `apply_auth` only reads its
    /// token cache (it never re-mints), so expiry must be handled
    /// at this layer.
    async fn push_blob_reauth(
        &self,
        client: &Client,
        reference: &Reference,
        bytes: &[u8],
        digest: &str,
    ) -> Result<(), OciError> {
        match client.push_blob(reference, bytes, digest).await {
            Ok(_) => Ok(()),
            Err(oci_client::errors::OciDistributionError::UnauthorizedError { .. }) => {
                let auth = self.auth_for(reference).await?;
                client
                    .auth(reference, &auth, RegistryOperation::Push)
                    .await
                    .map_err(|e| OciError::Distribution(format!("re-auth (push): {e}")))?;
                client
                    .push_blob(reference, bytes, digest)
                    .await
                    .map(|_| ())
                    .map_err(|e| {
                        OciError::Distribution(format!("push blob {digest} (re-authed): {e}"))
                    })
            }
            Err(e) => Err(OciError::Distribution(format!("push blob {digest}: {e}"))),
        }
    }

    /// Push the manifest of a chunked image artifact (ADR 0036):
    /// the small metadata layers (pushed here as blobs) plus one
    /// layer descriptor per chunk (whose blobs the caller already
    /// pushed via [`Self::push_chunk_blob`] / skipped via
    /// [`Self::blob_exists`]).
    ///
    /// Layer order: manifest.toml, bundle.json, bootstrap.disk.json,
    /// then every chunk in bootstrap-entry order. Consumers dispatch
    /// on mediaType, not position.
    pub async fn push_chunked_image_manifest(
        &self,
        uri: &str,
        layers: ChunkedImageLayers,
        chunks: &[ChunkLayerRef],
    ) -> Result<Digest256, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);

        // Push the small layers + config as blobs, collecting
        // descriptors as we go.
        let mut descriptors = Vec::with_capacity(2 + chunks.len());
        for (bytes, media_type) in [
            (&layers.bundle_json, ENGRAM_BUNDLE_MEDIA_TYPE),
            (
                &layers.disk_bootstrap_json,
                ENGRAM_BOOTSTRAP_DISK_MEDIA_TYPE,
            ),
        ] {
            let digest = sha256_digest(bytes);
            self.push_blob_reauth(client, &reference, bytes, digest.as_str())
                .await?;
            descriptors.push(OciDescriptor {
                media_type: media_type.to_string(),
                digest: digest.0,
                size: bytes.len() as i64,
                ..Default::default()
            });
        }
        for c in chunks {
            descriptors.push(OciDescriptor {
                media_type: ENGRAM_CHUNK_MEDIA_TYPE.to_string(),
                digest: c.digest.clone(),
                size: c.size as i64,
                ..Default::default()
            });
        }

        let config_digest = sha256_digest(&layers.config_json);
        self.push_blob_reauth(
            client,
            &reference,
            &layers.config_json,
            config_digest.as_str(),
        )
        .await?;

        let manifest = OciImageManifest {
            media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
            config: OciDescriptor {
                media_type: ENGRAM_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
                digest: config_digest.0,
                size: layers.config_json.len() as i64,
                ..Default::default()
            },
            layers: descriptors,
            ..Default::default()
        };
        let url = match client
            .push_manifest(&reference, &OciManifest::Image(manifest.clone()))
            .await
        {
            Ok(url) => url,
            Err(oci_client::errors::OciDistributionError::UnauthorizedError { .. }) => {
                let auth = self.auth_for(&reference).await?;
                client
                    .auth(&reference, &auth, RegistryOperation::Push)
                    .await
                    .map_err(|e| OciError::Distribution(format!("re-auth (push): {e}")))?;
                client
                    .push_manifest(&reference, &OciManifest::Image(manifest))
                    .await
                    .map_err(|e| {
                        OciError::Distribution(format!("push manifest (re-authed): {e}"))
                    })?
            }
            Err(e) => return Err(OciError::Distribution(format!("push manifest: {e}"))),
        };
        Ok(Digest256(url))
    }

    /// Pull one chunk blob by digest (ADR 0036). `oci-client`
    /// verifies the response body hashes to `digest` before this
    /// returns, so the bytes are trustworthy as-is; callers that
    /// address chunks by `ChunkHash` get verify-on-fetch for free
    /// because the OCI digest *is* the chunk hash.
    ///
    /// Auth is lazy: the first pull (or one whose bearer token
    /// expired mid-run) hits a 401, re-auths once, and retries.
    /// Crucially the token then lives in the **cached** client's
    /// token cache, so subsequent pulls are a single GET — not the
    /// fresh-client-per-chunk token-mint storm that tripped GitHub's
    /// rate limiting (ADR 0036).
    ///
    /// `size` pre-sizes the buffer (the bootstrap entry's `length`).
    pub async fn pull_chunk(&self, uri: &str, digest: &str, size: u64) -> Result<Bytes, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);

        let desc = OciDescriptor {
            media_type: ENGRAM_CHUNK_MEDIA_TYPE.to_string(),
            digest: digest.to_string(),
            size: size as i64,
            ..Default::default()
        };
        let mut buf = Vec::with_capacity(size as usize);
        match client.pull_blob(&reference, &desc, &mut buf).await {
            Ok(()) => {}
            Err(oci_client::errors::OciDistributionError::UnauthorizedError { .. }) => {
                let auth = self.auth_for(&reference).await?;
                client
                    .auth(&reference, &auth, RegistryOperation::Pull)
                    .await
                    .map_err(|e| OciError::Distribution(format!("re-auth (pull): {e}")))?;
                buf.clear();
                client
                    .pull_blob(&reference, &desc, &mut buf)
                    .await
                    .map_err(|e| {
                        OciError::Distribution(format!("pull chunk {digest} (re-authed): {e}"))
                    })?;
            }
            Err(e) => {
                return Err(OciError::Distribution(format!("pull chunk {digest}: {e}")));
            }
        }
        Ok(Bytes::from(buf))
    }
}

/// Parameters of a `WWW-Authenticate: Bearer …` challenge.
struct BearerChallengeParams {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

/// Parse `Bearer realm="…",service="…",scope="…"` (any order,
/// quoted values). Returns `None` for non-Bearer or realm-less
/// challenges.
fn parse_bearer_challenge(challenge: &str) -> Option<BearerChallengeParams> {
    let rest = challenge.trim().strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "realm" => realm = Some(v),
            "service" => service = Some(v),
            "scope" => scope = Some(v),
            _ => {}
        }
    }
    Some(BearerChallengeParams {
        realm: realm?,
        service,
        scope,
    })
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

/// Rewrite an image URI to pin it at a specific manifest `digest`,
/// dropping any tag it carried (`<reg>/<repo>:tag` → `<reg>/<repo>@sha256:…`).
///
/// Issue #192: a **mutable** tag (`:latest`) is unsafe as a cache key.
/// Anything that materialises bytes from a tag — most importantly the
/// host's tag-keyed local OCI cache (`ImageCache`) — can serve a STALE
/// previous bake even after the coord resolved a fresh `manifest_digest`
/// from the registry HEAD. Pinning the reference by digest before it
/// reaches the host makes the cache key content-addressed, so a moving
/// tag can never produce stale layers. The digest is content-addressed,
/// so the registry returns exactly the bytes the coord resolved.
///
/// Returns the original URI unchanged if it can't be parsed as an OCI
/// reference (the caller's later pull surfaces the real error with full
/// context) — we never want digest-pinning to be the thing that fails an
/// otherwise-valid enable.
pub fn digest_pinned_uri(image_uri: &str, digest: &Digest256) -> String {
    match image_uri.parse::<Reference>() {
        Ok(reference) => reference
            .clone_with_digest(digest.as_str().to_string())
            .whole(),
        Err(_) => image_uri.to_string(),
    }
}

/// The registry host of an image reference (`ghcr.io/a/b:t` →
/// `"ghcr.io"`, `alpine:3` → the docker.io default). ADR 0080 phase
/// 3b: the coordinator keys `registry_credentials` rows on this to
/// resolve static creds it ships in `MaterializeImage`. One place, the
/// same `oci_client::Reference` normalization every pull uses —
/// callers must never re-implement registry parsing.
pub fn registry_host(image_uri: &str) -> Result<String, OciError> {
    let reference: Reference = image_uri
        .parse()
        .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{image_uri}: {e}")))?;
    Ok(reference.registry().to_string())
}

// ADR 0080 phase 3b: `TemplateArtifacts` retired with
// `pull_template_metadata` — the enable pipeline consumes STANDARD
// docker images via `pull_docker_manifest` + the host-side
// `MaterializeImage` RPC; old engram artifacts can no longer be
// enabled (clean break; existing enabled rows keep working, their
// chunks are already in BlobStorage).

#[derive(Clone, Debug)]
pub struct PulledImage {
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
    /// ADR 0008 Phase 3 / ADR 0036: bootstrap layer for the disk
    /// side. When `Some`, the artifact carries one OCI blob per
    /// chunk and the host's tiered fault path can pull individual
    /// chunks from the registry directly (the bootstrap entries
    /// carry each chunk's blob digest).
    pub disk_bootstrap_path: Option<PathBuf>,
    pub manifest_digest: Digest256,
}

/// Small (non-chunk) layers of a chunked image artifact (ADR 0036),
/// passed to [`OciClient::push_chunked_image_manifest`]. All a few
/// KB; chunk bytes never ride through this struct — they're pushed
/// individually via [`OciClient::push_chunk_blob`].
#[derive(Clone, Debug)]
pub struct ChunkedImageLayers {
    pub config_json: Vec<u8>,
    /// `bundle.json` v2 — carries the bake's `disk_manifest` ref so
    /// consumers can resolve chunks through the chunk store.
    pub bundle_json: Vec<u8>,
    pub disk_bootstrap_json: Vec<u8>,
}

/// One chunk layer of a chunked image artifact (ADR 0036): its OCI
/// blob digest (`sha256:<chunk-hash>`) and size in bytes. The
/// manifest push records one descriptor per entry; the blobs
/// themselves were pushed (or HEAD-skipped) beforehand.
#[derive(Clone, Debug)]
pub struct ChunkLayerRef {
    pub digest: String,
    pub size: u64,
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

/// Pull one (small) OCI layer fully into a Vec. Used only for the
/// metadata layers in `pull_template_metadata` — never chunk layers,
/// which are pulled individually on demand via `pull_chunk`.
async fn pull_layer_to_vec(
    client: &Client,
    reference: &Reference,
    desc: &oci_client::manifest::OciDescriptor,
) -> Result<Vec<u8>, OciError> {
    use futures::TryStreamExt;
    let stream = client
        .pull_blob_stream(reference, desc)
        .await
        .map_err(|e| OciError::Distribution(format!("pull layer {}: {e}", desc.digest)))?;
    let parts: Vec<Bytes> = stream
        .try_collect()
        .await
        .map_err(|e| OciError::Distribution(format!("read layer {}: {e}", desc.digest)))?;
    let mut buf = Vec::with_capacity(desc.size.max(0) as usize);
    for p in parts {
        buf.extend_from_slice(&p);
    }
    Ok(buf)
}

/// Pull one OCI layer straight to a file, streaming — bounded
/// memory regardless of layer size (the legacy `rootfs.ext4` layer
/// can be multi-GB). `pull_blob` verifies the bytes against the
/// descriptor digest before this returns.
async fn pull_layer_to_file(
    client: &Client,
    reference: &Reference,
    desc: &oci_client::manifest::OciDescriptor,
    dest: &Path,
) -> Result<(), OciError> {
    let mut file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
    client
        .pull_blob(reference, desc, &mut file)
        .await
        .map_err(|e| {
            OciError::Distribution(format!(
                "pull layer {} to {}: {e}",
                desc.digest,
                dest.display()
            ))
        })?;
    file.flush().await.map_err(OciError::Io)?;
    Ok(())
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
    Digest256(format!("sha256:{}", hex::encode(h.finalize())))
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

    // A realistic, well-formed sha256 digest (64 hex chars). The OCI
    // `Reference` parser validates the digest shape, so the tests below
    // must use real-length digests, not toy strings.
    const FRESH: &str = "sha256:6499e6c6abcdef0123456789abcdef0123456789abcdef0123456789abcdef01";
    const PREV: &str = "sha256:c9a760b200000000000000000000000000000000000000000000000000000000";

    #[test]
    fn digest_pinned_uri_replaces_tag() {
        // Issue #192: a mutable tag must be swapped for the resolved
        // digest so the host's tag-keyed OCI cache can't serve stale
        // bytes. The result drops the tag and pins by digest.
        let d = Digest256(FRESH.to_string());
        assert_eq!(
            digest_pinned_uri("ghcr.io/cortexapps/engrams/demo:latest", &d),
            format!("ghcr.io/cortexapps/engrams/demo@{FRESH}"),
        );
    }

    #[test]
    fn digest_pinned_uri_adds_digest_to_untagged() {
        let d = Digest256(FRESH.to_string());
        assert_eq!(
            digest_pinned_uri("ghcr.io/cortexapps/engrams/demo", &d),
            format!("ghcr.io/cortexapps/engrams/demo@{FRESH}"),
        );
    }

    #[test]
    fn digest_pinned_uri_overrides_existing_digest() {
        // Already-pinned references re-pin to the freshly-resolved
        // digest (idempotent for the common no-op refresh case).
        let d = Digest256(FRESH.to_string());
        assert_eq!(
            digest_pinned_uri(&format!("ghcr.io/cortexapps/engrams/demo@{PREV}"), &d),
            format!("ghcr.io/cortexapps/engrams/demo@{FRESH}"),
        );
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
            .push_image("localhost:5000/test:t1", None, b"{}", None)
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
