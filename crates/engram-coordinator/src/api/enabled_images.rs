//! `/api/enabled-images` — operator-curated allow-list of OCI image
//! URIs that sessions may reference.
//!
//! Stage C (and ADR 0015 M5): this is the single source of truth for
//! "what images can a session use." ADR 0080: Postgres stores the URI
//! plus the RPC-supplied `image_config` (name/description/env/workdir/
//! resources/warm) and the image's Dockerfile-derived `oci_defaults`
//! — the image carries no engrams metadata — so `POST /sessions`
//! resolves the effective config from a Postgres row without going to
//! the registry on the hot path. The dashboard's image picker reads
//! from here.
//!
//! ADR 0080 phase 3b: enable is asynchronous and the heavy lifting is
//! HOST-side — validate the config + a KB-sized docker-manifest probe,
//! `MaterializeImage` on a disk-healthy host (which pulls the STANDARD
//! docker image, flattens, packs a bootable ext4, and chunks it into
//! its write-through chunk store → BlobStorage), capture the base
//! snapshot from the materialized manifest under the job's config,
//! upsert the row. Old engram OCI artifacts can no longer be enabled
//! (clean break; existing rows keep working — their chunks are already
//! in BlobStorage). Hosts diff `enabled_images` against their local
//! `ready_images` set on every heartbeat and prefetch what's missing.
//!
//! The verbs live on the app-gRPC `ImageService` (`grpc_app/image.rs`):
//! EnableImage (config inherit-on-unset; required first enable),
//! UpdateImage (ADR 0080 — cheap fields in place, capture-affecting
//! fields behind `allow_recapture`), RefreshImage (re-pull, config
//! carried forward), DisableImage (guarded soft-delete, ADR 0021 P1.8:
//! existing idle sessions still resume against the same chunk lineage;
//! re-enabling clears `soft_deleted_at`).

use chrono::Utc;
use engram_core::types::image::ImageConfig;
use engram_core::types::registry::{RegistryAuthSpec, ResolvedRegistryAuth};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::EnabledImage;
use engram_oci_auth::AuthStrategy;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

/// The OCI platform-arch string of THIS coordinator process. ADR 0080
/// phase 3b ships it in `MaterializeImage` — coordinator and hosts are
/// same-arch per deployment (prod GCE, dev-vm, mac VZ alike), and the
/// host validates against its own arch, so a mismatch fails loud
/// instead of materializing the wrong platform.
pub(crate) fn coord_platform_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
    }
}

/// ADR 0080 phase 3b: cheap enqueue-time validation that `image_uri`
/// names a pullable STANDARD docker/OCI image — a KB-sized manifest
/// probe through the coordinator's own auth resolver, so a typo'd URI
/// or missing credential fails the POST with an actionable 400 instead
/// of burning scanner attempts. Probes the coordinator's arch first
/// and falls back to the sibling arch (accepting either): the
/// authoritative platform choice happens at materialize time on the
/// picked host, this is only "does the image exist and can we auth".
pub(crate) async fn validate_plain_image(
    state: &SharedState,
    image_uri: &str,
) -> Result<(), ApiError> {
    let primary = coord_platform_arch();
    let fallback = if primary == "arm64" { "amd64" } else { "arm64" };
    let first_err = match state
        .services
        .oci
        .pull_docker_manifest(image_uri, "linux", primary)
        .await
    {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    if state
        .services
        .oci
        .pull_docker_manifest(image_uri, "linux", fallback)
        .await
        .is_ok()
    {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!(
        "registry probe for `{image_uri}` failed: {first_err}. Check that the URI names a \
         standard docker/OCI image (engram artifacts can no longer be enabled — push with \
         `docker build && docker push`) and that a matching registry credential exists if \
         the registry requires auth."
    )))
}

/// Shared enable/update/refresh tail: validate the config + the image
/// reference (manifest probe) and create-or-get the enable job
/// carrying `config`. The manifest digest is stamped later by the
/// scanner from the platform manifest the host actually materializes —
/// enqueue passes `None` rather than guessing a platform here.
///
/// In-flight-config guard (check-then-act, admin-visible): re-POSTing an
/// enable is a resume, so `create_or_get_enable_job` returns the
/// in-flight job — but that job captures under ITS config. If the caller
/// asked for a DIFFERENT config, silently returning the old job would
/// drop the edit; fail `Conflict` instead so the operator retries once
/// the in-flight job settles.
pub(crate) async fn enqueue_enable_job(
    state: &SharedState,
    image_uri: &str,
    config: &ImageConfig,
    force_recapture: bool,
) -> Result<engram_core::types::EnableJob, ApiError> {
    config
        .validate()
        .map_err(|e| ApiError::BadRequest(format!("image config for `{image_uri}`: {e}")))?;
    validate_plain_image(state, image_uri).await?;
    let job = state
        .services
        .meta
        .create_or_get_enable_job_with_options(image_uri, None, config, force_recapture)
        .await?;
    if &job.image_config != config {
        return Err(ApiError::Conflict(format!(
            "an enable job for `{image_uri}` is already in flight (id {}, state {}) with a \
             different config; wait for it to finish (or retry it to terminal), then re-send \
             this edit",
            job.id,
            job.state.as_str(),
        )));
    }
    if job.force_recapture != force_recapture {
        return Err(ApiError::Conflict(format!(
            "an enable job for `{image_uri}` is already in flight (id {}, state {}) with a \
             different recapture setting; wait for it to finish (or retry it to terminal), \
             then re-send this request",
            job.id,
            job.state.as_str(),
        )));
    }
    Ok(job)
}

