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

use oci_client::manifest::{OciDescriptor, OciManifest};

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

    /// Stream a (potentially large) layer blob straight to `dest` —
    /// bounded memory regardless of layer size, digest-verified before
    /// returning. One re-auth + retry on 401: the bearer token minted
    /// at the manifest pull expires mid-run for big multi-layer images
    /// (GHCR tokens live ~5 min).
    pub async fn pull_docker_blob_to_file(
        &self,
        uri: &str,
        blob: &DockerBlobRef,
        dest: &Path,
    ) -> Result<(), OciError> {
        let reference: oci_client::Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = self.client_for(&reference);
        let desc = blob.to_descriptor();

        let mut file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
        match client.pull_blob(&reference, &desc, &mut file).await {
            Ok(()) => {}
            Err(oci_client::errors::OciDistributionError::UnauthorizedError { .. }) => {
                let auth = self.auth_for(&reference).await?;
                client
                    .auth(&reference, &auth, oci_client::RegistryOperation::Pull)
                    .await
                    .map_err(|e| OciError::Distribution(format!("re-auth (pull): {e}")))?;
                // Recreate: the failed attempt may have written bytes.
                file = tokio::fs::File::create(dest).await.map_err(OciError::Io)?;
                client
                    .pull_blob(&reference, &desc, &mut file)
                    .await
                    .map_err(|e| {
                        OciError::Distribution(format!(
                            "pull layer {} (re-authed): {e}",
                            blob.digest
                        ))
                    })?;
            }
            Err(e) => {
                return Err(OciError::Distribution(format!(
                    "pull layer {} to {}: {e}",
                    blob.digest,
                    dest.display()
                )));
            }
        }
        use tokio::io::AsyncWriteExt;
        file.flush().await.map_err(OciError::Io)?;
        Ok(())
    }
}
