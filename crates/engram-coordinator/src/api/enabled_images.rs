//! `/api/enabled-images` — operator-curated allow-list of OCI image
//! URIs that sessions may reference.
//!
//! Stage C (and ADR 0015 M5): this is the single source of truth for
//! "what images can a session use." Postgres stores the URI plus a
//! snapshot of the manifest.toml fetched at enable time, so
//! `POST /sessions` resolves the manifest from a Postgres row without
//! going to the registry on the hot path. The dashboard's image
//! picker reads from here.
//!
//! ADR 0015 M5: enable is now atomic and simple — fetch the OCI
//! artifact, validate the manifest, push the chunked rootfs into
//! BlobStorage so hosts can prefetch from it, upsert the row.
//! The `templates` cascade (snapshot materialization, warm-pool
//! waiting) is gone; hosts diff `enabled_images` against their local
//! `ready_images` set on every heartbeat and prefetch what's
//! missing.
//!
//! Verbs:
//! - `POST /api/enabled-images   { image_uri }` — enable an image.
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
use engram_core::types::{EnabledImage, EnabledImageSummary, ImageManifest};
use engram_core::MetaError;
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

    let (mut row, _manifest, artifacts) = fetch_and_seal_manifest(&state, &req.image_uri).await?;
    // ADR 0016 Phase C commit 3a: stamp the bake's ManifestRef on
    // the row so the GC pin-set can read it back without re-pulling
    // bundle.json from OCI on every sweep. `None` for harness-only.
    row.disk_manifest = materialize_disk_chunks(&state, &artifacts).await?;
    state
        .services
        .meta
        .upsert_enabled_image(row.clone())
        .await?;

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

    let (mut refreshed, _manifest, artifacts) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    refreshed.id = existing.id;
    refreshed.created_at = existing.created_at;
    refreshed.updated_at = Some(Utc::now());

    refreshed.disk_manifest = materialize_disk_chunks(&state, &artifacts).await?;
    state
        .services
        .meta
        .upsert_enabled_image(refreshed.clone())
        .await?;

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

/// Pull the full engram OCI artifact at `image_uri` and validate it.
/// Returns the `EnabledImage` row ready to upsert, the parsed
/// `ImageManifest`, and the full `TemplateArtifacts` (so the
/// caller can push chunked-rootfs layers into BlobStorage).
async fn fetch_and_seal_manifest(
    state: &SharedState,
    image_uri: &str,
) -> Result<(EnabledImage, ImageManifest, engram_oci::TemplateArtifacts), ApiError> {
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

    let now = Utc::now();
    let row = EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.to_string(),
        manifest_toml,
        manifest_digest: artifacts.manifest_digest.as_str().to_string(),
        // Stamped by the caller after `materialize_disk_chunks`
        // returns the bake's ManifestRef (or `None` for harness-only).
        disk_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
    };
    Ok((row, manifest, artifacts))
}