fn base_snapshot_reuse_ok(config: &ImageConfig, force_recapture: bool) -> bool {
    config.warm.is_none() && !force_recapture
}

/// ADR 0080 phase 3b: resolve the STATIC registry credential for
/// `image_uri`'s registry host, decrypted coordinator-side (CredCipher
/// via the deployment KEK) so the host receives ready-to-use basic
/// auth in the `MaterializeImage` request. Non-static rows
/// (GcpWorkloadIdentity / Anonymous) and missing rows resolve to
/// `None` — the host's ambient resolver covers those per-pull.
pub(crate) async fn resolve_static_registry_auth(
    state: &SharedState,
    image_uri: &str,
) -> Result<Option<ResolvedRegistryAuth>, ApiError> {
    let registry = engram_oci::registry_host(image_uri)
        .map_err(|e| ApiError::BadRequest(format!("image uri `{image_uri}`: {e}")))?;
    let Some(row) = state
        .services
        .meta
        .registry_credential_for_host(&registry)
        .await?
    else {
        return Ok(None);
    };
    match row.auth {
        RegistryAuthSpec::Static {
            username,
            wrapped_dek,
            nonce,
            ciphertext,
            key_id,
        } => {
            let strategy = engram_oci_auth::StaticStrategy::seal_open(
                state.services.kek.as_ref(),
                username,
                &wrapped_dek,
                &nonce,
                &ciphertext,
                &key_id,
            )
            .await
            .map_err(|e| {
                ApiError::Internal(format!(
                    "decrypt static registry credential for `{registry}`: {e}"
                ))
            })?;
            let creds = strategy.fetch_creds().await.map_err(|e| {
                ApiError::Internal(format!("registry credential for `{registry}`: {e}"))
            })?;
            Ok(Some(ResolvedRegistryAuth {
                username: creds.username,
                password: creds.password,
            }))
        }
        RegistryAuthSpec::GcpWorkloadIdentity { .. } | RegistryAuthSpec::Anonymous => Ok(None),
    }
}

/// ADR 0080 phase 3b: run the `materializing` stage on a host — pick a
/// disk-healthy host (the capture picker's ADR 0078 disk floor),
/// resolve static registry auth, and drive the streaming
/// `MaterializeImage` RPC. `progress` receives the host's stage frames
/// (the caller persists each as the job's progress line + claim
/// renewal). Error mapping mirrors the capture RPC's: connect-time
/// transport deaths and WIRE_VERSION skews become the retryable
/// `Unavailable`; structured host failures keep their kind
/// (`ApiError::MaterializeFailed`) for the scanner's classifier.
pub(crate) async fn materialize_image_on_host(
    state: &SharedState,
    image_uri: &str,
    progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
) -> Result<engram_core::types::MaterializedImage, ApiError> {
    // ADR 0084 / ADR 0081: materialize boots no VM — it just pulls +
    // flattens + packs + chunks the rootfs — so it carries no capture
    // footprint, RAM reservation, or anti-affinity. `pick_materialize_host`
    // is the ADR 0078 disk-floor-only picker; the CAPTURE stage uses the
    // reserving `place_capture_job` path instead.
    let (host_id, host) =
        crate::placement::pick_materialize_host(state.services.meta.as_ref(), &state.host_registry)
            .await
            .map_err(|e| {
                ApiError::Unavailable(format!(
                    "no host is available to materialize this image ({e:?}). \
                     Register a disk-healthy host and retry the enable."
                ))
            })?;
    let registry_auth = resolve_static_registry_auth(state, image_uri).await?;
    let arch = coord_platform_arch();
    tracing::info!(
        %image_uri,
        %host_id,
        platform = %format!("linux/{arch}"),
        static_auth = registry_auth.is_some(),
        "materializing image on host for enable",
    );
    host.materialize_image(image_uri, "linux", arch, registry_auth, progress)
        .await
        .map_err(|e| match e {
            engram_core::SandboxError::MaterializeFailed(failure) => ApiError::MaterializeFailed {
                kind: failure.kind,
                message: format!(
                    "materialize `{image_uri}` on host {host_id} failed: {}",
                    failure.message
                ),
            },
            // Same retryable transport classes as the capture RPC
            // (ADR 0050 C / issue #229): re-pick a host next attempt.
            engram_core::SandboxError::Unavailable(msg) => ApiError::Unavailable(format!(
                "materialize `{image_uri}` could not reach host {host_id}: {msg}"
            )),
            engram_core::SandboxError::WireSkew { host: hw, coord } => {
                ApiError::Unavailable(format!(
                    "materialize `{image_uri}` hit a WIRE_VERSION skew against host {host_id} \
                     (host={hw}, coord={coord})"
                ))
            }
            other => ApiError::Internal(format!(
                "materialize `{image_uri}` on host {host_id} failed: {other}"
            )),
        })
}

