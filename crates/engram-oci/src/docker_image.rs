//! Standard OCI/Docker image pulls (ADR 0080).
//!
//! Everything else in this crate speaks the *engram artifact* dialect
//! (custom mediaTypes, chunk layers). This module is the seam for
//! pulling a **plain** `docker build && docker push` image: resolve
//! the manifest for an explicit platform (handling manifest lists /
//! OCI indexes), then fetch the config blob and layer blobs. The
//! caller (`engram-rootfs-materializer`) owns layer *decoding* —
//! compression dispatch and the fail-loud unknown-mediaType policy
//! live there; this module just moves verified bytes.
//!
//! Reuses the crate's cached clients, auth resolver, and token cache —
//! the registry seam stays in one place.

use std::path::Path;

use futures::TryStreamExt;
use oci_client::client::BlobResponse;
use oci_client::errors::OciDistributionError;
use oci_client::manifest::{OciDescriptor, OciManifest};
use oci_client::{Client, Reference, RegistryOperation};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{pull_layer_to_vec, Digest256, OciClient, OciError};

/// One blob of a resolved docker image manifest (config or layer).
#[derive(Clone, Debug)]
pub struct DockerBlobRef {
    pub media_type: String,
    /// `sha256:<hex>` — the registry verifies pulled bytes against it.
    pub digest: String,
    pub size: u64,
}

impl DockerBlobRef {
    fn from_descriptor(d: &OciDescriptor) -> Self {
        Self {
            media_type: d.media_type.clone(),
            digest: d.digest.clone(),
            size: d.size.max(0) as u64,
        }
    }

    fn to_descriptor(&self) -> OciDescriptor {
        OciDescriptor {
            media_type: self.media_type.clone(),
            digest: self.digest.clone(),
            size: self.size as i64,
            ..Default::default()
        }
    }
}

/// A standard docker/OCI image manifest, resolved to ONE platform.
#[derive(Clone, Debug)]
pub struct DockerImageManifest {
    /// Digest of the platform-specific image manifest (not the index).
    pub manifest_digest: Digest256,
    /// The image config blob (`application/vnd.oci.image.config.v1+json`
    /// or the docker `container.image.v1+json` equivalent) — carries
    /// `Env`/`WorkingDir`.
    pub config: DockerBlobRef,
    /// Layer blobs in application order (base first).
    pub layers: Vec<DockerBlobRef>,
}

impl OciClient {
    /// Resolve `uri`'s manifest for `os`/`architecture` (e.g.
    /// `"linux"`/`"arm64"`). A single-platform image manifest is
    /// returned as-is; a manifest list / OCI index is resolved to the
    /// matching platform entry — buildx attestation entries (platform
    /// `unknown/unknown`) never match. No platform entry = a loud
    /// error listing what the index carries.
    pub async fn pull_docker_manifest(
        &self,
        uri: &str,
        os: &str,
        architecture: &str,
    ) -> Result<DockerImageManifest, OciError> {
        let reference: oci_client::Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let (manifest, digest) = client
            .pull_manifest(&reference, &auth)
            .await
            .map_err(|e| OciError::Distribution(format!("pull manifest for {uri}: {e}")))?;

        let (image, digest) = match manifest {
            OciManifest::Image(m) => (m, digest),
            OciManifest::ImageIndex(index) => {
                let entry = index
                    .manifests
                    .iter()
                    .find(|e| {
                        e.platform
                            .as_ref()
                            .is_some_and(|p| p.os == os && p.architecture == architecture)
                    })
                    .ok_or_else(|| {
                        let available: Vec<String> = index
                            .manifests
                            .iter()
                            .map(|e| {
                                e.platform
                                    .as_ref()
                                    .map(|p| format!("{}/{}", p.os, p.architecture))
                                    .unwrap_or_else(|| "<no platform>".into())
                            })
                            .collect();
                        OciError::Distribution(format!(
                            "{uri}: image index has no {os}/{architecture} entry \
                             (available: {})",
                            available.join(", ")
                        ))
                    })?;
                let pinned = reference.clone_with_digest(entry.digest.clone());
                match client.pull_manifest(&pinned, &auth).await {
                    Ok((OciManifest::Image(m), d)) => (m, d),
                    Ok((OciManifest::ImageIndex(_), _)) => {
                        return Err(OciError::Distribution(format!(
                            "{uri}: index entry {} resolved to another index (nested \
                             indexes are not supported)",
                            entry.digest
                        )));
                    }
                    Err(e) => {
                        return Err(OciError::Distribution(format!(
                            "pull platform manifest {} for {uri}: {e}",
                            entry.digest
                        )));
                    }
                }
            }
        };

        Ok(DockerImageManifest {
            manifest_digest: Digest256(digest),
            config: DockerBlobRef::from_descriptor(&image.config),
            layers: image
                .layers
                .iter()
                .map(DockerBlobRef::from_descriptor)
                .collect(),
        })
    }

