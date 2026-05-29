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
//! - `POST /api/enabled-images   { image_uri }` — enable an image
//!   (or undelete a previously soft-deleted row).
//! - `GET  /api/enabled-images`                — list live rows
//!   (`soft_deleted_at IS NULL`).
//! - `POST /api/enabled-images/refresh { image_uri }` — re-fetch the
//!   manifest from the registry and update `manifest_toml` +
//!   `manifest_digest`. Useful when an image tag is moved.
//! - `POST /api/enabled-images/disable { image_uri }` — soft-delete
//!   the row. ADR 0021 P1.8: rather than physical DELETE, flip
//!   `soft_deleted_at = NOW()` so existing idle sessions can still
//!   resume against the same chunk lineage. Refuses (409) when one
//!   or more sessions in `{pending, created, active, evacuating}`
//!   still reference the image — the response lists the blocking
//!   sessions so the operator can decide. Re-enabling an
//!   image_uri via POST clears `soft_deleted_at`.
//!
//! URI as path-segment: image URIs contain `/` and `:`, which axum's
//! Path extractor will URL-decode but the dashboard's HTTP client
//! would have to encode. POSTing the URI in the body keeps both ends
//! simple and avoids the bikeshed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_core::types::snapshot::SnapshotRecord;
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

    let (mut row, manifest, artifacts) = fetch_and_seal_manifest(&state, &req.image_uri).await?;
    // ADR 0016 Phase C commit 3a: stamp the bake's ManifestRef on
    // the row so the GC pin-set can read it back without re-pulling
    // bundle.json from OCI on every sweep. `None` for harness-only.
    row.disk_manifest = materialize_disk_chunks(&state, &artifacts).await?;
    // ADR 0020 P1: an image is not enabled unless its per-image base
    // snapshot was captured + recorded. This blocks on a host-side
    // capture; on failure we return before writing the enabled_images
    // row, so a failed snapshot leaves zero rows (the NOT NULL FK on
    // base_snapshot_id makes that a schema invariant, not just a
    // convention).
    let (base_snapshot_id, base_snapshot_disk_manifest, base_snapshot_memory_manifest) =
        capture_and_record_base_snapshot(&state, &row, &manifest).await?;
    row.base_snapshot_id = Some(base_snapshot_id);
    row.base_snapshot_disk_manifest = Some(base_snapshot_disk_manifest);
    row.base_snapshot_memory_manifest = Some(base_snapshot_memory_manifest);
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

    let (mut refreshed, manifest, artifacts) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    refreshed.id = existing.id;
    refreshed.created_at = existing.created_at;
    refreshed.updated_at = Some(Utc::now());

    refreshed.disk_manifest = materialize_disk_chunks(&state, &artifacts).await?;
    // ADR 0020 P1: a moved tag is new content — capture a fresh base
    // snapshot for the new digest before the refreshed row goes live.
    let (base_snapshot_id, base_snapshot_disk_manifest, base_snapshot_memory_manifest) =
        capture_and_record_base_snapshot(&state, &refreshed, &manifest).await?;
    refreshed.base_snapshot_id = Some(base_snapshot_id);
    refreshed.base_snapshot_disk_manifest = Some(base_snapshot_disk_manifest);
    refreshed.base_snapshot_memory_manifest = Some(base_snapshot_memory_manifest);
    state
        .services
        .meta
        .upsert_enabled_image(refreshed.clone())
        .await?;

    Ok(Json(EnabledImageSummary::from(refreshed)))
}

/// ADR 0021 P1.8: response body for a refused disable.
///
/// 409 Conflict + this JSON, so the operator sees exactly which
/// sessions are pinning the image. Sessions in `{pending, created,
/// active, evacuating}` block; sessions in `{idle, completed, dead,
/// failed}` don't (idle resumes against the soft-deleted row, the
/// others are terminal).
#[derive(Serialize)]
pub struct DisableBlockedResponse {
    pub error: &'static str,
    pub message: String,
    pub blocking_sessions: Vec<BlockingSession>,
}

#[derive(Serialize)]
pub struct BlockingSession {
    pub session_id: Uuid,
    pub status: String,
}