/// Build the `EnabledImage` row skeleton for one enable job — the
/// materialize + capture stages stamp
/// `disk_manifest`/`oci_defaults`/`manifest_digest` and the
/// base-snapshot refs onto it before the ready-time upsert.
pub(crate) fn new_enable_row(image_uri: &str, config: &ImageConfig) -> EnabledImage {
    let now = Utc::now();
    EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.to_string(),
        image_config: config.clone(),
        // Stamped from the MaterializeImage result (the Dockerfile
        // ENV/WORKDIR out of the image config blob).
        oci_defaults: Default::default(),
        // Stamped from the MaterializeImage result (the digest of the
        // platform manifest the host actually materialized).
        manifest_digest: String::new(),
        // Stamped from the MaterializeImage result (content-derived).
        disk_manifest: None,
        // Stamped after `capture_and_record_base_snapshot`. The DB
        // column is NOT NULL, so the upsert only succeeds once this is
        // set — enforcing "enabled iff base snapshot exists".
        base_snapshot_id: None,
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
    }
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
pub(crate) async fn reuse_candidate_chunks_present(
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

/// ADR 0084 §B4: whole-artifact reuse gains the FC-version dimension —
/// a memory-manifest-bearing candidate with NO recorded
/// `fc_snapshot_version` can never be placement-gated at restore time
/// (`CapabilityRequirements::fc_snapshot_version: None` is the SOFT
/// "unconstrained" posture, ADR 0068), so silently reusing one would
/// let a fresh session restore that snapshot on ANY host regardless of
/// its actual FC `SNAPSHOT_VERSION` — re-arming the issue-#160
/// cross-version corruption class this whole ADR exists to keep
/// closed. `enabled_images` has no denormalized
/// `base_snapshot_fc_snapshot_version` column (unlike disk/memory
/// manifest) to check cheaply, so this reads the `snapshots` row
/// directly. Disk-only candidates (`memory_manifest: None`, e.g. VZ)
/// have no FC/UFFD restore risk at all — always `true`. A metadata
/// hiccup fails safe toward recapture (`false`), matching the sibling
/// chunk-presence self-heal's posture.
async fn candidate_fc_version_known(
    state: &SharedState,
    snapshot_id: engram_core::types::SnapshotId,
    memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
) -> bool {
    if memory_manifest.is_none() {
        return true;
    }
    match state.services.meta.get_snapshot(snapshot_id).await {
        Ok(Some(record)) => record.fc_snapshot_version.is_some(),
        Ok(None) => {
            tracing::warn!(
                %snapshot_id,
                "reuse verify: candidate's snapshots row is gone; treating as not reusable",
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                %snapshot_id, error = %e,
                "reuse verify: fc_snapshot_version lookup failed; treating as not reusable",
            );
            false
        }
    }
}

/// ADR 0084 P1b: whether [`try_reuse_base_snapshot`] found (and verified)
/// an existing base snapshot equivalent to what a fresh capture would
/// produce — the content/digest reuse fast paths lifted verbatim out of
/// the old `capture_and_record_base_snapshot` (no host RPC, no
/// `capture_jobs` row; cheap PG-only checks the scanner runs on every
/// tick before it ever asks a host to do anything).
pub(crate) type ReuseHit = (
    engram_core::types::SnapshotId,
    // Disk manifest of the base snapshot (always present).
    engram_core::types::manifest::ManifestRef,
    // Memory manifest — `None` for cold-boot backends (VZ) that capture a
    // disk-only base snapshot; `Some` for FC's chunked memory snapshot.
    Option<engram_core::types::manifest::ManifestRef>,
);

/// ADR 0020 P1 / ADR 0084 P1b: reuse fast path — if the image is already
/// enabled at equivalent content (or, legacy, the same OCI digest) with a
/// base snapshot whose chunks are still durable, return it instead of
/// ever creating a `capture_jobs` row. `Ok(None)` means a fresh capture
/// is required (`ensure_capture_job` is the caller's next step).
///
/// Idempotent, side-effect-free (besides logging): safe to call on every
/// scanner tick without contributing to the job's attempts budget.
pub(crate) async fn try_reuse_base_snapshot(
    state: &SharedState,
    row: &EnabledImage,
    // `RefreshImage(force_recapture = true)`: the operator wants a fresh
    // capture even if an equivalent artifact exists — disables both reuse
    // fast paths below (the claim handler's `ColdBasePlan` honors the same
    // flag, so a forced enable never short-circuits anywhere).
    force_recapture: bool,
) -> Result<Option<ReuseHit>, ApiError> {
    // The effective config (RPC-supplied config merged over the
    // Dockerfile-derived defaults) is what the capture VM boots with —
    // the warm hook needs the image's env (JAVA_HOME, PATH, …).
    let config = row.effective_config();
    // A `[warm]` hook captures live process state (plus the resolved
    // warm-env secrets) that is NOT a pure function of (rootfs bytes,
    // config): two enables with identical content can differ in warm env
    // or in the live external state the warm boot reaches. So
    // content/digest reuse is unsound for warm images — always re-capture.
    // (This is also what makes a warm-secret rotate actually take effect:
    // a re-enable with the same digest must not short-circuit to the stale
    // snapshot.) ADR 0084 §B4: warm images no longer fall through to a
    // fresh capture from scratch either — `ensure_capture_job`/the claim
    // handler's `ColdBasePlan` reuses the COLD BASE (env-agnostic) and
    // always re-runs the hook fresh. This whole-artifact path stays
    // warm-less-only (and `force_recapture` disables it outright).
    let reuse_ok = base_snapshot_reuse_ok(&config, force_recapture);

    // ADR 0036 P4 / ADR 0080: content-keyed reuse. A base snapshot is a
    // function of (rootfs bytes, capture-affecting resources) — the
    // bundle generations it embeds are only the fallback pin, because
    // session-create swaps aux drives to the host's CURRENT staged
    // generation (ADR 0035 Invariant 2). Name/description/env/workdir
    // are applied per-session, so they're deliberately NOT in the key.
    // If ANY enabled image (soft-deleted included — its snapshot stays
    // GC-pinned) was captured from the same disk content with the same
    // resources, that snapshot is equivalent to what a fresh capture
    // would produce: reuse it instead of booting a capture VM. With
    // deterministic bakes + content-derived ManifestRefs, this is what
    // makes a no-op re-bake's enable near-instant — and hosts already
    // hold the reused snapshot's chunks on NVMe, so no fleet-wide
    // re-prefetch either.
    if let Some(disk_ref) = row.disk_manifest.filter(|_| reuse_ok) {
        if let Some(existing) = state
            .services
            .meta
            .find_enabled_image_by_content(disk_ref, &row.image_config.resources)
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
                    && candidate_fc_version_known(state, id, memory_manifest).await
                {
                    tracing::info!(
                        image_uri = %row.image_uri,
                        reused_from = %existing.image_uri,
                        disk_manifest = %disk_ref,
                        snapshot_id = %id,
                        "content-identical image already captured; reusing base snapshot",
                    );
                    return Ok(Some((id, disk_manifest, memory_manifest)));
                }
                tracing::warn!(
                    image_uri = %row.image_uri,
                    reused_from = %existing.image_uri,
                    snapshot_id = %id,
                    "content-identical base snapshot is missing chunks or has no recorded \
                     fc_snapshot_version; re-capturing instead of reusing (self-heal)",
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
        if reuse_ok && existing.manifest_digest == row.manifest_digest {
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
                    && candidate_fc_version_known(state, id, memory_manifest).await
                {
                    tracing::info!(
                        image_uri = %row.image_uri,
                        digest = %row.manifest_digest,
                        snapshot_id = %id,
                        "base snapshot already recorded for this digest; reusing",
                    );
                    return Ok(Some((id, disk_manifest, memory_manifest)));
                }
                tracing::warn!(
                    image_uri = %row.image_uri,
                    digest = %row.manifest_digest,
                    snapshot_id = %id,
                    "recorded base snapshot for this digest is missing chunks or has no \
                     recorded fc_snapshot_version; re-capturing instead of reusing (self-heal)",
                );
            }
        }
    }

    Ok(None)
}

/// ADR 0084 P1b: ensure a `capture_jobs` row exists for this enable job
/// and return the MOST RECENT one (terminal or not) — the scanner's
/// entire interaction with capture dispatch. Inserts a WAITING row then
/// runs the reserving pick (`place_capture_job` over
/// `capture_candidate_hosts`) only when no
/// row exists yet for this enable job; an existing row (running,
/// reassigned, or terminal) is returned as-is — the actual `SandboxSpec`/
/// env/egress assembly is deferred to the CLAIM endpoint
/// (`host_http::claim_capture_job`), which resolves secrets fresh at
/// claim time rather than once at job-creation time (ADR 0084 §A).
///
/// ADR 0084 §C: the placement pick uses the honest
/// [`crate::placement::CaptureFootprint`] inputs for a capture job —
/// `mem_mib` from the image's declared/default resources,
/// `image_size_mib` read off the ALREADY-materialized disk
/// manifest's chunk-store `Manifest::total_bytes` (a metadata-only read,
/// no chunk bytes fetched). Falls back to [`CaptureFootprint::floor_only`]
/// (LOUDLY logged) if the manifest can't be read — a capture must never
/// fail to even GET a placement pick over a sizing-metadata hiccup.
pub(crate) async fn capture_footprint_for(
    state: &SharedState,
    disk_manifest: engram_core::types::manifest::ManifestRef,
    config: &ImageConfig,
) -> crate::placement::CaptureFootprint {
    let mem_mib = config.resolved_memory_mib() as u64;
    match state.services.chunk_store.get_manifest(disk_manifest).await {
        Ok(manifest) => {
            let image_size_mib = (manifest.total_bytes / (1024 * 1024)).max(1);
            crate::placement::CaptureFootprint::for_capture(image_size_mib, mem_mib)
        }
        Err(e) => {
            tracing::warn!(
                manifest = %disk_manifest,
                error = %e,
                "capture footprint: could not read the disk manifest's total_bytes; \
                 falling back to FLOOR-ONLY sizing (no footprint headroom veto) for this pick",
            );
            crate::placement::CaptureFootprint::floor_only()
        }
    }
}

/// Same as [`capture_footprint_for`] but reads its inputs off an
/// existing `capture_jobs` row (the deadline-scan reassign pick and the
/// scanner's retryable-failure reassign pick both already have one) —
/// parses `row.disk_manifest` and merges `row.image_config` over
/// `row.oci_defaults` the same way the claim handler does.
pub(crate) async fn capture_footprint_for_job_row(
    state: &SharedState,
    row: &engram_core::types::capture_job::CaptureJobRow,
) -> crate::placement::CaptureFootprint {
    let config = row.image_config.merged_with(&row.oci_defaults);
    match row
        .disk_manifest
        .parse::<engram_core::types::manifest::ManifestRef>()
    {
        Ok(disk_manifest) => capture_footprint_for(state, disk_manifest, &config).await,
        Err(e) => {
            tracing::warn!(
                capture_job_id = %row.id,
                disk_manifest = %row.disk_manifest,
                error = %e,
                "capture footprint: capture_jobs.disk_manifest failed to parse; \
                 falling back to FLOOR-ONLY sizing for this reassign pick",
            );
            crate::placement::CaptureFootprint::floor_only()
        }
    }
}

/// ADR 0084 §B: the claim handler's cold-base decision for one attempt.
/// Computed ENTIRELY coordinator-side: the claiming host's own `hosts`
/// row already carries `capabilities.backend` + `capabilities.
/// fc_snapshot_version`, and `row`/`config` already carry
/// `disk_manifest`/`resources` — there is nothing here the executor
/// could derive independently, which is exactly why [`ColdBasePlan`]
/// is computed once, here, and only ever echoed back.
///
/// Fail-safe: any metadata hiccup (host lookup / `get_cold_base` /
/// chunk-presence probe) degrades to [`ColdBasePlan::NotApplicable`]
/// (or the coarsest `Miss` reason) rather than failing the claim — a
/// missed reuse opportunity costs an extra cold boot, not a broken
/// capture.
pub(crate) async fn resolve_cold_base_plan(
    state: &SharedState,
    host_id: engram_core::HostId,
    row: &engram_core::types::capture_job::CaptureJobRow,
    config: &ImageConfig,
) -> engram_core::types::capture_job::ColdBasePlan {
    use engram_core::types::capture_job::{ColdBaseMissReason, ColdBasePlan};

    let hosts = match state.services.meta.list_active_hosts().await {
        Ok(hosts) => hosts,
        Err(e) => {
            tracing::warn!(
                %host_id, capture_job_id = %row.id, error = %e,
                "resolve_cold_base_plan: list_active_hosts failed; treating as NotApplicable",
            );
            return ColdBasePlan::NotApplicable;
        }
    };
    let Some(host) = hosts.iter().find(|h| h.id == host_id) else {
        return ColdBasePlan::NotApplicable;
    };
    let backend_kind = "firecracker";
    let Some(fc_version) = (host.capabilities.backend == backend_kind)
        .then_some(host.capabilities.fc_snapshot_version.as_deref())
        .flatten()
    else {
        // Non-FC host, or an FC host that hasn't reported a
        // `fc_snapshot_version` yet — no cold-base concept applies.
        return ColdBasePlan::NotApplicable;
    };

    let content_key = engram_core::types::capture_job::cold_base_content_key(
        &row.disk_manifest,
        &config.resources,
        Some(fc_version),
        backend_kind,
    );

    // `RefreshImage(force_recapture = true)`: an operator forcing a fresh
    // capture must not get the old cold base restored back at them —
    // that's the exact artifact they're trying to flush (e.g. after a
    // capture-affecting change the content key can't see). Resolve as a
    // Miss so the executor cold-boots from scratch AND records the fresh
    // result under this content_key. `NoCandidate` rather than a new
    // variant: `ColdBaseMissReason` is telemetry-only, and it rides the
    // claim response JSON to the host — a variant an old host-agent can't
    // deserialize would break claims during a coord-first roll.
    let forced = match state.services.meta.get_enable_job(row.enable_job_id).await {
        Ok(job) => job.map(|j| j.force_recapture).unwrap_or(false),
        Err(e) => {
            tracing::warn!(
                %host_id, capture_job_id = %row.id, error = %e,
                "resolve_cold_base_plan: enable-job lookup for force_recapture failed; \
                 assuming not forced",
            );
            false
        }
    };
    if forced {
        tracing::info!(
            %host_id, capture_job_id = %row.id, %content_key,
            "resolve_cold_base_plan: force_recapture set on the enable job; \
             skipping cold-base reuse",
        );
        return ColdBasePlan::Miss {
            content_key,
            reason: ColdBaseMissReason::NoCandidate,
        };
    }

    let candidate = match state.services.meta.get_cold_base(&content_key).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                %host_id, capture_job_id = %row.id, %content_key, error = %e,
                "resolve_cold_base_plan: get_cold_base failed; treating as a miss",
            );
            None
        }
    };
    let Some(candidate) = candidate else {
        let fc_version_changed = state
            .services
            .meta
            .cold_base_fc_version_changed(&row.disk_manifest, fc_version)
            .await
            .unwrap_or(false);
        let reason = if fc_version_changed {
            ColdBaseMissReason::FcVersionChanged
        } else {
            ColdBaseMissReason::NoCandidate
        };
        return ColdBasePlan::Miss {
            content_key,
            reason,
        };
    };

    // Verify chunk presence before ever handing this candidate to a
    // host (the same self-heal `try_reuse_base_snapshot` applies to
    // whole-artifact reuse).
    let (Ok(disk_ref), mem_ref) = (
        candidate
            .disk_manifest
            .parse::<engram_core::types::manifest::ManifestRef>(),
        candidate
            .memory_manifest
            .parse::<engram_core::types::manifest::ManifestRef>()
            .ok(),
    ) else {
        tracing::warn!(
            %content_key, disk_manifest = %candidate.disk_manifest,
            "resolve_cold_base_plan: cold_bases row has an unparseable manifest ref; \
             treating as a miss",
        );
        return ColdBasePlan::Miss {
            content_key,
            reason: ColdBaseMissReason::ChunksMissing,
        };
    };
    let present = reuse_candidate_chunks_present(
        &state.services.chunk_store,
        state.services.blob.clone(),
        disk_ref,
        mem_ref,
    )
    .await;
    if !present {
        tracing::warn!(
            %content_key, snapshot_id = %candidate.snapshot_id,
            "resolve_cold_base_plan: cold-base candidate is missing chunks in BlobStorage; \
             recapturing instead of reusing (self-heal)",
        );
        return ColdBasePlan::Miss {
            content_key,
            reason: ColdBaseMissReason::ChunksMissing,
        };
    }

    match bincode::deserialize::<engram_core::types::snapshot::SnapshotMetadata>(
        &candidate.snapshot_bincode,
    ) {
        Ok(snapshot) => ColdBasePlan::Hit {
            content_key,
            snapshot: Box::new(snapshot),
        },
        Err(e) => {
            tracing::warn!(
                %content_key, snapshot_id = %candidate.snapshot_id, error = %e,
                "resolve_cold_base_plan: cold_bases row's snapshot_bincode failed to decode; \
                 treating as a miss",
            );
            ColdBasePlan::Miss {
                content_key,
                reason: ColdBaseMissReason::ChunksMissing,
            }
        }
    }
}

