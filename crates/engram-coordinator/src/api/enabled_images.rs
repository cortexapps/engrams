//! `/api/enabled-images` — operator-curated allow-list of OCI image
//! URIs that sessions may reference.
//!
//! Stage C: this is the single source of truth for "what images can a
//! session use." Postgres stores the URI plus a snapshot of the
//! manifest.toml fetched at enable time, so `POST /sessions` resolves
//! the manifest from a Postgres row without going to the registry on
//! the hot path. The dashboard's image picker reads from here.
//!
//! Verbs:
//! - `POST /api/enabled-images   { image_uri }` — enable an image:
//!   pull just the manifest layer from the registry (auth via
//!   `registry_credentials`), parse it as engram TOML to validate,
//!   upsert. Returns the new row.
//! - `GET  /api/enabled-images`                — list enabled rows.
//! - `POST /api/enabled-images/refresh { image_uri }` — re-fetch the
//!   manifest from the registry and update `manifest_toml` +
//!   `manifest_digest`. Useful when an image tag is moved.
//! - `POST /api/enabled-images/disable { image_uri }` — remove the
//!   row. Sessions referencing the image fail at the next create.
//!
//! URI as path-segment: image URIs contain `/` and `:`, which axum's
//! Path extractor will URL-decode but the dashboard's HTTP client
//! would have to encode. POSTing the URI in the body keeps both ends
//! simple and avoids the bikeshed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::session::split_image_ref;
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::types::template::TemplateRecord;
use engram_core::types::{EnabledImage, EnabledImageSummary, ImageManifest};
use engram_core::{MetaError, TemplateRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct EnableImageRequest {
    /// Full OCI URI: `<host>[/path]/<repo>:<tag>`. The host portion
    /// must match a row in `registry_credentials` for non-anonymous
    /// pulls; missing rows fall through to anonymous (works for
    /// public registries and `localhost:5001`).
    pub image_uri: String,
}

#[derive(Deserialize)]
pub struct ImageUriRequest {
    pub image_uri: String,
}

#[derive(Serialize)]
pub struct ListEnabledImagesResponse {
    pub images: Vec<EnabledImageSummary>,
}

pub async fn enable_image(
    State(state): State<SharedState>,
    Json(req): Json<EnableImageRequest>,
) -> Result<(StatusCode, Json<EnabledImageSummary>), ApiError> {
    if req.image_uri.trim().is_empty() {
        return Err(ApiError::BadRequest("image_uri must not be empty".into()));
    }

    let (row, manifest, bundle_canonical_snapshot, artifacts) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    state
        .services
        .meta
        .upsert_enabled_image(row.clone())
        .await?;

    // ADR 0014 M1.11: when the bundle carries a canonical_snapshot,
    // materialize the bake's OCI artifact into the deployment's
    // BlobStorage (state.bin, sidecar, memory + disk chunks +
    // manifests at canonical keys), then cascade into snapshots +
    // templates. Both have to succeed for the warm pool to actually
    // refill — a templates row pointing at a snapshot whose chunks
    // aren't in BlobStorage is the regression we just fixed.
    if let Some(snapshot) = bundle_canonical_snapshot {
        let materialized = materialize_template_artifacts(&state, snapshot, &artifacts).await;
        match materialized {
            Ok(snapshot) => {
                if let Err(e) =
                    cascade_into_templates(&state, &req.image_uri, &manifest, snapshot).await
                {
                    tracing::warn!(
                        image_uri = %req.image_uri,
                        error = %e,
                        "enabled image; templates cascade failed after materialization \
                         (warm pool will not fire for this image)",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    image_uri = %req.image_uri,
                    error = %e,
                    "enabled image; OCI → BlobStorage materialization failed \
                     (image is enabled but warm pool will not fire)",
                );
            }
        }
    }

    Ok((StatusCode::CREATED, Json(EnabledImageSummary::from(row))))
}

pub async fn list_enabled_images(
    State(state): State<SharedState>,
) -> Result<Json<ListEnabledImagesResponse>, ApiError> {
    let rows = state.services.meta.list_enabled_images().await?;
    let images = rows.into_iter().map(EnabledImageSummary::from).collect();
    Ok(Json(ListEnabledImagesResponse { images }))
}

pub async fn refresh_enabled_image(
    State(state): State<SharedState>,
    Json(req): Json<ImageUriRequest>,
) -> Result<Json<EnabledImageSummary>, ApiError> {
    if req.image_uri.trim().is_empty() {
        return Err(ApiError::BadRequest("image_uri must not be empty".into()));
    }

    // Refresh = "this URI must already be enabled, re-pull and update."
    // We require the row to exist so an operator can't accidentally
    // enable something via /refresh that they didn't enable via the
    // explicit POST path (which is the place that's auditable).
    let existing = state
        .services
        .meta
        .get_enabled_image(&req.image_uri)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "image `{}` is not enabled; call POST /api/enabled-images first",
                req.image_uri
            ))
        })?;

    let (mut refreshed, manifest, bundle_canonical_snapshot, artifacts) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    refreshed.id = existing.id;
    refreshed.created_at = existing.created_at;
    refreshed.updated_at = Some(Utc::now());

    state
        .services
        .meta
        .upsert_enabled_image(refreshed.clone())
        .await?;

    // ADR 0014 M1.11: refresh re-runs the full pipeline —
    // materialize the new artifact's chunks/state/sidecar into
    // BlobStorage, then cascade. upsert_template's internal
    // transaction flips the prior active row to false so warm-pool
    // hosts pick up the new snapshot on the next heartbeat-ack.
    // Content-addressed chunks dedup against the prior bake
    // automatically — only genuinely new bytes hit the wire.
    if let Some(snapshot) = bundle_canonical_snapshot {
        let materialized = materialize_template_artifacts(&state, snapshot, &artifacts).await;
        match materialized {
            Ok(snapshot) => {
                if let Err(e) =
                    cascade_into_templates(&state, &req.image_uri, &manifest, snapshot).await
                {
                    tracing::warn!(
                        image_uri = %req.image_uri,
                        error = %e,
                        "refreshed image; templates cascade failed (warm pool will not refill new snapshot)",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    image_uri = %req.image_uri,
                    error = %e,
                    "refreshed image; OCI → BlobStorage materialization failed \
                     (warm pool will not refill new snapshot)",
                );
            }
        }
    }

    Ok(Json(EnabledImageSummary::from(refreshed)))
}

