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
) -> Result<engram_core::types::EnableJob, ApiError> {
    config
        .validate()
        .map_err(|e| ApiError::BadRequest(format!("image config for `{image_uri}`: {e}")))?;
    validate_plain_image(state, image_uri).await?;
    let job = state
        .services
        .meta
        .create_or_get_enable_job(image_uri, None, config)
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
    Ok(job)
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
    let (host_id, host) =
        crate::placement::pick_capture_host(state.services.meta.as_ref(), &state.host_registry)
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
    // Issue #539: live `CaptureProgress` events for the whole call.
    // Unused (no events sent) on the content/digest-reuse fast paths
    // below — no host RPC is made there, so there's nothing to report.
    progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
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
    // snapshot.)
    let reuse_ok = config.warm.is_none();

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
    //
    // Issue #192: pin the capture's image reference to the digest we
    // just resolved, NOT `row.image_uri`'s (possibly mutable) tag. The
    // host's local OCI cache is keyed on this URI string; a moving tag
    // (`:latest`) lets a tag-keyed cache hit serve a previous bake's
    // rootfs even though the coord materialized fresh chunks — so the
    // recorded `manifest_digest` and the captured base-snapshot bytes
    // disagree, and the fleet silently keeps booting the old guest. A
    // digest-pinned reference is content-addressed and immutable, so the
    // host pulls (and caches) exactly the resolved bake.
    let capture_uri = engram_oci::digest_pinned_uri(
        &row.image_uri,
        &engram_oci::Digest256(row.manifest_digest.clone()),
    );
    // ADR 0057: base-snapshot capture is a trusted, ephemeral build step (it may
    // run a `[warm]` hook that needs egress), and the captured snapshot is
    // network-agnostic — every session that later restores it gets its own
    // policy network. So capture boots with allow-all egress.
    let capture_network = engram_core::types::image::NetworkPolicy {
        default: engram_core::types::image::NetworkDefault::Allow,
        allow_hosts: Vec::new(),
        allow_host_patterns: Vec::new(),
    };
    // ADR 0080 phase 3b: the capture VM boots from the freshly
    // MATERIALIZED chunked ext4 (`row.disk_manifest`, stamped by the
    // `materializing` stage) — the explicit rootfs-manifest override,
    // the same shape as ADR 0028's disk-only recovery. There is no
    // engram OCI artifact for the host to pull anymore; `capture_uri`
    // stays digest-pinned purely as record-keeping (`spec.image`).
    let spec = crate::api::sessions::cold_boot_spec(
        &capture_uri,
        &config,
        row.disk_manifest,
        capture_network,
    );

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
    // Resolve the capture-time env for the `[warm]` hook (`warm.env`, ADR
    // 0080): literals pass through, secret refs resolve through the same
    // SecretStore a session uses — FAIL-LOUD: an unresolvable ref aborts
    // the capture here rather than baking a corrupt "warm" snapshot. The
    // host receives only resolved values (never the refs). The values
    // flow coord→host→capture-exec and whatever the warm processes
    // persist lands in the base snapshot — which we treat as
    // secret-bearing (see ADR 0007 storage model); the refs themselves
    // never leave the DB.
    let warm_env = config
        .warm
        .as_ref()
        .map(|w| w.env.as_slice())
        .unwrap_or(&[]);
    let capture_env = resolve_capture_env(state, &row.image_uri, warm_env).await?;

    // ADR 0080 (wire v13): assemble the `[warm]` hook's capture egress
    // policy HERE (one egress builder for sessions and captures alike)
    // and ship it ready-to-register; the host stamps the
    // sandbox-dependent identity (sandbox_id, guest IP) at registration.
    // `None` ⇒ the capture VM stays egress-less.
    let capture_egress = config
        .warm
        .as_ref()
        .and_then(|w| w.network.as_ref())
        .and_then(crate::session_boot::assemble_capture_egress_policy);

    // Thread the image's optional `[warm]` hook into capture: the host
    // runs it in the live VM before the snapshot freezes, so a warmed
    // process (e.g. a gradle daemon) is captured into the base snapshot.
    // A warm failure is fail-loud — it surfaces here as a capture error
    // and aborts the enable.
    //
    // Issue #539: `progress` receives live `CaptureProgress` events for
    // the call's lifetime — the caller (`enable_scanner::advance_one`)
    // drains it into a fenced `enable_jobs` write per event.
    let meta = host
        .build_base_snapshot(
            spec,
            config.warm.clone(),
            capture_env,
            capture_egress,
            progress,
        )
        .await
        .map_err(|e| match e {
            engram_core::SandboxError::CaptureFailed(failure) => ApiError::CaptureFailed {
                kind: failure.kind,
                message: format!(
                    "base snapshot capture for `{}` failed on host {host_id}: {failure}",
                    row.image_uri
                ),
            },
            // ADR 0050 C / issue #229: a connect-time transport death
            // (host rolled between `pick_capture_host` and this RPC, or a
            // mixed-version WIRE_VERSION rejection) is the SAME retryable
            // failure class as a mid-stream `WarmExecTransport` — both
            // just mean "didn't reach a live, matching-wire host", and
            // `classify_capture_error` already retries `ApiError::
            // Unavailable` via the attempts budget. Route both here
            // instead of falling into the generic `Internal` (bail-fast)
            // arm, or the enable wedges non-retryable on a transient roll.
            engram_core::SandboxError::Unavailable(msg) => ApiError::Unavailable(format!(
                "base snapshot capture for `{}` could not reach host {host_id}: {msg}",
                row.image_uri
            )),
            engram_core::SandboxError::WireSkew {
                host: host_wire,
                coord,
            } => ApiError::Unavailable(format!(
                "base snapshot capture for `{}` hit a WIRE_VERSION skew against host \
                     {host_id} (host={host_wire}, coord={coord})",
                row.image_uri
            )),
            other => ApiError::Internal(format!(
                "base snapshot capture for `{}` failed on host {host_id}: {other}",
                row.image_uri
            )),
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
    // ADR 0068: stamp the capturing host's FC snapshot-version so a
    // later restore (a fresh `create`, ADR 0020 — there is no warm pool,
    // every create restores this base row) can eventually be paired
    // against it at placement. Best-effort: a lookup failure degrades to
    // NULL (today's unconstrained behavior), never fails the enable.
    let fc_snapshot_version = state
        .services
        .meta
        .fc_snapshot_version_for_host(host_id)
        .await
        .unwrap_or_default();
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
            fc_snapshot_version,
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

/// Resolve an image's warm env (`config.warm.env`, ADR 0080) into
/// concrete `name → value` pairs for the `[warm]` hook. Literals pass
/// through; secret refs resolve through the same
/// [`engram_core::traits::SecretStore`] a session uses — the ref is
/// consulted both as a name (org-secret store) and as the schema's
/// deployment `ref` (backends like GCP SM that honor explicit refs).
///
/// FAIL-LOUD (ADR 0080): an unresolvable or erroring ref fails the
/// capture with an actionable error. The pre-0080 behavior (warn + skip)
/// let a missing secret silently bake a corrupt "warm" base snapshot
/// that every session then inherited.
async fn resolve_capture_env(
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