pub(crate) async fn ensure_capture_job(
    state: &SharedState,
    row: &EnabledImage,
    enable_job_id: Uuid,
) -> Result<engram_core::types::capture_job::CaptureJobRow, ApiError> {
    if let Some(existing) = state
        .services
        .meta
        .latest_capture_job_for_enable(enable_job_id)
        .await?
    {
        return Ok(existing);
    }
    let disk_manifest_ref = row.disk_manifest.ok_or_else(|| {
        ApiError::Internal(format!(
            "enable job for `{}` reached the capturing stage without a disk_manifest \
             (the materializing stage should have stamped one)",
            row.image_uri
        ))
    })?;
    let disk_manifest = disk_manifest_ref.to_string();
    let config = row.effective_config();
    tracing::info!(
        image_uri = %row.image_uri,
        %enable_job_id,
        "creating capture job for image enable",
    );
    // ADR 0084 (c): insert the row WAITING (host_id NULL) with its
    // placement budgets stamped from the image config — the single source
    // session placement reserves with, so a capture is exactly as visible
    // to the fleet as a session of this image.
    let new_job = engram_core::types::capture_job::NewCaptureJob {
        enable_job_id,
        image_uri: row.image_uri.clone(),
        manifest_digest: row.manifest_digest.clone(),
        disk_manifest,
        image_config: row.image_config.clone(),
        oci_defaults: row.oci_defaults.clone(),
        mem_budget_mib: config.resolved_memory_mib() as i64,
        cpu_budget_vcpus: config.resolved_vcpus() as i32,
    };
    let inserted = state.services.meta.insert_capture_job(new_job).await?;
    // Immediately attempt the reserving 2D pick so a fresh job dispatches
    // THIS tick rather than idling a full scan interval. No fit ⇒ the row
    // stays WAITING and the capacity scan (`capture_job_capacity_scan`)
    // re-offers it every tick, failing it with `CapacityTimeout` past the
    // queue deadline. `fc_snapshot_version` pin: `None` at job-creation
    // time (the claim handler resolves the cold-base candidate fresh per
    // attempt — ADR 0084 §B5 "known gap").
    let footprint = capture_footprint_for(state, disk_manifest_ref, &config).await;
    let candidates =
        crate::placement::capture_candidate_hosts(state.services.meta.as_ref(), footprint, None)
            .await
            .map_err(|e| {
                ApiError::Internal(format!(
                    "capture candidate hosts for `{}`: {e:?}",
                    row.image_uri
                ))
            })?;
    let placed = state
        .services
        .meta
        .place_capture_job(inserted.id, &candidates)
        .await?;
    Ok(placed.unwrap_or(inserted))
}