pub async fn disable_enabled_image(
    State(state): State<SharedState>,
    Json(req): Json<ImageUriRequest>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    use engram_core::traits::DisableEnabledImageOutcome as Outcome;

    let outcome = state
        .services
        .meta
        .soft_delete_enabled_image(&req.image_uri)
        .await
        .map_err(|e| match e {
            MetaError::NotFound => {
                ApiError::NotFound(format!("image `{}` is not enabled", req.image_uri))
            }
            other => other.into(),
        })?;

    match outcome {
        Outcome::Disabled | Outcome::AlreadyDisabled => {
            // Both are no-error for the caller. AlreadyDisabled
            // keeps the endpoint idempotent for retries — the
            // operator who clicked "Disable" twice gets the same
            // 204 either way. Distinguishing would only be useful
            // for telemetry, which we already get from the
            // outcome metric below.
            tracing::info!(
                image_uri = %req.image_uri,
                already_disabled = matches!(outcome, Outcome::AlreadyDisabled),
                "enabled_image soft-deleted",
            );
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Outcome::Blocked(sessions) => {
            // Refuse with 409 + the offending sessions. The list
            // is bounded to the first 16 by the PG query so the
            // response stays small even when many sessions
            // reference the image.
            let n = sessions.len();
            let body = DisableBlockedResponse {
                error: "image_in_use",
                message: format!(
                    "Cannot disable image `{}`: {n} session(s) reference it. \
                     Wait for them to go idle, or force-stop them, then retry.",
                    req.image_uri,
                ),
                blocking_sessions: sessions
                    .into_iter()
                    .map(|(id, status)| BlockingSession {
                        session_id: id.as_uuid(),
                        status,
                    })
                    .collect(),
            };
            Ok((StatusCode::CONFLICT, Json(body)).into_response())
        }
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
        // Stamped by the caller after `capture_and_record_base_snapshot`.
        // The DB column is NOT NULL, so the upsert only succeeds once
        // this is set — enforcing "enabled iff base snapshot exists".
        base_snapshot_id: None,
        // ADR 0021 P2: stamped by the caller from the captured snapshot's
        // disk + memory manifests, alongside base_snapshot_id. `None` until then.
        base_snapshot_disk_manifest: None,
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        // Newly enabled or refreshed → always live. The upsert's
        // ON CONFLICT branch in PG flips `soft_deleted_at = NULL`
        // explicitly, so even an existing soft-deleted row gets
        // undeleted by re-enabling.
        soft_deleted_at: None,
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

/// ADR 0020 P1: capture (or reuse) the per-image base snapshot, record
/// its `snapshots` row, and return the snapshot id. The caller stamps it
/// onto the enabled_images row's NOT NULL `base_snapshot_id` and upserts
/// only after this succeeds — so a capture failure aborts the whole
/// enable and leaves zero rows (the FK makes "enabled iff base snapshot"
/// a schema invariant).
///
/// Idempotent: if the image is already enabled at the same content
/// digest with a base snapshot, reuse it — no re-boot.
///
/// The capture runs on a prod host (so it inherits the host CPU's
/// CPUID baseline; pair with `ENGRAM_FC_CPU_TEMPLATE=T2CL` for fleet
/// portability — ADR 0020). The host attaches its local stub harness,
/// boots to agentd-ready, snapshots (chunked memory + uploaded
/// state/sidecar), and tears the capture VM down.
async fn capture_and_record_base_snapshot(
    state: &SharedState,
    row: &EnabledImage,
    manifest: &ImageManifest,
) -> Result<
    (
        engram_core::types::SnapshotId,
        // (disk manifest, memory manifest) of the base snapshot.
        engram_core::types::manifest::ManifestRef,
        engram_core::types::manifest::ManifestRef,
    ),
    ApiError,
> {
    // Idempotency: if this image is already enabled at the same content
    // digest and carries a base snapshot, reuse it — re-enabling
    // shouldn't re-boot a capture VM.
    if let Some(existing) = state
        .services
        .meta
        .get_enabled_image(&row.image_uri)
        .await?
    {
        if existing.manifest_digest == row.manifest_digest {
            if let Some(id) = existing.base_snapshot_id {
                tracing::info!(
                    image_uri = %row.image_uri,
                    digest = %row.manifest_digest,
                    snapshot_id = %id,
                    "base snapshot already recorded for this digest; reusing",
                );
                let disk_manifest = existing.base_snapshot_disk_manifest.ok_or_else(|| {
                    ApiError::Internal(format!(
                        "enabled image `{}` reuses base snapshot {id} but carries no \
                         base_snapshot_disk_manifest (NOT NULL since migration 0042); \
                         refresh the image to re-stamp it",
                        row.image_uri
                    ))
                })?;
                let memory_manifest = existing.base_snapshot_memory_manifest.ok_or_else(|| {
                    ApiError::Internal(format!(
                        "enabled image `{}` reuses base snapshot {id} but carries no \
                         base_snapshot_memory_manifest (NOT NULL since migration 0043); \
                         refresh the image to re-stamp it",
                        row.image_uri
                    ))
                })?;
                return Ok((id, disk_manifest, memory_manifest));
            }
        }
    }

    let vcpus = manifest
        .resources
        .suggested_vcpus
        .unwrap_or(crate::api::sessions::DEFAULT_VCPUS);
    let memory_mib = manifest
        .resources
        .suggested_memory_mib
        .unwrap_or(crate::api::sessions::DEFAULT_MEMORY_MIB);
    let disk_gib = manifest
        .resources
        .suggested_disk_gib
        .unwrap_or(crate::api::sessions::DEFAULT_DISK_GIB);

    // Anonymous capture spec — no session env, no harness pack (the
    // host substitutes its stub harness so the snapshot carries a
    // harness drive slot for per-session swap at restore).
    let spec = SandboxSpec {
        image: row.image_uri.clone(),
        rootfs_source: None,
        image_uri: Some(row.image_uri.clone()),
        cpu: CpuLimit { vcpus },
        memory: MemoryLimit {
            max_mib: memory_mib,
        },
        disk: DiskLimit { max_gib: disk_gib },
        ttl: None,
        env: manifest.env.clone(),
        workdir: None,
        network: manifest.network.clone(),
    };

    let (host_id, host) = state.host_registry.pick_capture_host().ok_or_else(|| {
        ApiError::Unavailable(
            "no host is available to capture this image's base snapshot. \
             Register a host and retry the enable."
                .into(),
        )
    })?;

    tracing::info!(
        image_uri = %row.image_uri,
        host_id = %host_id,
        "capturing base snapshot for image enable",
    );
    let meta = host.build_base_snapshot(spec).await.map_err(|e| {
        ApiError::Internal(format!(
            "base snapshot capture for `{}` failed on host {host_id}: {e}",
            row.image_uri
        ))
    })?;

    // A base snapshot is only useful if its chunked manifests are
    // durable in BlobStorage — verify before recording, so an
    // unrecoverable capture aborts the enable rather than persisting a
    // dead pointer.
    let recoverable = crate::api::snapshot::verify_snapshot_recoverable(
        state.services.blob.as_ref(),
        meta.disk_manifest.as_ref(),
        meta.memory_manifest.as_ref(),
    )
    .await;
    if !recoverable {
        return Err(ApiError::Internal(format!(
            "base snapshot for `{}` was captured but its chunked manifests \
             failed HEAD-verify in BlobStorage; not enabling",
            row.image_uri
        )));
    }

    let now = Utc::now();
    // Record the snapshot row (session_id = NULL — a template artifact,
    // not a session capture). The caller stamps the returned id onto the
    // enabled_images row's NOT NULL base_snapshot_id and upserts it only
    // after this succeeds, so the rows go live together.
    state
        .services
        .meta
        .record_snapshot(SnapshotRecord {
            id: meta.id,
            session_id: None,
            host_id: Some(host_id),
            image_version: meta.image_version.clone(),
            size_bytes: meta.size_bytes,
            created_at: meta.created_at,
            last_accessed_at: now,
            disk_manifest: meta.disk_manifest,
            memory_manifest: meta.memory_manifest,
            recoverable,
        })
        .await?;

    tracing::info!(
        image_uri = %row.image_uri,
        snapshot_id = %meta.id,
        size_bytes = meta.size_bytes,
        "recorded base snapshot for image",
    );
    let disk_manifest = meta.disk_manifest.ok_or_else(|| {
        ApiError::Internal(format!(
            "base snapshot for `{}` was captured without a chunked disk manifest; \
             residency requires a chunked rootfs — not enabling",
            row.image_uri
        ))
    })?;
    let memory_manifest = meta.memory_manifest.ok_or_else(|| {
        ApiError::Internal(format!(
            "base snapshot for `{}` was captured without a chunked memory manifest; \
             memory residency requires one — not enabling",
            row.image_uri
        ))
    })?;
    Ok((meta.id, disk_manifest, memory_manifest))
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