/// ADR 0015 M5: push the chunked rootfs into BlobStorage at the
/// content-addressed key for each chunk, plus the reconstructed
/// `Manifest` at the disk `ManifestRef`. Hosts' prefetch loop reads
/// from BlobStorage; nothing else feeds it.
///
/// Bundle without disk chunks (`disk_bootstrap_json` / `disk_chunks_blob`
/// both `None`) is silently accepted — that's the harness-builder
/// pattern (bake produced just a manifest layer).
async fn materialize_disk_chunks(
    state: &SharedState,
    artifacts: &engram_oci::TemplateArtifacts,
) -> Result<Option<engram_core::types::manifest::ManifestRef>, ApiError> {
    let (Some(boot), Some(blob_bytes), Some(bundle_json)) = (
        artifacts.disk_bootstrap_json.as_deref(),
        artifacts.disk_chunks_blob.as_deref(),
        artifacts.bundle_json.as_deref(),
    ) else {
        // Harness-only image: no chunked-disk artifact to materialize,
        // no ManifestRef to stamp on the row. Pin-set will skip it
        // via the partial index on `disk_manifest_id`.
        return Ok(None);
    };
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(boot)
        .map_err(|e| ApiError::Internal(format!("parse disk bootstrap json: {e}")))?;
    let manifest = bootstrap.to_manifest();
    // The bake wrote bundle.json with the disk_manifest ref it used
    // against its LOCAL chunk store. Hosts re-read that ref on
    // prefetch to ask `chunk_store.get_manifest(ref)` — so we must
    // materialize at the SAME ref, not a fresh one. Otherwise the
    // host's lookup faults with `blob not found` (ADR 0015 M5
    // integration-test regression caught by the dev-vm smoke).
    let manifest_ref = parse_disk_manifest_ref(bundle_json).ok_or_else(|| {
        ApiError::BadRequest(
            "bundle.json missing or unparseable `disk_manifest` field; \
             can't materialize chunks without knowing the canonical ref"
                .into(),
        )
    })?;
    let (wrote, deduped) = materialize_chunk_blob(
        state.services.blob.as_ref(),
        &state.services.chunk_store,
        manifest_ref,
        &bootstrap,
        &manifest,
        blob_bytes,
    )
    .await?;
    tracing::info!(
        wrote,
        deduped,
        manifest = %manifest_ref,
        "materialized disk chunks into BlobStorage",
    );
    Ok(Some(manifest_ref))
}

/// Parse the bake's bundle.json and pull out its `disk_manifest`
/// ref, the same key hosts ask the chunk store for at prefetch
/// time. The bake serializes `ManifestRef` via serde so the field
/// is a JSON object `{manifest_id, version}`. Returns `None` for
/// missing/malformed bundles — caller surfaces as 400.
fn parse_disk_manifest_ref(
    bundle_json: &[u8],
) -> Option<engram_core::types::manifest::ManifestRef> {
    let v: serde_json::Value = serde_json::from_slice(bundle_json).ok()?;
    let field = v.get("disk_manifest")?.clone();
    serde_json::from_value(field).ok()
}

/// Push a chunk-blob into BlobStorage at the content-addressed key
/// for each chunk in `bootstrap`, plus the reconstructed `Manifest`
/// at `manifest_ref`. Content-addressing means chunks present from
/// a prior bake are skipped — only genuinely new bytes hit the wire.
/// Bounded-parallel exists+put fans out so the materialization isn't
/// a thousand-deep sequential round-trip.
///
/// Returns (wrote, deduped) chunk counts.
async fn materialize_chunk_blob(
    blob: &dyn engram_core::traits::BlobStorage,
    chunk_store: &engram_chunk_store::ChunkStore,
    manifest_ref: engram_core::types::manifest::ManifestRef,
    bootstrap: &engram_chunk_store::Bootstrap,
    manifest: &engram_chunk_store::Manifest,
    chunks_blob: &[u8],
) -> Result<(usize, usize), ApiError> {
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut tasks = FuturesUnordered::new();
    // GCS is comfortable with high concurrency on a single bucket
    // (documented at 5k writes/sec/bucket once you spread keys).
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
    match chunk_store.get_manifest(manifest_ref).await {
        Ok(_) => {
            tracing::debug!(
                manifest = %manifest_ref,
                "manifest already present in BlobStorage; skipping put_manifest"
            );
        }
        Err(_) => {
            chunk_store
                .put_manifest(manifest_ref, manifest)
                .await
                .map_err(|e| ApiError::Internal(format!("put_manifest {manifest_ref}: {e}")))?;
        }
    }

    Ok((written, deduped))
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
        let manifest = bootstrap.to_manifest();
        let manifest_ref = ManifestRef::new();

        let (wrote, deduped) = materialize_chunk_blob(
            blob.as_ref(),
            &chunk_store,
            manifest_ref,
            &bootstrap,
            &manifest,
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
        // re-written.
        let (wrote2, deduped2) = materialize_chunk_blob(
            blob.as_ref(),
            &chunk_store,
            manifest_ref,
            &bootstrap,
            &manifest,
            &chunks_blob,
        )
        .await
        .expect("materialize again");
        assert_eq!(wrote2, 0, "second materialize should dedup all chunks");
        assert_eq!(deduped2, 2);
    }
}