/// ADR 0084 §D: the [`finalize_capture_job`] outcome — its
/// `reuse_outcome` label (ADR 0084 §D's taxonomy) alongside the
/// [`ReuseHit`] the caller upserts onto the `enabled_images` row. A
/// plain string, not an enum: it's a one-way trip straight into
/// `set_enable_job_reuse_outcome`'s `TEXT` column.
pub(crate) type FinalizeOutcome = (ReuseHit, &'static str);

/// ADR 0084 P1b/P3: consume a `stage == Done` `capture_jobs` row —
/// decode its `result_bincode` (the executor's `CaptureJobResult`,
/// bincode-encoded), verify the chunked manifests are actually durable,
/// record the `snapshots` row, and (P3) record/skip the `cold_bases` row
/// per the executor's `cold_base` outcome. Mirrors the tail of the old
/// `capture_and_record_base_snapshot` exactly, except the FC
/// snapshot-version comes straight off the job row (the host already
/// stamped it in its terminal report) instead of a separate
/// `fc_snapshot_version_for_host` lookup.
pub(crate) async fn finalize_capture_job(
    state: &SharedState,
    capture_row: &engram_core::types::capture_job::CaptureJobRow,
) -> Result<FinalizeOutcome, ApiError> {
    let bytes = capture_row.result_bincode.as_deref().ok_or_else(|| {
        ApiError::Internal(format!(
            "capture job {} is `done` but carries no result_bincode",
            capture_row.id
        ))
    })?;
    let result: engram_core::types::capture_job::CaptureJobResult = bincode::deserialize(bytes)
        .map_err(|e| {
            ApiError::Internal(format!(
                "capture job {} result_bincode failed to decode: {e}",
                capture_row.id
            ))
        })?;
    let meta = result.snapshot;

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
            capture_row.image_uri
        )));
    }

    // ADR 0084 §B6/§D: record (or skip) the cold-base row + derive the
    // reuse_outcome label. Done BEFORE `record_snapshot` below — an
    // upsert_cold_base failure should abort the enable the same way a
    // recoverability failure does, rather than leave the overlay
    // recorded with an inconsistent cold-base row.
    use engram_core::types::capture_job::ColdBaseMissReason;
    let reuse_outcome: &'static str = match &result.cold_base {
        None => "recaptured:content_changed",
        Some(cb) if !cb.freshly_captured => "reused_cold_base",
        Some(cb) => {
            let label = match cb.miss_reason {
                Some(ColdBaseMissReason::NoCandidate) => "recaptured:no_cold_base",
                Some(ColdBaseMissReason::ChunksMissing) => "recaptured:chunks_missing",
                Some(ColdBaseMissReason::FcVersionChanged) => "recaptured:fc_version_changed",
                // Shouldn't happen (the executor only sets `freshly_captured`
                // from a `ColdBasePlan::Miss`, which always carries a
                // reason) — fall back to the generic label rather than
                // panicking on a telemetry field.
                None => "recaptured:content_changed",
            };
            let disk_manifest_text = cb
                .snapshot
                .disk_manifest
                .map(|r| r.to_string())
                .ok_or_else(|| {
                    ApiError::Internal(format!(
                        "capture job {} produced a cold base with no disk_manifest",
                        capture_row.id
                    ))
                })?;
            let memory_manifest_text = cb
                .snapshot
                .memory_manifest
                .map(|r| r.to_string())
                .ok_or_else(|| {
                    ApiError::Internal(format!(
                        "capture job {} produced a cold base with no memory_manifest \
                         (a cold base only ever exists on FC, which always chunks memory)",
                        capture_row.id
                    ))
                })?;
            let fc_snapshot_version = capture_row.fc_snapshot_version.clone().ok_or_else(|| {
                ApiError::Internal(format!(
                    "capture job {} produced a cold base but stamped no fc_snapshot_version",
                    capture_row.id
                ))
            })?;
            let snapshot_bincode = bincode::serialize(&cb.snapshot).map_err(|e| {
                ApiError::Internal(format!(
                    "capture job {}: failed to re-encode cold-base snapshot for storage: {e}",
                    capture_row.id
                ))
            })?;
            state
                .services
                .meta
                .upsert_cold_base(engram_core::types::capture_job::ColdBaseRow {
                    content_key: cb.content_key.clone(),
                    snapshot_id: cb.snapshot.id,
                    disk_manifest: disk_manifest_text,
                    memory_manifest: memory_manifest_text,
                    fc_snapshot_version,
                    captured_at: cb.snapshot.created_at,
                    snapshot_bincode,
                })
                .await?;
            label
        }
    };

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
            // A `done` capture was necessarily dispatched, so `host_id` is
            // `Some`; `SnapshotRecord.host_id` is itself `Option`.
            host_id: capture_row.host_id,
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
            // ADR 0068: stamp the capturing host's FC snapshot-version —
            // the job row already carries it (stamped by the host in its
            // terminal report), no separate lookup needed.
            fc_snapshot_version: capture_row.fc_snapshot_version.clone(),
        })
        .await?;

    tracing::info!(
        image_uri = %capture_row.image_uri,
        snapshot_id = %meta.id,
        size_bytes = meta.size_bytes,
        "recorded base snapshot for image",
    );
    let disk_manifest = meta.disk_manifest.ok_or_else(|| {
        ApiError::Internal(format!(
            "base snapshot for `{}` was captured without a chunked disk manifest; \
             residency requires a chunked rootfs — not enabling",
            capture_row.image_uri
        ))
    })?;
    // Memory manifest is optional (migration 0049): FC produces a chunked
    // memory snapshot, VZ cold-boots and captures disk only. Pass through
    // whatever the backend produced — `None` skips memory residency.
    Ok((
        (meta.id, disk_manifest, meta.memory_manifest),
        reuse_outcome,
    ))
}