    /// Pull a small blob (the image config) fully into memory,
    /// digest-verified by `oci-client`. The preceding
    /// [`Self::pull_docker_manifest`] primes the cached client's token
    /// cache (the ADR 0036 lesson: never a token mint per blob).
    pub async fn pull_docker_blob_to_vec(
        &self,
        uri: &str,
        blob: &DockerBlobRef,
    ) -> Result<Vec<u8>, OciError> {
        let reference: oci_client::Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        pull_layer_to_vec(client, &reference, &blob.to_descriptor()).await
    }

    /// Stream a (potentially multi-GB) layer blob straight to `dest` —
    /// bounded memory regardless of layer size, whole-file digest-verified
    /// before returning.
    ///
    /// **Transient-resilient (the `dev-brain` incident).** A ~60 GiB
    /// image's fat layers are long single-stream downloads; through GKE
    /// Cloud NAT they hit idle/receive resets mid-body (reqwest:
    /// `error decoding response body`). Rather than fail the layer — and
    /// with it the whole image pull, which the enable scanner scrubs and
    /// restarts from zero — this **keeps the bytes already written and
    /// resumes** with `Range: bytes=<offset>-` (a 206 appends; a registry
    /// that ignores `Range` and 200s restarts the layer from zero so we
    /// never double a prefix), up to [`BlobRetryConfig::max_attempts`]
    /// with exponential backoff (429 backs off harder). One re-auth on
    /// 401 (GHCR bearer tokens live ~5 min) doesn't consume an attempt.
    ///
    /// [`BlobRetryConfig`]: crate::BlobRetryConfig
    pub async fn pull_docker_blob_to_file(
        &self,
        uri: &str,
        blob: &DockerBlobRef,
        dest: &Path,
    ) -> Result<(), OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        let desc = blob.to_descriptor();
        self.stream_blob_resumable(client, &reference, &desc, &blob.digest, dest)
            .await
    }

    /// Stream `desc` to `dest`, retrying transient failures in place with
    /// HTTP `Range` resume. `digest` is the expected `sha256:<hex>` of the
    /// WHOLE blob; the assembled file is verified against it after the
    /// last byte lands (a running hash over the bytes actually written —
    /// covering the full assembly across all ranges, never per-range).
    async fn stream_blob_resumable(
        &self,
        client: &Client,
        reference: &Reference,
        desc: &OciDescriptor,
        digest: &str,
        dest: &Path,
    ) -> Result<(), OciError> {
        let cfg = self.blob_retry();
        let mut file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
        let mut hasher = Sha256::new();
        let mut offset: u64 = 0;
        let mut attempt: u32 = 0;
        let mut reauthed = false;

        loop {
            // offset == 0 → `Range: bytes=0-`; a range-honoring registry
            // 206s, one that can't 200s the whole body. offset > 0 is a
            // resume.
            let started = client
                .pull_blob_stream_partial(reference, desc, offset, None)
                .await;

            let mut stream = match started {
                // 206: the registry honored the range — append at `offset`.
                Ok(BlobResponse::Partial(s)) => s,
                // 200: the registry ignored `Range` and is resending from
                // byte 0. Restart the file (truncate) + hasher so a resume
                // never appends a duplicate prefix.
                Ok(BlobResponse::Full(s)) => {
                    if offset != 0 {
                        file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
                        hasher = Sha256::new();
                        offset = 0;
                    }
                    s
                }
                // Token expired mid-run — re-auth ONCE (free of the retry
                // budget) and retry the request.
                Err(e) if is_unauthorized(&e) && !reauthed => {
                    reauthed = true;
                    let auth = self.auth_for(reference).await?;
                    client
                        .auth(reference, &auth, RegistryOperation::Pull)
                        .await
                        .map_err(|e| OciError::Distribution(format!("re-auth (pull): {e}")))?;
                    continue;
                }
                Err(e) => {
                    attempt += 1;
                    if request_retryable(&e) && attempt < cfg.max_attempts {
                        tokio::time::sleep(cfg.backoff(attempt, is_rate_limited(&e))).await;
                        continue;
                    }
                    return Err(OciError::Distribution(format!(
                        "pull layer {digest} to {}: {e} (after {attempt} attempt(s))",
                        dest.display()
                    )));
                }
            };

            // Drain the body, appending to the file. A mid-stream reset
            // (the prod `error decoding response body`) leaves what we've
            // written in place; the next attempt resumes from `offset`.
            let mut stream_err = None;
            loop {
                match stream.try_next().await {
                    Ok(Some(chunk)) => {
                        hasher.update(&chunk);
                        file.write_all(&chunk).await.map_err(OciError::Io)?;
                        offset += chunk.len() as u64;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        stream_err = Some(e);
                        break;
                    }
                }
            }

            if let Some(e) = stream_err {
                attempt += 1;
                if attempt < cfg.max_attempts {
                    file.flush().await.map_err(OciError::Io)?;
                    tokio::time::sleep(cfg.backoff(attempt, false)).await;
                    continue;
                }
                return Err(OciError::Distribution(format!(
                    "pull layer {digest} to {}: mid-stream error at offset {offset} \
                     (after {attempt} attempt(s)): {e}",
                    dest.display()
                )));
            }

            // Whole body received: verify the assembled file. The running
            // hash covers exactly the bytes on disk (updated as each range
            // appended), so this is the same whole-blob guarantee as a
            // single-shot verified pull.
            file.flush().await.map_err(OciError::Io)?;
            let actual = format!("sha256:{:x}", hasher.finalize_reset());
            if actual == digest {
                return Ok(());
            }
            // Assembly doesn't match the manifest digest (a corrupt range
            // or a truncated resume the server never flagged). Restart from
            // scratch, bounded by the same attempt budget.
            attempt += 1;
            if attempt < cfg.max_attempts {
                file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
                offset = 0;
                tokio::time::sleep(cfg.backoff(attempt, false)).await;
                continue;
            }
            return Err(OciError::Distribution(format!(
                "pull layer {digest} to {}: digest mismatch after assembly \
                 (got {actual}, after {attempt} attempt(s))",
                dest.display()
            )));
        }
    }
}

/// A 401: the bearer token expired mid-run. `pull_blob_stream_partial`
/// surfaces it as `ServerError { code: 401 }`; other paths as the typed
/// `UnauthorizedError`.
fn is_unauthorized(e: &OciDistributionError) -> bool {
    matches!(
        e,
        OciDistributionError::UnauthorizedError { .. }
            | OciDistributionError::ServerError { code: 401, .. }
    )
}

/// A 429: the registry is rate-limiting. Retryable, but backs off harder.
fn is_rate_limited(e: &OciDistributionError) -> bool {
    matches!(e, OciDistributionError::ServerError { code: 429, .. })
}

/// A request-level failure worth retrying: a registry 5xx / 429, or any
/// reqwest transport error (connect / timeout / mid-body reset). A
/// definitive 4xx other than 429 is NOT retried — retrying can't fix it.
fn request_retryable(e: &OciDistributionError) -> bool {
    match e {
        OciDistributionError::ServerError { code, .. } => {
            *code == 429 || (500..=599).contains(code)
        }
        OciDistributionError::RequestError(_) => true,
        _ => false,
    }
}