pub async fn disable_enabled_image(
    State(state): State<SharedState>,
    Json(req): Json<ImageUriRequest>,
) -> Result<StatusCode, ApiError> {
    match state
        .services
        .meta
        .delete_enabled_image(&req.image_uri)
        .await
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(MetaError::NotFound) => Err(ApiError::NotFound(format!(
            "image `{}` is not enabled",
            req.image_uri
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Pull the full engram OCI artifact at `image_uri` and validate
/// it. Returns:
///   - the `EnabledImage` row ready to upsert,
///   - the parsed `ImageManifest` (so the caller can read resource
///     hints for the templates cascade),
///   - the bundle's `canonical_snapshot` block if present (ADR
///     0014 M1.11 cascade input),
///   - the full `TemplateArtifacts` (state.bin, sidecar, chunks
///     blobs, bootstrap layers) — the materializer downstream
///     re-shards these into the deployment's BlobStorage so prod
///     host-agents can restore by canonical key.
///
/// Bundle.json is optional: artifacts baked before ADR 0014 M1.3
/// don't carry it. We still enable the image; the cascade is skipped.
async fn fetch_and_seal_manifest(
    state: &SharedState,
    image_uri: &str,
) -> Result<
    (
        EnabledImage,
        ImageManifest,
        Option<SnapshotMetadata>,
        engram_oci::TemplateArtifacts,
    ),
    ApiError,
> {
    let artifacts = state
        .services
        .oci
        .pull_template_artifacts(image_uri)
        .await
        .map_err(|e| {
            ApiError::BadRequest(format!(
                "registry pull for `{image_uri}` failed: {e}. \
                 Check that the URI is correct and that a matching \
                 registry credential exists if the registry requires auth."
            ))
        })?;

    let manifest_toml = String::from_utf8(artifacts.manifest_toml.clone()).map_err(|e| {
        ApiError::BadRequest(format!(
            "manifest layer for `{image_uri}` is not valid UTF-8: {e}"
        ))
    })?;

    let manifest: ImageManifest = toml::from_str(&manifest_toml).map_err(|e| {
        ApiError::BadRequest(format!(
            "manifest.toml at `{image_uri}` failed to parse as engram ImageManifest: {e}"
        ))
    })?;

    let canonical_snapshot = artifacts.bundle_json.as_deref().and_then(|bytes| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|root| root.get("canonical_snapshot").cloned())
            .and_then(|snap| serde_json::from_value::<SnapshotMetadata>(snap).ok())
    });

    let now = Utc::now();
    let row = EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.to_string(),
        manifest_toml,
        manifest_digest: artifacts.manifest_digest.as_str().to_string(),
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
    };
    Ok((row, manifest, canonical_snapshot, artifacts))
}

/// ADR 0014: persist a chunk-blob into BlobStorage at the content-
/// addressed key for each chunk in `bootstrap`, plus the recon-
/// structed `Manifest` at `manifest_ref`. Content-addressing means
/// chunks present from a prior bake are skipped — only genuinely
/// new bytes hit the wire. Bounded-parallel exists+put fans out so
/// the materialization isn't a thousand-deep sequential round-trip.
///
/// Returns the chunk counts so the caller can surface a "wrote
/// 47 of 1024 chunks (rest deduplicated)" status line.
async fn materialize_chunk_blob(
    blob: &dyn engram_core::traits::BlobStorage,
    chunk_store: &engram_chunk_store::ChunkStore,
    manifest_ref: engram_core::types::manifest::ManifestRef,
    bootstrap_json: &[u8],
    chunks_blob: &[u8],
) -> Result<(usize, usize), ApiError> {
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(bootstrap_json)
        .map_err(|e| ApiError::Internal(format!("parse bootstrap json: {e}")))?;
    let manifest = bootstrap.to_manifest();

    use futures::stream::{FuturesUnordered, StreamExt};
    let mut tasks = FuturesUnordered::new();
    // GCS is comfortable with high concurrency on a single bucket
    // (documented at 5k writes/sec/bucket once you spread keys).
    // The materializer is the latency-critical path on enable —
    // bump concurrency so a 1024-chunk memory.bin materializes in
    // seconds rather than minutes. Coord's other workloads are
    // not on this hot path.
    let concurrency = 64;
    let mut iter = bootstrap.entries.iter();
    let mut written = 0usize;
    let mut deduped = 0usize;

    async fn process_one(
        blob: &dyn engram_core::traits::BlobStorage,
        entry: engram_chunk_store::BootstrapEntry,
        chunks_blob_slice: bytes::Bytes,
    ) -> Result<bool, ApiError> {
        let key = entry.sha256.storage_key();
        // exists-then-put is the dedup. A redundant put would still
        // be semantically a no-op (same content at same key), but
        // skipping the upload saves bandwidth + GCS write quota.
        match blob.exists(&key).await {
            Ok(true) => Ok(false),
            Ok(false) => {
                blob.put(&key, chunks_blob_slice)
                    .await
                    .map_err(|e| ApiError::Internal(format!("put chunk {}: {e}", entry.sha256)))?;
                Ok(true)
            }
            Err(e) => Err(ApiError::Internal(format!(
                "exists probe {}: {e}",
                entry.sha256
            ))),
        }
    }

    // Prime the pump.
    for _ in 0..concurrency {
        if let Some(entry) = iter.next() {
            let start = entry.blob_offset as usize;
            let end = start
                .checked_add(entry.length as usize)
                .ok_or_else(|| ApiError::Internal("chunk offset+length overflow".into()))?;
            if end > chunks_blob.len() {
                return Err(ApiError::Internal(format!(
                    "chunks blob too short for entry {}: end={end}, blob_len={}",
                    entry.sha256,
                    chunks_blob.len()
                )));
            }
            let slice = bytes::Bytes::copy_from_slice(&chunks_blob[start..end]);
            tasks.push(process_one(blob, entry.clone(), slice));
        }
    }
    while let Some(res) = tasks.next().await {
        match res? {
            true => written += 1,
            false => deduped += 1,
        }
        if let Some(entry) = iter.next() {
            let start = entry.blob_offset as usize;
            let end = start + entry.length as usize;
            let slice = bytes::Bytes::copy_from_slice(&chunks_blob[start..end]);
            tasks.push(process_one(blob, entry.clone(), slice));
        }
    }

    // After every chunk landed, the manifest JSON itself goes in
    // BlobStorage. host-agent's `chunk_store.get_manifest(mref)`
    // resolves through the same backend; a manifest with missing
    // chunks would be a footgun (faults on first read).
    //
    // put_manifest refuses to overwrite — that's the right
    // primitive for sessions (snapshot_id + version is supposed to
    // be unique). For the materializer, re-enabling the same
    // image is a real workflow (operator retry, idempotent
    // refresh), so probe first and only write when absent. The
    // chunks themselves are content-addressed so the same bytes
    // land at the same keys regardless of how many times we run.
    match chunk_store.get_manifest(manifest_ref).await {
        Ok(_) => {
            tracing::debug!(
                manifest = %manifest_ref,
                "manifest already present in BlobStorage; skipping put_manifest"
            );
        }
        Err(_) => {
            chunk_store
                .put_manifest(manifest_ref, &manifest)
                .await
                .map_err(|e| ApiError::Internal(format!("put_manifest {manifest_ref}: {e}")))?;
        }
    }

    Ok((written, deduped))
}

/// ADR 0014 M1.11: materialize the bake's OCI artifact into the
/// deployment's BlobStorage so prod host-agents can restore the
/// template by canonical key. Three pieces land in the store:
///   1. memory chunks (via `materialize_chunk_blob` with the
///      memory bootstrap + chunks layer)
///   2. state.bin → BlobStorage at `state_blob_key(snapshot_id)`
///   3. sidecar.json → BlobStorage at `sidecar_blob_key(snapshot_id)`
///
/// Returns the updated `SnapshotMetadata` with the canonical
/// state/sidecar keys stamped on so the snapshots row reflects
/// where the bytes actually live.
async fn materialize_template_artifacts(
    state: &SharedState,
    mut snapshot: SnapshotMetadata,
    artifacts: &engram_oci::TemplateArtifacts,
) -> Result<SnapshotMetadata, ApiError> {
    let blob = state.services.blob.clone();

    // Memory chunks + manifest. Only fires when the bake actually
    // captured canonical memory (both `memory_bootstrap_json` and
    // `memory_chunks_blob` must be present, paired by push-side
    // symmetry check). Without these, the snapshot is "metadata
    // only" — restorable from rootfs but with no memory dedup.
    if let (Some(boot), Some(blob_bytes), Some(mref)) = (
        artifacts.memory_bootstrap_json.as_deref(),
        artifacts.memory_chunks_blob.as_deref(),
        snapshot.memory_manifest,
    ) {
        let (wrote, deduped) = materialize_chunk_blob(
            blob.as_ref(),
            &state.services.chunk_store,
            mref,
            boot,
            blob_bytes,
        )
        .await?;
        tracing::info!(
            snapshot_id = %snapshot.id,
            manifest = %mref,
            wrote,
            deduped,
            "materialized memory chunks into BlobStorage",
        );
    }

    // Disk chunks + manifest. Same shape as memory; `disk_manifest`
    // is the ref the host's NBD daemon / materialize-to-file path
    // queries. Without this, the cold-create path falls back to
    // pulling the rootfs.ext4 layer (slow), or to on-fault Range-GET
    // against the OCI artifact (works but reaches back to the
    // registry every miss).
    if let (Some(boot), Some(blob_bytes), Some(mref)) = (
        artifacts.disk_bootstrap_json.as_deref(),
        artifacts.disk_chunks_blob.as_deref(),
        snapshot.disk_manifest,
    ) {
        let (wrote, deduped) = materialize_chunk_blob(
            blob.as_ref(),
            &state.services.chunk_store,
            mref,
            boot,
            blob_bytes,
        )
        .await?;
        tracing::info!(
            snapshot_id = %snapshot.id,
            manifest = %mref,
            wrote,
            deduped,
            "materialized disk chunks into BlobStorage",
        );
    }

    // state.bin
    if let Some(bytes) = artifacts.snapshot_state.as_deref() {
        let key = engram_chunk_store::snapshot_blob::state_blob_key(snapshot.id);
        // dedup probe is cheap; same content at the same key from a
        // prior enable would re-PUT for nothing.
        if !blob
            .exists(&key)
            .await
            .map_err(|e| ApiError::Internal(format!("exists probe state.bin: {e}")))?
        {
            blob.put(&key, bytes::Bytes::copy_from_slice(bytes))
                .await
                .map_err(|e| ApiError::Internal(format!("put state.bin at {key}: {e}")))?;
        }
        snapshot.state_blob_key = Some(key);
    }

    // sidecar.json
    if let Some(bytes) = artifacts.snapshot_sidecar_json.as_deref() {
        let key = engram_chunk_store::snapshot_blob::sidecar_blob_key(snapshot.id);
        if !blob
            .exists(&key)
            .await
            .map_err(|e| ApiError::Internal(format!("exists probe sidecar: {e}")))?
        {
            blob.put(&key, bytes::Bytes::copy_from_slice(bytes))
                .await
                .map_err(|e| ApiError::Internal(format!("put sidecar at {key}: {e}")))?;
        }
        snapshot.sidecar_blob_key = Some(key);
    }

    Ok(snapshot)
}

/// ADR 0014 M1.11: insert `snapshots` + `templates` rows from a
/// just-pulled bundle's `canonical_snapshot` block. Uses two
/// separate writes — `record_snapshot` (upsert on id) followed by
/// `upsert_template` (which has its own internal transaction
/// flipping the prior active row). The two calls are not in one
/// outer transaction: `upsert_template`'s ON CONFLICT semantics
/// already cover idempotent re-enable, and a partial failure
/// (snapshots row inserted but templates upsert failed) leaves the
/// snapshots row inert — referenced by nothing, GC'd on the next
/// sweep. Acceptable for the M1.11 scope; M1.12+ may tighten this
/// if we observe orphans in prod.
async fn cascade_into_templates(
    state: &SharedState,
    image_uri: &str,
    manifest: &ImageManifest,
    snapshot: SnapshotMetadata,
) -> Result<(), ApiError> {
    let (image_repo, image_tag) = {
        let (repo, tag) = split_image_ref(image_uri);
        (repo.to_string(), tag.to_string())
    };
    if image_tag.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "image `{image_uri}` has no tag suffix; templates cascade requires `<repo>:<tag>`"
        )));
    }

    // Defaults match the demo image's historical bake config; the
    // image's `resources` hints override when present. vcpus must be
    // >= 1 and memory_mib >= 128 (FC's documented minimum).
    let vcpus = manifest.resources.suggested_vcpus.unwrap_or(1).max(1);
    let memory_mib = manifest
        .resources
        .suggested_memory_mib
        .unwrap_or(512)
        .max(128);

    let now = Utc::now();
    let snapshot_record = SnapshotRecord {
        id: snapshot.id,
        // Template snapshots have no session_id (migration 0028).
        session_id: None,
        host_id: None,
        image_version: image_tag.clone(),
        size_bytes: snapshot.size_bytes,
        created_at: snapshot.created_at,
        last_accessed_at: now,
        disk_manifest: snapshot.disk_manifest,
        memory_manifest: snapshot.memory_manifest,
        // ADR 0014: canonical template snapshots are durable in
        // BlobStorage by construction. Mark recoverable so the M2
        // recovery path (when it lands) treats them as restorable.
        recoverable: true,
    };
    state.services.meta.record_snapshot(snapshot_record).await?;

    let template = TemplateRecord {
        template_ref: TemplateRef::new(),
        image_repo: image_repo.clone(),
        image_tag: image_tag.clone(),
        // M1.12 (option D): templates are harness-agnostic. The
        // column is nullable (migration 0029) and the unique key
        // no longer references it; new cascade rows write None.
        harness_pack_uri: None,
        snapshot_id: snapshot.id,
        vcpus,
        memory_mib,
        created_at: now,
        active: true,
    };
    state.services.meta.upsert_template(template).await?;

    tracing::info!(
        image_repo = %image_repo,
        image_tag = %image_tag,
        snapshot_id = %snapshot.id,
        vcpus,
        memory_mib,
        "templates cascade complete; warm pool will refill on next host heartbeat",
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::{Bootstrap, BootstrapEntry, ChunkSize, ManifestKind, ManifestRef};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    /// Regression guard for the OCI → BlobStorage materializer.
    /// Constructs a synthetic chunks-blob + bootstrap, runs the
    /// slicing logic, and asserts:
    ///   1. each chunk lands at its content-addressed key
    ///   2. the manifest lands at the supplied ManifestRef
    ///   3. a second call against the same BlobStorage dedups
    ///      (no redundant puts on top of identical bytes)
    #[tokio::test]
    async fn materialize_chunk_blob_writes_chunks_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());

        // Two chunks of distinct bytes. Hash → storage key derivation
        // mirrors what the bake's chunk_file produces.
        // Manifest validation requires per-chunk offsets to be
        // multiples of chunk_size. Use a tiny chunk_size (64 B) +
        // pad each chunk to that boundary so the test stays
        // self-contained without a 512 KiB allocation.
        let chunk_size_u32: u32 = 64;
        let chunk_size = ChunkSize::bytes(chunk_size_u32 as u64);
        let mut c0 = b"chunk-zero".to_vec();
        c0.resize(chunk_size_u32 as usize, 0);
        let mut c1 = b"chunk-one-different".to_vec();
        c1.resize(chunk_size_u32 as usize, 0);
        let h0 = engram_chunk_store::ChunkHash::of(&c0);
        let h1 = engram_chunk_store::ChunkHash::of(&c1);
        let mut chunks_blob = Vec::new();
        chunks_blob.extend_from_slice(&c0);
        chunks_blob.extend_from_slice(&c1);

        let bootstrap = Bootstrap {
            schema_version: engram_chunk_store::BOOTSTRAP_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            total_bytes: (c0.len() + c1.len()) as u64,
            chunk_size,
            entries: vec![
                BootstrapEntry {
                    file_offset: 0,
                    blob_digest: None,
                    blob_offset: 0,
                    length: c0.len() as u32,
                    sha256: h0,
                },
                BootstrapEntry {
                    file_offset: c0.len() as u64,
                    blob_digest: None,
                    blob_offset: c0.len() as u64,
                    length: c1.len() as u32,
                    sha256: h1,
                },
            ],
        };
        let bootstrap_json = serde_json::to_vec(&bootstrap).unwrap();
        let manifest_ref = ManifestRef::new();

        let (wrote, deduped) = materialize_chunk_blob(
            blob.as_ref(),
            &chunk_store,
            manifest_ref,
            &bootstrap_json,
            &chunks_blob,
        )
        .await
        .expect("materialize");
        assert_eq!(wrote, 2);
        assert_eq!(deduped, 0);

        // Chunks landed at their content-addressed keys.
        for (h, body) in [(h0, &c0), (h1, &c1)] {
            let got = blob.get(&h.storage_key()).await.expect("get chunk");
            assert_eq!(got.as_ref(), body.as_slice());
        }

        // Manifest landed at the canonical ref.
        let m = chunk_store
            .get_manifest(manifest_ref)
            .await
            .expect("get_manifest");
        assert_eq!(m.chunks.len(), 2);
        assert_eq!(m.chunks[0].hash, h0);
        assert_eq!(m.chunks[1].hash, h1);
        assert_eq!(m.total_bytes, bootstrap.total_bytes);

        // Second call dedups — same content, same keys, nothing
        // re-written. This is the optimization the user asked about.
        let (wrote2, deduped2) = materialize_chunk_blob(
            blob.as_ref(),
            &chunk_store,
            manifest_ref,
            &bootstrap_json,
            &chunks_blob,
        )
        .await
        .expect("materialize again");
        assert_eq!(wrote2, 0, "second materialize should dedup all chunks");
        assert_eq!(deduped2, 2);
    }
}