/// Resolve an image's warm env (`config.warm.env`, ADR 0080) into
/// concrete `name → value` pairs for the `[warm]` hook. Literals pass
/// through; secret refs resolve through the same
/// [`engram_core::traits::SecretStore`] a session uses — the ref is
/// consulted both as a name (org-secret store) and as the schema's
/// deployment `ref` (backends like GCP SM that honor explicit refs).
///
/// FAIL-LOUD (ADR 0080): an unresolvable or erroring ref fails the
/// claim with an actionable error. The pre-0080 behavior (warn + skip)
/// let a missing secret silently bake a corrupt "warm" base snapshot
/// that every session then inherited. ADR 0084 P1b: called from the
/// coordinator's capture-job CLAIM handler (`host_http::
/// claim_capture_job`) instead of from the old direct-RPC capture path
/// — resolution now happens fresh on every claim (including a
/// reassign), never once at job-creation time.
pub(crate) async fn resolve_capture_env(
    state: &SharedState,
    image_uri: &str,
    warm_env: &[engram_core::types::CaptureEnvEntry],
) -> Result<std::collections::HashMap<String, String>, ApiError> {
    use engram_core::types::CaptureEnvValue;
    if warm_env.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let (repo, image_tag) = engram_core::types::session::split_image_ref(image_uri);
    let ctx = engram_core::traits::SecretContext { repo, image_tag };
    let mut out = std::collections::HashMap::with_capacity(warm_env.len());
    for entry in warm_env {
        let value = match &entry.value {
            CaptureEnvValue::Literal { value } => value.clone(),
            CaptureEnvValue::SecretRef { secret_ref } => {
                let schema = engram_core::types::image::SecretSchema {
                    r#ref: Some(secret_ref.clone()),
                    required: true,
                    ..Default::default()
                };
                match state.services.secrets.get(&ctx, secret_ref, &schema).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        return Err(ApiError::BadRequest(format!(
                            "warm env `{}` references secret `{secret_ref}`, which is not \
                             resolvable (checked the org-secret store and the deployment \
                             secret backend). Add the secret or fix the ref, then retry \
                             the enable — capturing without it would bake a corrupt warm \
                             snapshot.",
                            entry.name,
                        )));
                    }
                    Err(e) => {
                        return Err(ApiError::Internal(format!(
                            "warm env `{}`: resolving secret `{secret_ref}` failed: {e}",
                            entry.name,
                        )));
                    }
                }
            }
        };
        out.insert(entry.name.clone(), value);
    }
    Ok(out)
}

// ADR 0080 phase 3b: `parse_disk_manifest_ref`, `ChunkSource`, and
// `materialize_chunk_blob` — the coordinator-side engram-artifact
// chunk push — retired. The `materializing` stage now runs HOST-side
// via the `MaterializeImage` RPC (`materialize_image_on_host` above):
// the host chunks the packed ext4 into its write-through chunk store,
// so the chunks + manifest land in BlobStorage without the coordinator
// ever holding image bytes.

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::{ManifestKind, ManifestRef};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    #[test]
    fn force_recapture_disables_base_snapshot_reuse() {
        let plain: ImageConfig =
            toml::from_str("name = \"plain\"\n[resources]\nsuggested_vcpus = 2\n").unwrap();
        assert!(base_snapshot_reuse_ok(&plain, false));
        assert!(!base_snapshot_reuse_ok(&plain, true));

        let warm: ImageConfig = toml::from_str(
            r#"
            name = "warm"

            [resources]
            suggested_vcpus = 2

            [warm]
            command = ["true"]
            "#,
        )
        .unwrap();
        assert!(!base_snapshot_reuse_ok(&warm, false));
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
