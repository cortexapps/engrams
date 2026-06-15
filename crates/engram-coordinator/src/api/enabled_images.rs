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

use chrono::Utc;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{EnabledImage, ImageManifest};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

// ADR 0039 Task 32: `enable_image` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `get_enable_job` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `list_enable_jobs` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `retry_enable_job` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `list_enabled_images` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `refresh_enabled_image` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

// ADR 0039 Task 32: `disable_enabled_image` axum shim removed. See `fetch_and_seal_manifest` for the gRPC entry point.

/// Pull the full engram OCI artifact at `image_uri` and validate it.
/// Returns the `EnabledImage` row ready to upsert, the parsed
/// `ImageManifest`, and the full `TemplateArtifacts` (so the
/// caller can push chunked-rootfs layers into BlobStorage).
pub(crate) async fn fetch_and_seal_manifest(
    state: &SharedState,
    image_uri: &str,
) -> Result<(EnabledImage, ImageManifest, engram_oci::TemplateArtifacts), ApiError> {
    let artifacts = state
        .services
        .oci
        .pull_template_metadata(image_uri)
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

    // ADR 0048: an enabled image MUST declare its vCPU count so
    // placement can pack against a host's CPU budget. (A stale manifest
    // using the old `suggested_vcpus` key already fails the parse above
    // via `deny_unknown_fields`.)
    validate_enabled_manifest(&manifest, image_uri).map_err(ApiError::BadRequest)?;

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
/// Bundle without disk chunks (`disk_bootstrap_json` and
/// `bundle_json` both `None`) is silently accepted — that's the
/// harness-builder pattern (bake produced just a manifest layer).
pub(crate) async fn materialize_disk_chunks(
    state: &SharedState,
    image_uri: &str,
    artifacts: &engram_oci::TemplateArtifacts,
    progress: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
) -> Result<Option<engram_core::types::manifest::ManifestRef>, ApiError> {
    let (Some(boot), Some(bundle_json)) = (
        artifacts.disk_bootstrap_json.as_deref(),
        artifacts.bundle_json.as_deref(),
    ) else {
        // Harness-only image: no chunked-disk artifact to materialize,
        // no ManifestRef to stamp on the row. Pin-set will skip it
        // via the partial index on `disk_manifest_id`.
        return Ok(None);
    };
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(boot)
        .map_err(|e| ApiError::Internal(format!("parse disk bootstrap json: {e}")))?;
    // ADR 0036 clean break: the materializer only speaks the
    // per-chunk-blob shape. A v1 (monolithic chunks blob) artifact
    // can't be enabled — its blob layer no longer has a consumer.
    if !bootstrap.is_per_chunk() {
        return Err(ApiError::BadRequest(format!(
            "`{image_uri}` carries a pre-ADR-0036 monolithic chunk-blob artifact; \
             re-bake the image with a current `engram image build` and push again"
        )));
    }
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
    let source = ChunkSource::Oci {
        oci: state.services.oci.as_ref(),
        image_uri,
    };
    let (wrote, deduped) = materialize_chunk_blob(
        state.services.blob.as_ref(),
        &state.services.chunk_store,
        manifest_ref,
        &bootstrap,
        &manifest,
        &source,
        progress,
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

/// Verify a reuse-candidate base snapshot's chunks are actually present in
/// BlobStorage before re-pointing a fresh enable at it. Content-keyed reuse
/// skips the capture VM entirely — but if the candidate snapshot's chunks
/// were lost (a GC over-delete, a manual deletion, a partial earlier upload),
/// reusing it re-points the image at a corrupt base that 404s at restore (the
/// wedged-session class). The disk materialize re-uploads missing disk chunks
/// from the OCI source on every refresh, but a base snapshot's MEMORY chunks
/// have no source other than a fresh capture — so a miss in EITHER of the
/// candidate's manifests means we must recapture rather than reuse. HEADs
/// every chunk (short-circuiting on the first miss); any miss — or any error
/// reading a manifest/probe — is treated as "not reusable" so we fail safe
/// toward a correct fresh capture.
async fn reuse_candidate_chunks_present(
    chunk_store: &engram_chunk_store::ChunkStore,
    blob: std::sync::Arc<dyn engram_core::traits::BlobStorage>,
    disk_manifest: engram_core::types::manifest::ManifestRef,
    memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
) -> bool {
    use futures::stream::{self, StreamExt};
    for mref in std::iter::once(disk_manifest).chain(memory_manifest) {
        let manifest = match chunk_store.get_manifest(mref).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    manifest = %mref,
                    error = %e,
                    "reuse verify: could not read candidate base-snapshot manifest; \
                     treating as not reusable (will recapture)",
                );
                return false;
            }
        };
        // Collect owned keys up front: holding a borrow of `manifest` (a
        // slice Iter) across the buffer_unordered awaits makes this future
        // non-Send, and it runs inside the spawned enable pipeline.
        let keys: Vec<String> = manifest
            .chunks
            .iter()
            .map(|c| c.hash.storage_key())
            .collect();
        let mut checks = stream::iter(keys.into_iter().map(|key| {
            let blob = blob.clone();
            async move { blob.exists(&key).await }
        }))
        .buffer_unordered(32);
        while let Some(res) = checks.next().await {
            match res {
                Ok(true) => {}
                Ok(false) => return false,
                Err(e) => {
                    tracing::warn!(
                        manifest = %mref,
                        error = %e,
                        "reuse verify: chunk existence probe failed; \
                         treating as not reusable (will recapture)",
                    );
                    return false;
                }
            }
        }
    }
    true
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
pub(crate) async fn capture_and_record_base_snapshot(
    state: &SharedState,
    row: &EnabledImage,
    manifest: &ImageManifest,
) -> Result<
    (
        engram_core::types::SnapshotId,
        // Disk manifest of the base snapshot (always present).
        engram_core::types::manifest::ManifestRef,
        // Memory manifest — `None` for cold-boot backends (VZ) that capture a
        // disk-only base snapshot; `Some` for FC's chunked memory snapshot.
        Option<engram_core::types::manifest::ManifestRef>,
    ),
    ApiError,
> {
    // ADR 0036 P4: content-keyed reuse. A base snapshot is a function
    // of (rootfs bytes, manifest.toml) — the bundle generations it
    // embeds are only the fallback pin, because session-create swaps
    // aux drives to the host's CURRENT staged generation (ADR 0035
    // Invariant 2; `restore_in_jail`'s `swap_aux_to_current`). So if
    // ANY enabled image (soft-deleted included — its snapshot stays
    // GC-pinned) was captured from the same disk content with the
    // same manifest.toml, that snapshot is equivalent to what a fresh
    // capture would produce: reuse it instead of booting a capture
    // VM. With deterministic bakes + content-derived ManifestRefs,
    // this is what makes a no-op re-bake's enable near-instant — and
    // hosts already hold the reused snapshot's chunks on NVMe, so no
    // fleet-wide re-prefetch either.
    if let Some(disk_ref) = row.disk_manifest {
        if let Some(existing) = state
            .services
            .meta
            .find_enabled_image_by_content(disk_ref, &row.manifest_toml)
            .await?
        {
            if let Some(id) = existing.base_snapshot_id {
                let disk_manifest = existing.base_snapshot_disk_manifest.ok_or_else(|| {
                    ApiError::Internal(format!(
                        "enabled image `{}` reuses base snapshot {id} but carries no \
                         base_snapshot_disk_manifest (NOT NULL since migration 0042); \
                         refresh the image to re-stamp it",
                        existing.image_uri
                    ))
                })?;
                // Memory manifest is nullable since migration 0049 — `None`
                // for cold-boot backends (VZ). Reuse whatever the row carries.
                let memory_manifest = existing.base_snapshot_memory_manifest;
                // Self-heal: only reuse if the candidate's chunks are actually
                // durable. Re-pointing at a base snapshot whose chunks were
                // reaped is the wedged-session bug; memory chunks have no
                // source but a fresh capture, so a miss ⇒ recapture.
                if reuse_candidate_chunks_present(
                    &state.services.chunk_store,
                    state.services.blob.clone(),
                    disk_manifest,
                    memory_manifest,
                )
                .await
                {
                    tracing::info!(
                        image_uri = %row.image_uri,
                        reused_from = %existing.image_uri,
                        disk_manifest = %disk_ref,
                        snapshot_id = %id,
                        "content-identical image already captured; reusing base snapshot",
                    );
                    return Ok((id, disk_manifest, memory_manifest));
                }
                tracing::warn!(
                    image_uri = %row.image_uri,
                    reused_from = %existing.image_uri,
                    snapshot_id = %id,
                    "content-identical base snapshot is missing chunks in BlobStorage; \
                     re-capturing instead of reusing (self-heal)",
                );
            }
        }
    }

    // Legacy idempotency for rows without a chunked-disk manifest
    // (harness-only images): same URI at the same OCI digest with a
    // recorded snapshot — re-enabling shouldn't re-boot a capture VM.
    if let Some(existing) = state
        .services
        .meta
        .get_enabled_image(&row.image_uri)
        .await?
    {
        if existing.manifest_digest == row.manifest_digest {
            if let Some(id) = existing.base_snapshot_id {
                let disk_manifest = existing.base_snapshot_disk_manifest.ok_or_else(|| {
                    ApiError::Internal(format!(
                        "enabled image `{}` reuses base snapshot {id} but carries no \
                         base_snapshot_disk_manifest (NOT NULL since migration 0042); \
                         refresh the image to re-stamp it",
                        row.image_uri
                    ))
                })?;
                // Memory manifest is nullable since migration 0049 — `None`
                // for cold-boot backends (VZ). Reuse whatever the row carries.
                let memory_manifest = existing.base_snapshot_memory_manifest;
                // Self-heal: only reuse if the candidate's chunks are durable
                // (see the content-keyed branch above).
                if reuse_candidate_chunks_present(
                    &state.services.chunk_store,
                    state.services.blob.clone(),
                    disk_manifest,
                    memory_manifest,
                )
                .await
                {
                    tracing::info!(
                        image_uri = %row.image_uri,
                        digest = %row.manifest_digest,
                        snapshot_id = %id,
                        "base snapshot already recorded for this digest; reusing",
                    );
                    return Ok((id, disk_manifest, memory_manifest));
                }
                tracing::warn!(
                    image_uri = %row.image_uri,
                    digest = %row.manifest_digest,
                    snapshot_id = %id,
                    "recorded base snapshot for this digest is missing chunks in BlobStorage; \
                     re-capturing instead of reusing (self-heal)",
                );
            }
        }
    }

    // Anonymous capture spec — no session env, no harness pack (the
    // host substitutes its stub harness so the snapshot carries a
    // harness drive slot for per-session swap at restore). ADR 0027
    // bundles + ADR 0027 memory floor live inside the shared helper;
    // capture + restore MUST agree on `mem_size_mib` (FC requires it),
    // and ADR 0028's disk-only recovery boots the same shape.
    let spec = crate::api::sessions::cold_boot_spec(&row.image_uri, manifest, None);

    let (host_id, host) =
        crate::placement::pick_capture_host(state.services.meta.as_ref(), &state.host_registry)
            .await
            .map_err(|e| {
                ApiError::Unavailable(format!(
                    "no host is available to capture this image's base snapshot \
                     ({e:?}). Register a host and retry the enable."
                ))
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
            // ADR 0035: pin the capture's bundle generations.
            aux_bundles: meta.aux_bundles.clone(),
            disk_manifest: meta.disk_manifest,
            memory_manifest: meta.memory_manifest,
            recoverable,
            // Template artifact — no session, no event log.
            events_cursor: None,
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
    // Memory manifest is optional (migration 0049): FC produces a chunked
    // memory snapshot, VZ cold-boots and captures disk only. Pass through
    // whatever the backend produced — `None` skips memory residency.
    Ok((meta.id, disk_manifest, meta.memory_manifest))
}

/// ADR 0048: enable-time manifest validation. An enabled image must
/// declare `[resources] vcpus = N` so placement can reserve CPU and pack
/// hosts against a budget. Pure (no I/O) so it's unit-tested directly.
fn validate_enabled_manifest(manifest: &ImageManifest, image_uri: &str) -> Result<(), String> {
    if manifest.resources.vcpus.is_none() {
        return Err(format!(
            "manifest.toml at `{image_uri}` must declare `[resources] vcpus = N` \
             (ADR 0048: placement reserves CPU). Re-bake the image with a vcpus \
             declaration and retry the enable."
        ));
    }
    Ok(())
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

/// Where `materialize_chunk_blob` reads each chunk's bytes from.
///
/// `Map` serves chunks from memory (tests only). `Oci` pulls each
/// chunk's own blob from the registry on demand (ADR 0036), so the
/// coord never holds more than `CONCURRENCY` chunks at once — the bound
/// that stops a multi-GiB image from OOM-killing the coord on enable.
enum ChunkSource<'a> {
    /// In-memory chunk map — only the materialize unit test constructs
    /// this; prod always uses `Oci`.
    #[cfg(test)]
    Map(&'a std::collections::HashMap<engram_chunk_store::ChunkHash, bytes::Bytes>),
    Oci {
        oci: &'a engram_oci::OciClient,
        image_uri: &'a str,
    },
}

impl ChunkSource<'_> {
    async fn fetch(
        &self,
        entry: &engram_chunk_store::BootstrapEntry,
    ) -> Result<bytes::Bytes, ApiError> {
        // `*self` copies the Copy ref-fields out (they're all `&_`), so
        // the Oci arm gets `&str`/`&OciClient` rather than the
        // double-refs match-ergonomics would bind on `match self`.
        match *self {
            #[cfg(test)]
            ChunkSource::Map(map) => map.get(&entry.sha256).cloned().ok_or_else(|| {
                ApiError::Internal(format!("test chunk map missing {}", entry.sha256))
            }),
            ChunkSource::Oci { oci, image_uri } => {
                let digest = entry.blob_digest.as_deref().ok_or_else(|| {
                    ApiError::Internal(format!(
                        "bootstrap entry {} missing per-chunk blob digest (pre-ADR-0036 \
                         artifact slipped past the is_per_chunk gate?)",
                        entry.sha256
                    ))
                })?;
                // One blob GET per chunk; a single transient registry
                // hiccup (connection reset / partial body) shouldn't
                // fail the whole enable. Retry with backoff — and back
                // off much harder on 429s, which want a politer pause
                // than connection blips (Retry-After is typically
                // seconds-to-minutes).
                const MAX_ATTEMPTS: u32 = 5;
                let mut attempt = 1u32;
                loop {
                    match oci.pull_chunk(image_uri, digest, entry.length as u64).await {
                        Ok(b) => break Ok(b),
                        Err(e) if attempt < MAX_ATTEMPTS => {
                            let rate_limited = e.to_string().contains("429");
                            let backoff_ms =
                                if rate_limited { 5_000 } else { 200 } * attempt as u64;
                            tracing::warn!(
                                chunk = %entry.sha256,
                                attempt,
                                rate_limited,
                                error = %e,
                                "chunk pull failed; retrying with backoff"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                            attempt += 1;
                        }
                        Err(e) => {
                            break Err(ApiError::Internal(format!(
                                "fetch chunk {} after {attempt} attempts: {e}",
                                entry.sha256
                            )))
                        }
                    }
                }
            }
        }
    }
}

/// Push each chunk in `bootstrap` into BlobStorage at its
/// content-addressed key, plus the reconstructed `Manifest` at
/// `manifest_ref`. Content-addressing means chunks already present (a
/// prior bake / re-enable / deterministic re-bake sharing chunks) are
/// skipped — and with an `Oci` source they aren't even fetched, so a
/// delta re-enable moves only delta bytes (ADR 0036). Bounded-parallel
/// exists?-fetch-put keeps the wire busy without a thousand-deep
/// sequential round-trip.
///
/// Returns (wrote, deduped) chunk counts.
async fn materialize_chunk_blob(
    blob: &dyn engram_core::traits::BlobStorage,
    chunk_store: &engram_chunk_store::ChunkStore,
    manifest_ref: engram_core::types::manifest::ManifestRef,
    bootstrap: &engram_chunk_store::Bootstrap,
    manifest: &engram_chunk_store::Manifest,
    source: &ChunkSource<'_>,
    progress: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
) -> Result<(usize, usize), ApiError> {
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut tasks = FuturesUnordered::new();
    // Cap in-flight chunks: with an OciRange source each task holds a
    // freshly-downloaded chunk (~chunk_size) until its put completes, so
    // peak memory is ~CONCURRENCY × chunk_size. That bound is the whole
    // point — it's what keeps a multi-GiB enable from OOM-killing the
    // coord. GCS easily sustains this fan-out (5k writes/sec/bucket).
    const CONCURRENCY: usize = 16;
    let mut iter = bootstrap.entries.iter();
    let mut written = 0usize;
    let mut deduped = 0usize;

    async fn process_one(
        blob: &dyn engram_core::traits::BlobStorage,
        source: &ChunkSource<'_>,
        entry: engram_chunk_store::BootstrapEntry,
    ) -> Result<bool, ApiError> {
        let key = entry.sha256.storage_key();
        // Content-addressed: a chunk already in BlobStorage is skipped,
        // and (OciRange) never fetched.
        if blob
            .exists(&key)
            .await
            .map_err(|e| ApiError::Internal(format!("exists probe {}: {e}", entry.sha256)))?
        {
            return Ok(false);
        }
        let bytes = source.fetch(&entry).await?;
        blob.put(&key, bytes)
            .await
            .map_err(|e| ApiError::Internal(format!("put chunk {}: {e}", entry.sha256)))?;
        Ok(true)
    }

    for _ in 0..CONCURRENCY {
        if let Some(entry) = iter.next() {
            tasks.push(process_one(blob, source, entry.clone()));
        }
    }
    while let Some(res) = tasks.next().await {
        match res? {
            true => written += 1,
            false => deduped += 1,
        }
        // ADR 0036: progress counter for the enable job's
        // chunks_done — counts every settled chunk (fetched or
        // dedup-skipped); the scanner's checkpoint task persists it.
        if let Some(p) = &progress {
            p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(entry) = iter.next() {
            tasks.push(process_one(blob, source, entry.clone()));
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

    #[test]
    fn enable_validation_requires_a_vcpus_declaration() {
        // No [resources] at all → rejected.
        let bare: ImageManifest = toml::from_str("name = \"x\"\n").unwrap();
        let err = validate_enabled_manifest(&bare, "r/x:t").unwrap_err();
        assert!(err.contains("vcpus"), "error must name the field: {err}");

        // [resources] present but vcpus omitted → rejected.
        let no_vcpus: ImageManifest =
            toml::from_str("name = \"x\"\n[resources]\nsuggested_memory_mib = 2048\n").unwrap();
        assert!(validate_enabled_manifest(&no_vcpus, "r/x:t").is_err());

        // Declared → accepted.
        let ok: ImageManifest = toml::from_str("name = \"x\"\n[resources]\nvcpus = 4\n").unwrap();
        assert!(validate_enabled_manifest(&ok, "r/x:t").is_ok());

        // A stale `suggested_vcpus` key fails the PARSE (deny_unknown_fields),
        // so it never reaches validation — proven here for completeness.
        assert!(toml::from_str::<ImageManifest>(
            "name = \"x\"\n[resources]\nsuggested_vcpus = 2\n"
        )
        .is_err());
    }

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
        let chunk_map: std::collections::HashMap<_, _> = [
            (h0, bytes::Bytes::from(c0.clone())),
            (h1, bytes::Bytes::from(c1.clone())),
        ]
        .into_iter()
        .collect();

        // ADR 0036 per-chunk shape: each entry addresses its own
        // blob (digest == chunk hash), blob_offset always 0.
        let bootstrap = Bootstrap {
            schema_version: engram_chunk_store::BOOTSTRAP_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            total_bytes: (c0.len() + c1.len()) as u64,
            chunk_size,
            entries: vec![
                BootstrapEntry {
                    file_offset: 0,
                    blob_digest: Some(format!("sha256:{}", h0.to_hex())),
                    blob_offset: 0,
                    length: c0.len() as u32,
                    sha256: h0,
                },
                BootstrapEntry {
                    file_offset: c0.len() as u64,
                    blob_digest: Some(format!("sha256:{}", h1.to_hex())),
                    blob_offset: 0,
                    length: c1.len() as u32,
                    sha256: h1,
                },
            ],
        };
        assert!(bootstrap.is_per_chunk());
        let manifest = bootstrap.to_manifest();
        let manifest_ref = ManifestRef::new();

        let (wrote, deduped) = materialize_chunk_blob(
            blob.as_ref(),
            &chunk_store,
            manifest_ref,
            &bootstrap,
            &manifest,
            &ChunkSource::Map(&chunk_map),
            None,
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
            &ChunkSource::Map(&chunk_map),
            None,
        )
        .await
        .expect("materialize again");
        assert_eq!(wrote2, 0, "second materialize should dedup all chunks");
        assert_eq!(deduped2, 2);
    }

    /// Self-heal verify: a reuse candidate whose manifest chunks are all
    /// present in BlobStorage is reusable; a missing chunk (the reaped-base
    /// case) makes it NOT reusable, so the enable path recaptures instead of
    /// re-pointing the image at a corrupt base snapshot.
    #[tokio::test]
    async fn reuse_candidate_verify_detects_missing_chunk() {
        use engram_chunk_store::{ChunkHash, ChunkRef, Manifest};

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());
        let cs = ManifestKind::Memory.default_chunk_size();

        // Manifest whose single chunk IS stored → reusable.
        let present_hash = chunk_store.put_chunk(b"present-chunk-bytes").await.unwrap();
        let mut present = Manifest::empty(ManifestKind::Memory, cs);
        present.chunks.push(ChunkRef {
            offset: 0,
            hash: present_hash,
        });
        let present_ref = ManifestRef::new();
        chunk_store
            .put_manifest(present_ref, &present)
            .await
            .unwrap();
        assert!(
            reuse_candidate_chunks_present(&chunk_store, blob.clone(), present_ref, None).await,
            "all chunks present ⇒ reusable",
        );

        // Manifest referencing a chunk whose blob was never stored (reaped)
        // → NOT reusable; the enable path must recapture.
        let missing_hash = ChunkHash::of(b"a-reaped-chunk-never-stored");
        let mut missing = Manifest::empty(ManifestKind::Memory, cs);
        missing.chunks.push(ChunkRef {
            offset: 0,
            hash: missing_hash,
        });
        let missing_ref = ManifestRef::new();
        chunk_store
            .put_manifest(missing_ref, &missing)
            .await
            .unwrap();
        assert!(
            !reuse_candidate_chunks_present(&chunk_store, blob.clone(), missing_ref, None).await,
            "a missing chunk ⇒ not reusable (recapture)",
        );

        // The prod shape: intact disk manifest + a missing MEMORY chunk (a
        // reaped base-memfile chunk) ⇒ not reusable.
        assert!(
            !reuse_candidate_chunks_present(
                &chunk_store,
                blob.clone(),
                present_ref,
                Some(missing_ref),
            )
            .await,
            "present disk + missing memory chunk ⇒ not reusable",
        );
    }
}
