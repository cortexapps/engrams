use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use engram_core::traits::{SecretBundle, SecretContext};
use engram_core::types::session::{split_image_ref, ImageRef, SessionMode};
use engram_core::types::{ImageManifest, SecretMode, Session, SessionSpec, SessionState};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Default sandbox sizing for sessions created without explicit limits.
/// Phase 1 numbers — will move to per-repo `engram.toml` config later.
pub(crate) const DEFAULT_VCPUS: u32 = 2;
pub(crate) const DEFAULT_MEMORY_MIB: u32 = 4096;
pub(crate) const DEFAULT_DISK_GIB: u32 = 20;
/// ADR 0027: memory floor for browser-enabled images. chromium-headless-shell
/// needs ~250-400 MB resident; floor at 1 GiB for headroom. The 4 GiB default
/// already exceeds this, so it only bites images that lowered
/// `suggested_memory_mib`. Capture (base snapshot) and restore must agree on
/// `mem_size_mib` (FC requires it), so both paths apply this same floor.
pub(crate) const BROWSER_MEMORY_FLOOR_MIB: u32 = 1024;

/// Resolved guest memory (MiB) for an image: its `suggested_memory_mib` (or
/// the default), floored for `[browser] enabled` images (ADR 0027). The
/// single source of truth shared by base-snapshot capture
/// (`enabled_images`) and session restore — FC requires the restore
/// `mem_size_mib` to equal the snapshot's, so they MUST compute it
/// identically. Don't inline the floor; call this.
pub(crate) fn resolved_memory_mib(manifest: &engram_core::types::ImageManifest) -> u32 {
    manifest
        .resources
        .suggested_memory_mib
        .unwrap_or(DEFAULT_MEMORY_MIB)
        .max(if manifest.browser_enabled() {
            BROWSER_MEMORY_FLOOR_MIB
        } else {
            0
        })
}

/// ADR 0048: resolved guest vCPU count for an image. Enable-time
/// validation (`enabled_images::validate_manifest`) guarantees the
/// declaration is present for enabled images; `DEFAULT_VCPUS` is the
/// defensive fallback for the test / non-enabled paths, mirroring
/// `resolved_memory_mib`. This is the budget placement reserves.
pub(crate) fn resolved_vcpus(manifest: &engram_core::types::ImageManifest) -> u32 {
    manifest.resources.vcpus.unwrap_or(DEFAULT_VCPUS)
}

/// The system's cold-boot `SandboxSpec` shape — a fresh kernel boot
/// (not a snapshot restore) with manifest-derived resources, env, and
/// the ADR 0027 aux bundles (current generations: a fresh boot has no
/// snapshot device model to pin against).
///
/// Two callers, by design the SAME shape (ADR 0028):
/// - base-snapshot capture at image enable (`enabled_images.rs`),
///   `rootfs_manifest = None` — the image's own rootfs;
/// - disk-only cold-boot recovery (`evacuation.rs` Fix B),
///   `rootfs_manifest = Some(live_disk_manifest)` — a fresh kernel
///   mounting the session's evolved rootfs lineage.
pub(crate) fn cold_boot_spec(
    image_uri: &str,
    manifest: &engram_core::types::ImageManifest,
    rootfs_manifest: Option<engram_core::types::manifest::ManifestRef>,
) -> engram_core::types::sandbox::SandboxSpec {
    use engram_core::types::sandbox::{AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};

    let vcpus = resolved_vcpus(manifest);
    let memory_mib = resolved_memory_mib(manifest);
    let disk_gib = manifest
        .resources
        .suggested_disk_gib
        .unwrap_or(DEFAULT_DISK_GIB);

    // ADR 0027: the `skills` bundle is universal; `playwright` rides
    // only when the image opted in.
    let mut aux_ro_drives = vec![AuxRoDrive::skills()];
    if manifest.browser_enabled() {
        aux_ro_drives.push(AuxRoDrive::playwright());
    }

    SandboxSpec {
        image: image_uri.to_string(),
        rootfs_source: None,
        image_uri: Some(image_uri.to_string()),
        rootfs_manifest,
        cpu: CpuLimit { vcpus },
        memory: MemoryLimit {
            max_mib: memory_mib,
        },
        disk: DiskLimit { max_gib: disk_gib },
        ttl: None,
        env: manifest.env.clone(),
        workdir: None,
        network: manifest.network.clone(),
        aux_ro_drives,
    }
}

/// Inject resolved secrets into the sandbox env according to the
/// manifest's `secret_mode`.
///
/// In `Literal` mode, real values land as env vars — fine for the dev
/// loop, never use in production.
///
/// In `Broker` mode, *placeholder* env vars land — the per-session
/// network proxy substitutes the real value only on outbound HTTPS
/// requests to the secret's `allow_hosts`. Today the proxy isn't
/// wired yet, so Broker mode results in placeholders that don't
/// authenticate anything; documented as next-round work in DESIGN.md.
fn apply_secrets_to_env(
    env: &mut HashMap<String, String>,
    bundle: &SecretBundle,
    mode: SecretMode,
    session: SessionId,
) {
    match mode {
        SecretMode::Literal => {
            for (name, resolved) in &bundle.secrets {
                env.insert(name.clone(), resolved.value.clone());
            }
        }
        SecretMode::Broker => {
            // Per-session, per-secret placeholder — the only way the
            // real value can leak via process state is if the proxy
            // is misconfigured. We log every placeholder issue with
            // a SHA-256 prefix so audit logs can correlate without
            // exposing the value.
            for name in bundle.secrets.keys() {
                let placeholder = format!(
                    "engram_ph_{}_{}",
                    session.as_uuid().simple(),
                    short_hash(name),
                );
                env.insert(name.clone(), placeholder);
            }
            // TODO(secrets-broker): register the keyring with the
            // per-session proxy here, and have the proxy substitute
            // placeholders on outbound HTTPS requests whose host
            // matches `schema.allow_hosts` / `schema.allow_host_patterns`.
            // Until that lands, Broker-mode images will see
            // unsubstituted placeholders and any real-API calls fail.
            tracing::warn!(
                %session,
                secret_count = bundle.secrets.len(),
                "secret broker proxy is not yet implemented; \
                 placeholders will not be substituted on outbound traffic",
            );
        }
    }
}

fn short_hash(s: &str) -> String {
    // Stable 8-char tag of `s`, just for placeholder uniqueness. Not
    // a security primitive — placeholders aren't sensitive.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() & 0xffff_ffff)
}

/// Seal the per-request `secrets` overrides under the deployment KEK
/// and persist them keyed by `session_id`. Plaintext is JSON-encoded.
/// Caller is expected to have validated `overrides` is non-empty.
pub(crate) async fn persist_session_secrets(
    state: &SharedState,
    session_id: SessionId,
    overrides: &HashMap<String, String>,
) -> Result<(), ApiError> {
    let plaintext = serde_json::to_vec(overrides)
        .map_err(|e| ApiError::Internal(format!("serialize session secrets: {e}")))?;
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher
        .seal(&plaintext)
        .await
        .map_err(|e| ApiError::Internal(format!("seal session secrets: {e}")))?;
    let row = engram_core::types::registry::SessionSecrets {
        session_id,
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
        created_at: chrono::Utc::now(),
    };
    state.services.meta.upsert_session_secrets(row).await?;
    Ok(())
}

/// Material returned by [`resume_manifest_bundle`] — everything the
/// resume path needs from the image manifest and its secret schema
/// to (1) build the post-resume launch env and (2) rebuild the
/// per-session egress policy (§A.1.7) without a second SecretStore
/// round-trip.
pub(crate) struct ResumeManifestBundle {
    pub manifest: ImageManifest,
    pub bundle: SecretBundle,
    /// `manifest.env` with `apply_secrets_to_env` already applied
    /// (placeholders in Broker mode, raw values in Literal mode).
    /// Per-request overrides from `load_session_secrets` are NOT
    /// folded in here — the caller layers them on top.
    pub env: HashMap<String, String>,
}

/// Re-resolve the image manifest's `[secrets.*]` schema against the
/// deployment's `SecretStore` for a session. Looks up the session's
/// image_uri in `enabled_images`, parses the cached manifest, calls
/// `services.secrets.resolve(...)` with the same `(repo, tag)`
/// context the create path used, and applies the bundle into a fresh
/// env map via the same `apply_secrets_to_env` rules. Returns the
/// parsed manifest + the resolved bundle + the env-with-placeholders
/// so callers that also need to rebuild the egress policy can do so
/// without a second resolve.
///
/// Re-resolving (vs. snapshotting at create) means an operator who
/// rotates a secret in the deployment's secret store mid-session
/// sees the new value land on the next resume — surprising the
/// harness with a stale value would be a bug, not the fix. The
/// caller is expected to fold per-request overrides
/// (CLAUDE_CODE_OAUTH_TOKEN, etc.) on top via [`load_session_secrets`].
pub(crate) async fn resume_manifest_bundle(
    state: &SharedState,
    session: &Session,
) -> Result<ResumeManifestBundle, ApiError> {
    // ADR 0021 P1.8: resume reads via `get_enabled_image_any` so a
    // session whose image was soft-deleted while it was idle still
    // resumes — the chunk lineage is pinned by the soft-deleted
    // row until the (future) refcount-based GC retires it, and the
    // manifest_toml is still on the same row regardless of the
    // soft-delete flag. Only a completely-physically-missing row
    // (no PG entry at all) is terminal; that's the chunk-GC-ran
    // case, which today's deployment can't reach (GC was pulled
    // pre-ADR-0021, per commit 3a3fa50).
    let enabled = state
        .services
        .meta
        .get_enabled_image_any(&session.image)
        .await?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "session image `{}` has no enabled_images row at all (lineage gone — chunk-GC or operator nuke); session is unrecoverable",
                session.image,
            ))
        })?;
    let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
        ApiError::Internal(format!(
            "stored manifest for `{}` failed to parse: {e}",
            session.image,
        ))
    })?;
    let (repo, tag) = split_image_ref(&session.image);
    let secret_ctx = SecretContext {
        repo,
        image_tag: tag,
    };
    let bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest.secrets, None)
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;
    let mut env: HashMap<String, String> = manifest.env.clone();
    apply_secrets_to_env(&mut env, &bundle, manifest.secret_mode, session.id);
    Ok(ResumeManifestBundle {
        manifest,
        bundle,
        env,
    })
}

/// The full launch environment for an existing session: the image
/// manifest's `[env]` + resolved `[secrets]` (via
/// [`resume_manifest_bundle`]) with the per-request secret overrides
/// from [`load_session_secrets`] layered on top — exactly the env the
/// harness is (re)spawned with on resume.
///
/// Shared by two callers that both need this assembled identically:
/// the resume path (which also wants the [`ResumeManifestBundle`] for
/// the egress-policy rebuild + harness resolution) and `/exec` (which
/// wants the env + the manifest `workdir`). Returning the bundle as
/// well as the folded env lets `/exec` read `manifest.workdir` without
/// a second load.
///
/// Best-effort by design: a manifest-load failure (warn-logged in
/// [`resume_manifest_bundle`]) yields `(None, request-or-default env)`,
/// and a secrets-load failure is warn-logged and skipped — neither
/// blocks the caller. This keeps `/exec` working (request-only env, the
/// pre-injection behaviour) even if the image lineage is degraded.
pub(crate) async fn resolve_session_env(
    state: &SharedState,
    session: &Session,
) -> (Option<ResumeManifestBundle>, HashMap<String, String>) {
    let bundle = match resume_manifest_bundle(state, session).await {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(
                session_id = %session.id,
                error = %e,
                "resolve_session_env: manifest bundle load failed; launch env falls back to overrides-only",
            );
            None
        }
    };
    let mut env = bundle.as_ref().map(|b| b.env.clone()).unwrap_or_default();
    match load_session_secrets(state, session.id).await {
        Ok(Some(overrides)) => {
            for (k, v) in overrides {
                env.insert(k, v);
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                session_id = %session.id,
                error = %e,
                "resolve_session_env: per-request secret overrides unavailable; continuing without them",
            );
        }
    }

    // ADR 0031: re-apply the owner's git attribution + auto-injected Claude
    // token on resume (the per-resume harness child is a fresh process, so it
    // needs these in its env again). Keyed on the session's stored owner;
    // best-effort — an unparseable/legacy user_id or missing auth runtime just
    // skips it.
    if let (Some(rt), Some(uid_str)) = (state.auth.as_ref(), session.user_id.as_ref()) {
        if let Ok(uuid) = uid_str.parse::<uuid::Uuid>() {
            match rt.users.get_user(engram_core::UserId(uuid)).await {
                Ok(user) => {
                    let principal = user.to_principal();
                    // Git [user] attribution is human-initiated sessions only —
                    // the service principal's service_email isn't a valid commit
                    // author (GitHub rejects the squash-merge).
                    if !principal.is_service(&rt.config.service_email) {
                        env.insert("ENGRAM_USER_EMAIL".into(), principal.email.clone());
                        env.insert("ENGRAM_USER_NAME".into(), principal.git_name());
                    }
                    let harness = bundle.as_ref().and_then(|b| b.manifest.harness.as_ref());
                    inject_user_claude_token(state, &principal, harness, session.mode, &mut env)
                        .await;
                }
                Err(e) => tracing::warn!(
                    session_id = %session.id,
                    error = %e,
                    "resolve_session_env: owner lookup failed; resume env omits attribution + token",
                ),
            }
        }
    }

    (bundle, env)
}

/// ADR 0016 §A.1.7: rebuild the post-resume [`SessionEgressPolicy`]
/// from a previously-loaded [`ResumeManifestBundle`] + the new
/// sandbox's guest IP. Returns `None` when the host doesn't know a
/// guest IP for this sandbox (process backend, VZ in some configs)
/// or when the IP isn't parseable as v4 — callers then fall through
/// to the legacy unspecified-IP placeholder, matching the pre-fix
/// behaviour.
///
/// Async only because of `host.guest_ip`. Pure assembly delegated to
/// [`assemble_resume_egress_policy`] for testability.
pub(crate) async fn build_resume_egress_policy(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: engram_core::SandboxId,
    bundle: &SecretBundle,
    manifest: &ImageManifest,
    env_with_placeholders: &HashMap<String, String>,
) -> Option<engram_core::types::egress::SessionEgressPolicy> {
    let guest_ip_str = state.services.host.guest_ip(sandbox_id).await?;
    let guest_ip = guest_ip_str.parse::<std::net::Ipv4Addr>().ok()?;
    Some(assemble_resume_egress_policy(
        session_id,
        sandbox_id,
        guest_ip,
        bundle,
        manifest,
        env_with_placeholders,
    ))
}

/// Pure synchronous assembly path for the resume egress policy.
/// Split out from [`build_resume_egress_policy`] so unit tests can
/// exercise the placeholder-matching + allow-list cloning logic
/// without standing up a SharedState. The behaviour mirrors the
/// create path's policy build (sessions.rs around line 635) by
/// construction.
pub(crate) fn assemble_resume_egress_policy(
    session_id: SessionId,
    sandbox_id: engram_core::SandboxId,
    guest_ip: std::net::Ipv4Addr,
    bundle: &SecretBundle,
    manifest: &ImageManifest,
    env_with_placeholders: &HashMap<String, String>,
) -> engram_core::types::egress::SessionEgressPolicy {
    let mut secrets = Vec::new();
    for (name, resolved) in &bundle.secrets {
        // The env's value for this secret IS the placeholder the
        // proxy will see in-VM. In Broker mode this is
        // `engram_ph_*`; in Literal mode it's the raw value. Either
        // way, the proxy substitutes when an outbound request's
        // body / header matches it.
        let Some(placeholder) = env_with_placeholders.get(name).cloned() else {
            continue;
        };
        secrets.push(engram_core::types::egress::EgressSecretEntry {
            placeholder,
            real_value: resolved.value.clone(),
            allow_hosts: resolved.schema.allow_hosts.clone(),
            allow_host_patterns: resolved.schema.allow_host_patterns.clone(),
        });
    }
    engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip,
        network_allow_hosts: manifest.network.allow_hosts.clone(),
        network_allow_host_patterns: manifest.network.allow_host_patterns.clone(),
        secrets,
        secret_mode: manifest.secret_mode,
    }
}

/// Reverse of [`persist_session_secrets`]: fetch the sealed row,
/// open it under the deployment KEK, and return the original
/// `(name, value)` map. `Ok(None)` when no row exists (the session
/// either had no overrides at create or was created before this
/// persistence path landed).
pub(crate) async fn load_session_secrets(
    state: &SharedState,
    session_id: SessionId,
) -> Result<Option<HashMap<String, String>>, ApiError> {
    let row = state.services.meta.get_session_secrets(session_id).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let nonce: [u8; 12] = row
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Internal("session_secrets.nonce wrong length".into()))?;
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: row.wrapped_dek,
        nonce,
        ciphertext: row.ciphertext,
        key_id: row.key_id,
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plaintext = cipher
        .open(&sealed)
        .await
        .map_err(|e| ApiError::Internal(format!("open session secrets: {e}")))?;
    let map: HashMap<String, String> = serde_json::from_slice(&plaintext)
        .map_err(|e| ApiError::Internal(format!("deserialize session secrets: {e}")))?;
    Ok(Some(map))
}

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    /// Which baked image to boot. Required.
    pub image: ImageRef,
    /// How the session uses the image (ADR 0021 P1.3). Defaults to
    /// `SessionMode::Agent` — drive the image's baked harness if it
    /// has one. `SessionMode::DevVm` keeps a harnessed image's agent
    /// resident-but-undriven; the user interacts via shell /
    /// `engram exec`. Per-session *selection* of which harness to
    /// run is gone (the harness is baked at image-bake time).
    #[serde(default)]
    pub mode: SessionMode,
    // ADR 0031: `user_id` is no longer client-supplied — the owner is stamped
    // server-side from the authenticated principal (closes a trivial
    // impersonation hole).
    /// Initial prompt for the agent. Only meaningful when
    /// `mode = Agent` and the image carries a `[harness]` block;
    /// the API rejects with 400 when set alongside `mode = DevVm`
    /// (silent drop is a footgun).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Per-request secret values keyed by env-var name. Only
    /// honored under `SecretMode::Literal`; broker-mode images
    /// reject overrides.
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
    pub status: &'static str,
    pub image_version: String,
    /// `"warm"` if the session was satisfied by a pre-restored
    /// warm-pool slot, `"cold"` if it took the full create path.
    /// Used both by the dashboard (badge in session detail) and by
    /// the metrics wrapper to label `engram_session_boot_seconds`
    /// without re-running the scheduling decision.
    pub kind: &'static str,
}

// Root span of the cold-boot distributed trace (ADR 0019). Every
// downstream gRPC call to a host inherits this span's `traceparent` via
// `TraceparentInjector`, so the coord → host-agent → firecracker →
// uffd-handler timeline stitches into one trace. Inert (no export) unless
// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, but the span is always created so
// propagation works the moment a collector is wired up.
#[tracing::instrument(name = "session.create", skip_all)]
pub async fn create_session(
    state: State<SharedState>,
    current: crate::api::principal::CurrentUser,
    req: Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let start = std::time::Instant::now();
    let result = create_session_inner(state, current, req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let outcome = match &result {
        Ok(_) => "success",
        Err(ApiError::BadRequest(_)) => "bad_request",
        Err(ApiError::NotFound(_)) => "image_not_enabled",
        Err(ApiError::Unavailable(_)) => "scheduling_rejected",
        Err(_) => "internal",
    };
    // Read kind off the success response; on error we don't know
    // which path was attempted (warm-lease may have errored before
    // we knew to fall through, or the spec failed validation
    // pre-scheduling), so label as `unknown`.
    let kind = match &result {
        Ok((_, body)) => body.kind,
        Err(_) => "unknown",
    };
    metrics::histogram!(
        crate::metrics::SESSION_BOOT_SECONDS,
        "phase" => "total",
        "outcome" => outcome,
        "kind" => kind,
    )
    .record(elapsed);
    metrics::counter!(
        crate::metrics::SESSION_CREATE_TOTAL,
        "outcome" => outcome,
    )
    .increment(1);
    result
}

async fn create_session_inner(
    State(state): State<SharedState>,
    crate::api::principal::CurrentUser(principal): crate::api::principal::CurrentUser,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    // Resolve manifest / secrets / env / harness from the request.
    let prepared = prepare_from_request(&state, &principal, &req).await?;
    let crate::session_boot::PreparedBoot {
        inputs,
        memory_mib,
        cpu_budget_vcpus,
        image_repo,
        image_tag,
    } = prepared;
    let session_id = inputs.session_id;
    let base_snapshot_id = inputs.base_snapshot_id;

    // -------- Reserve a host (ADR 0046 best-fit, ADR 0048 2D) --------
    let ctx = crate::placement::ScheduleContext {
        repo: &image_repo,
        image_version: &image_tag,
        prefer_snapshot_id: Some(base_snapshot_id),
        memory_mib: Some(memory_mib),
        cpu_budget_vcpus: Some(cpu_budget_vcpus),
        required_image_digest: None,
        exclude_host: None,
        prefer_host: None,
    };
    let candidates = crate::placement::candidates_for(state.services.meta.as_ref(), &ctx)
        .await
        .map_err(engram_core::SandboxError::from)?;
    let host_id = match state
        .services
        .meta
        .reserve_placement(
            session_id,
            &inputs.spec,
            memory_mib as i64,
            cpu_budget_vcpus as i32,
            &candidates.hosts,
            candidates.affinity_len,
        )
        .await
        .map_err(|e| ApiError::Internal(format!("reserve_placement: {e}")))?
    {
        Some(h) => h,
        // ADR 0048: no host fits → QUEUE (FIFO) instead of 503. The queue
        // scanner re-attempts placement as capacity frees / the fleet
        // scales up, and drives the same boot path once a host fits.
        None => {
            return enqueue_create(&state, inputs, &image_tag).await;
        }
    };

    // -------- Boot on the reserved host --------
    //
    // Issue #210: DETACH the boot pipeline from the cancellable request
    // future. `boot_on_reserved_host` makes a LIVE sandbox in
    // `restore_base_on_host` (session_boot.rs) and only later records it via
    // `create_session_created`; the compensating `host.destroy` runs solely
    // on the row-insert `Err` arm, never on a dropped future. Awaited INLINE,
    // a client disconnect between the restore and the insert (a slow restore,
    // up to a 240s deadline) drops the future: a running sandbox is left with
    // no recorded binding AND the disposition below — which releases the
    // `pending` reservation / fails the session — never runs, so the reserved
    // capacity stays pinned too.
    //
    // Mirror the resume lane (and ADR 0034 / the #208 teleport detachment):
    // run the boot AND its full disposition in a `tokio::spawn`ed task so the
    // sandbox is always recorded or torn down and the reservation is always
    // released, regardless of the request's fate. The handler awaits the
    // JoinHandle only to shape the connected client's response.
    let st = state.clone();
    let boot_handle = tokio::spawn(async move {
        match crate::session_boot::boot_on_reserved_host(&st, inputs, host_id).await {
            Ok(()) => Ok(()),
            Err(crate::session_boot::BootError::NotStarted(e)) => {
                // The sandbox never came up (or was torn down on the insert
                // failure); release the reservation row so the host's free
                // capacity is restored at once (reconcile would also reap it).
                // No Failed transition — nothing usable ever existed.
                if let Err(de) = st.services.meta.delete_pending_session(session_id).await {
                    tracing::warn!(%session_id, error = %de,
                        "delete_pending_session after boot failure failed; reconcile will reap");
                }
                Err(e)
            }
            Err(crate::session_boot::BootError::Started(e)) => {
                // The sandbox booted but a later step failed — fail the
                // session (the sandbox was already unbound by the boot pipeline).
                let _ = st
                    .services
                    .meta
                    .transition_session(session_id, SessionState::Failed)
                    .await;
                Err(e)
            }
        }
    });

    let boot_result = boot_handle.await.map_err(|join_err| {
        // The boot task panicked: it did NOT run its disposition, so the
        // pending reservation may still be pinned. The placement reconcile /
        // stale-pending guard (10 min) reclaims it; surface a 500 so the
        // client doesn't believe the session is live.
        ApiError::Internal(format!("session boot task panicked: {join_err}"))
    })?;

    boot_result.map(|()| {
        (
            StatusCode::CREATED,
            Json(CreateSessionResponse {
                session_id,
                status: SessionState::Active.as_str(),
                image_version: image_tag,
                // ADR 0020: every session is a base-snapshot restore now.
                kind: "restored",
            }),
        )
    })
}

/// ADR 0048: enqueue a create that found no capacity. INSERTs the row at
/// `queued` (carrying the budgets + prompt the scanner reconstructs from),
/// seals any per-request secret overrides now the FK is satisfiable, emits
/// `Pending → Queued`, and returns 201 `{status:"queued"}`. The handler
/// NEVER blocks — the `queue_scanner` owns the continuation.
async fn enqueue_create(
    state: &SharedState,
    inputs: crate::session_boot::BootInputs,
    image_tag: &str,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let session_id = inputs.session_id;
    state
        .services
        .meta
        .enqueue_session_create(
            session_id,
            &inputs.spec,
            // The budgets the scanner will reserve with — same values create
            // computed, so the queued demand signal is exact.
            resolved_budget_mib(&inputs),
            resolved_budget_vcpus(&inputs),
            inputs.prompt.as_deref(),
        )
        .await
        .map_err(|e| ApiError::Internal(format!("enqueue_session_create: {e}")))?;
    // Seal per-request overrides now the row (FK target) exists.
    if let Some(overrides) = inputs.deferred_session_secrets.as_ref() {
        if let Err(e) = persist_session_secrets(state, session_id, overrides).await {
            tracing::warn!(%session_id, error = %e,
                "queued session secrets persist failed; resume/boot will lose overrides");
        }
    }
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Pending,
                to: SessionState::Queued,
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(%session_id, error = %e, "emit pending→queued failed; continuing");
    }
    tracing::info!(%session_id, "no capacity — session queued for placement (ADR 0048)");
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id,
            status: SessionState::Queued.as_str(),
            image_version: image_tag.to_string(),
            kind: "queued",
        }),
    ))
}

/// The session's memory budget, recovered from the env baked into
/// `BootInputs` (it isn't stored separately — `resolved_memory_mib`
/// is the source of truth, recomputed identically by the scanner).
fn resolved_budget_mib(inputs: &crate::session_boot::BootInputs) -> i64 {
    inputs.memory_mib as i64
}
fn resolved_budget_vcpus(inputs: &crate::session_boot::BootInputs) -> i32 {
    inputs.cpu_budget_vcpus as i32
}

/// ADR 0048 C5/C6: resolve a session's manifest / secrets / env / harness
/// from a live create request. Mints a fresh `SessionId`. Shares
/// [`prepare_inner`] with the queue scanner's [`prepare_from_row`] so the
/// two paths can't drift.
pub(crate) async fn prepare_from_request(
    state: &SharedState,
    principal: &engram_core::types::user::Principal,
    req: &CreateSessionRequest,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    // Strict lookup: a create may only target a LIVE enabled image
    // (a soft-deleted image still resumes idle sessions, but can't
    // accept new ones).
    let enabled = state
        .services
        .meta
        .get_enabled_image(&req.image)
        .await
        .map_err(|e| ApiError::Internal(format!("enabled_images lookup: {e}")))?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "image `{}` is not enabled. Operators enable images via \
                 POST /api/enabled-images before sessions can reference them.",
                req.image
            ))
        })?;
    prepare_inner(
        state,
        Some(principal.user_id.to_string()),
        Some(principal),
        &req.image,
        req.mode,
        req.prompt.clone(),
        req.secrets.clone(),
        SessionId::new(),
        enabled,
    )
    .await
}

/// ADR 0048 C6: resolve the same boot inputs from a durable `queued` row,
/// so the queue scanner can boot a session no live request is holding.
/// The owner principal is reconstructed from `session.user_id`; secret
/// overrides come from the sealed `session_secrets` row; the prompt from
/// `queue_prompt`. Tolerant image lookup (the image may have been
/// soft-deleted while the session waited — the lineage is still pinned).
pub(crate) async fn prepare_from_row(
    state: &SharedState,
    session: &Session,
    prompt: Option<String>,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    let principal = principal_for_session(state, session).await;
    let overrides = load_session_secrets(state, session.id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(session_id = %session.id, error = %e,
            "prepare_from_row: secret overrides unavailable; continuing without them");
            None
        });
    let enabled = state
        .services
        .meta
        .get_enabled_image_any(&session.image)
        .await
        .map_err(|e| ApiError::Internal(format!("enabled_images lookup: {e}")))?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "queued session `{}` image `{}` has no enabled_images row (lineage gone)",
                session.id, session.image
            ))
        })?;
    prepare_inner(
        state,
        session.user_id.clone(),
        principal.as_ref(),
        &session.image,
        session.mode,
        prompt,
        overrides,
        session.id,
        enabled,
    )
    .await
}

/// Reconstruct the owning `Principal` from a session's stored `user_id`
/// (mirrors `resolve_session_env`'s owner lookup). `None` when there's no
/// auth runtime, the id is unparseable, or the user row is gone — callers
/// then skip git attribution + the auto-injected Claude token, exactly as
/// resume does.
async fn principal_for_session(
    state: &SharedState,
    session: &Session,
) -> Option<engram_core::types::user::Principal> {
    let rt = state.auth.as_ref()?;
    let uid = session.user_id.as_ref()?;
    let uuid = uid.parse::<uuid::Uuid>().ok()?;
    match rt.users.get_user(engram_core::UserId(uuid)).await {
        Ok(user) => Some(user.to_principal()),
        Err(e) => {
            tracing::warn!(session_id = %session.id, error = %e,
                "principal_for_session: owner lookup failed; boot omits attribution + token");
            None
        }
    }
}

/// Shared resolution for both create entry points (ADR 0048 C5/C6). Mirrors
/// the original create phases: harness-launchable gate → resolve secrets →
/// build spec_env / session_env / agent / network → resolve the base
/// snapshot + budgets. `user_id` stamps the row's owner; `principal` (when
/// Some and non-service) drives git attribution + the Claude token.
#[allow(clippy::too_many_arguments)]
async fn prepare_inner(
    state: &SharedState,
    user_id: Option<String>,
    principal: Option<&engram_core::types::user::Principal>,
    image_uri: &str,
    mode: SessionMode,
    prompt: Option<String>,
    secret_overrides: Option<HashMap<String, String>>,
    session_id: SessionId,
    enabled: engram_core::types::EnabledImage,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    // ADR 0021 P1.3: a dev-VM session leaves any baked harness undriven,
    // so a prompt is meaningless — reject it explicitly.
    if mode.is_dev_vm() && !prompt.as_deref().map(str::is_empty).unwrap_or(true) {
        return Err(ApiError::BadRequest(
            "`prompt` requires `mode = agent` — a dev-VM session has no agent to receive it".into(),
        ));
    }

    let (image_repo, image_tag) = {
        let (r, t) = split_image_ref(image_uri);
        (r.to_string(), t.to_string())
    };
    let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
        ApiError::Internal(format!(
            "stored manifest for {image_uri} failed to parse: {e}"
        ))
    })?;
    if let Some(h) = manifest.harness.as_ref() {
        if !h.is_launchable() && !mode.is_dev_vm() {
            return Err(ApiError::Internal(format!(
                "enabled image `{image_uri}` has a [harness] block that didn't resolve \
                 (name/exec unset) — re-bake the image"
            )));
        }
    }

    // -------- Resolve secrets --------
    let secret_ctx = SecretContext {
        repo: &image_repo,
        image_tag: &image_tag,
    };
    if secret_overrides
        .as_ref()
        .map(|m| !m.is_empty())
        .unwrap_or(false)
        && manifest.secret_mode != engram_core::types::image::SecretMode::Literal
    {
        return Err(ApiError::BadRequest(
            "per-request `secrets` are only supported for `secret_mode = literal` images".into(),
        ));
    }
    let secret_bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest.secrets, secret_overrides.as_ref())
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;

    let spec = SessionSpec {
        image: image_uri.to_string(),
        mode,
        user_id,
    };

    // -------- Build the sandbox env --------
    let mut spec_env: HashMap<String, String> = manifest.env.clone();
    apply_secrets_to_env(
        &mut spec_env,
        &secret_bundle,
        manifest.secret_mode,
        session_id,
    );

    let deferred_session_secrets: Option<HashMap<String, String>> =
        if manifest.secret_mode == engram_core::types::image::SecretMode::Literal {
            if let Some(overrides) = secret_overrides.as_ref() {
                for (name, value) in overrides {
                    spec_env.insert(name.clone(), value.clone());
                }
                if overrides.is_empty() {
                    None
                } else {
                    Some(overrides.clone())
                }
            } else {
                None
            }
        } else {
            None
        };

    if let Some(h) = manifest.harness.as_ref() {
        if let Some(name) = h.name.as_deref() {
            spec_env.insert("ENGRAM_SESSION_HARNESS_NAME".into(), name.to_string());
        }
    }

    let mut session_env = spec_env.clone();
    session_env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());

    // ADR 0031: git attribution + Claude token — only for a resolved,
    // non-service principal (mirrors create + resume).
    if let Some(p) = principal {
        let is_service = state
            .auth
            .as_ref()
            .is_some_and(|rt| p.is_service(&rt.config.service_email));
        if !is_service {
            session_env.insert("ENGRAM_USER_EMAIL".into(), p.email.clone());
            session_env.insert("ENGRAM_USER_NAME".into(), p.git_name());
        }
        inject_user_claude_token(state, p, manifest.harness.as_ref(), mode, &mut session_env).await;
    }

    let mut agent = resolve_harness(
        state,
        manifest.harness.as_ref(),
        mode,
        session_id,
        prompt.as_deref(),
        session_env.clone(),
        manifest.workdir.clone(),
    )?;
    if let Some(a) = agent.as_mut() {
        inject_harness_env(state, session_id, manifest.git.as_ref(), &mut a.env).await;
    }

    let network = manifest.network.clone();

    // -------- Base snapshot + budgets --------
    let base_snapshot_id = enabled.base_snapshot_id.ok_or_else(|| {
        ApiError::Internal(format!(
            "enabled image `{image_uri}` has no base snapshot — re-enable it \
             (POST /api/enabled-images) to capture one"
        ))
    })?;
    let memory_mib = resolved_memory_mib(&manifest);
    let cpu_budget_vcpus = resolved_vcpus(&manifest);

    Ok(crate::session_boot::PreparedBoot {
        inputs: crate::session_boot::BootInputs {
            session_id,
            spec,
            base_snapshot_id,
            spec_env,
            agent,
            session_env,
            secret_bundle,
            network,
            secret_mode: manifest.secret_mode,
            deferred_session_secrets,
            prompt: prompt.filter(|s| !s.is_empty()),
            memory_mib,
            cpu_budget_vcpus,
        },
        memory_mib,
        cpu_budget_vcpus,
        image_repo,
        image_tag,
    })
}
/// ADR 0020 P1: attempt to restore a session from the image's base
/// snapshot, late-binding the session harness. Builds the
/// `SnapshotMetadata` from the base snapshot's `snapshots` row (the
/// portable blob keys are deterministic from `snapshot_id`), picks a
/// host, and runs the combined restore + harness-swap op. Any error
/// bubbles to the caller, which falls back to a cold create — so this
/// never fails a session, it only declines to fast-path it.
/// ADR 0039 (cache locality): the BlobStorage key for a base image's
/// *canonical* working-set trace (`traces/<memory_manifest_id>/canonical.json`,
/// published by the image-builder's profile pass), or `None` when the base
/// snapshot has no memory image.
///
/// When set on the restore `SnapshotMetadata`, the host narrows its
/// memory-chunk prefetch from "all base-memory chunks" to "just the chunks
/// the kernel faults in the first ~5 s" — the REAP-style warm set
/// (`prefetch_memory_chunks` in the pooled backend). Keyed off the *memory*
/// manifest because the trace describes memory faults; disk-only / cold-boot
/// base snapshots (VZ) have no memory image, so there is nothing to narrow.
///
/// Safe before any bake has published a canonical trace: a missing blob is
/// treated as an empty working set and the host falls back to full-manifest
/// prefetch (the prior, behaviour-preserving path).
pub(crate) fn base_working_set_blob_key(
    memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
) -> Option<String> {
    memory_manifest
        .map(|m| engram_chunk_store::working_set::TraceRef::canonical(m.manifest_id).storage_key())
}

/// ADR 0031: inject the initiating user's saved Claude Code OAuth token into
/// the session env, but only when the resolved harness is built-in Claude and
/// the session drives it (agent mode). Best-effort — a missing or unopenable
/// token is logged, never fatal, so the harness can still fall back to its own
/// login path. Shared by the create and resume paths.
async fn inject_user_claude_token(
    state: &SharedState,
    principal: &engram_core::types::user::Principal,
    harness: Option<&engram_core::types::image::HarnessManifest>,
    mode: SessionMode,
    session_env: &mut std::collections::HashMap<String, String>,
) {
    let is_builtin_claude =
        harness.and_then(|h| h.name.as_deref()) == Some("claude") && !mode.is_dev_vm();
    if !is_builtin_claude {
        return;
    }
    let Some(rt) = state.auth.as_ref() else {
        return;
    };
    let kind = engram_core::types::user::UserToken::KIND_CLAUDE_OAUTH;
    match rt.users.get_user_token(principal.user_id, kind).await {
        Ok(Some(tok)) => {
            match engram_auth::open_user_token(state.services.kek.as_ref(), &tok).await {
                Ok(plain) => {
                    session_env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), plain);
                }
                Err(e) => {
                    tracing::warn!(user_id = %principal.user_id, error = %e, "could not open saved Claude token")
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(user_id = %principal.user_id, error = %e, "could not load saved Claude token")
        }
    }
}

pub async fn get_session(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<Session>, ApiError> {
    let s = state.services.meta.get_session(id).await?;
    Ok(Json(s))
}

/// One row of the session list. Flattens the `Session` (so existing
/// consumers see the same fields) and adds the owner's identity for the
/// admin "All sessions" view's owner chips. `owner_*` is `None` in the
/// "mine" view (the owner is implicit — you).
#[derive(Serialize)]
pub struct SessionListItem {
    #[serde(flatten)]
    pub session: Session,
    pub owner_email: Option<String>,
    pub owner_name: Option<String>,
}

#[derive(Serialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionListItem>,
}

#[derive(serde::Deserialize, Default)]
pub struct ListSessionsParams {
    /// `mine` (default) — the caller's own sessions. `all` — every session
    /// (admin only); rows carry owner identity for attribution.
    #[serde(default)]
    pub scope: Option<String>,
}

/// `GET /sessions` — owner-scoped (ADR 0031). A member sees only their own
/// sessions; an admin sees their own (`scope=mine`, default) or everyone's
/// (`scope=all`). Returns only live rows — terminal states and
/// `host_lost` are excluded (what `list_active_sessions` selects).
pub async fn list_sessions(
    State(state): State<SharedState>,
    crate::api::principal::CurrentUser(principal): crate::api::principal::CurrentUser,
    axum::extract::Query(params): axum::extract::Query<ListSessionsParams>,
) -> Result<Json<ListSessionsResponse>, ApiError> {
    let show_all = match params.scope.as_deref().unwrap_or("mine") {
        "all" => {
            if !principal.is_admin() {
                return Err(ApiError::Forbidden(
                    "scope=all requires the admin role".into(),
                ));
            }
            true
        }
        // "mine" or anything else → own sessions only.
        _ => false,
    };

    let all = state.services.meta.list_active_sessions().await?;
    let mine = principal.user_id.to_string();
    let filtered: Vec<Session> = if show_all {
        all
    } else {
        all.into_iter()
            .filter(|s| s.user_id.as_deref() == Some(mine.as_str()))
            .collect()
    };

    // For the All view, attach owner identity (one batch lookup → map).
    let owners: HashMap<String, (Option<String>, String)> = if show_all {
        match state.auth.as_ref() {
            Some(rt) => rt
                .users
                .list_users()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|u| (u.id.to_string(), (u.display_name, u.email)))
                .collect(),
            None => HashMap::new(),
        }
    } else {
        HashMap::new()
    };

    let sessions = filtered
        .into_iter()
        .map(|s| {
            let (owner_name, owner_email) = s
                .user_id
                .as_ref()
                .and_then(|uid| owners.get(uid))
                .map(|(name, email)| (name.clone(), Some(email.clone())))
                .unwrap_or((None, None));
            SessionListItem {
                session: s,
                owner_email,
                owner_name,
            }
        })
        .collect();
    Ok(Json(ListSessionsResponse { sessions }))
}

pub async fn delete_session(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    // Drive the session to its FSM-legal terminal BEFORE destroying the
    // sandbox. `terminate_session` reads the current state and picks the
    // terminal `SessionState::terminal_target` permits — `Completed` for
    // states that ran, `Failed` for the early states (Pending / Created /
    // GuestReady) that never became usable (this is what fixes the
    // `5fadd364` phantom: deleting a `Created` session used to drive an
    // illegal Created→Completed that surfaced as Conflict, destroying the
    // sandbox but leaving the row non-terminal). Terminating first also
    // removes this row from the heartbeat reconcile's "active session whose
    // sandbox is missing" view, so reconcile can't race us into HostLost
    // during the (best-effort, can-take-seconds) destroy RPC below.
    //
    // - `Ok(None)`: already terminal — idempotent 204, nothing to tear down.
    // - `Ok(Some((prev, target)))`: transitioned; emit + drop the broker
    //   token, then destroy.
    // - `Conflict`: a sibling (drain, dead-host detector, eviction scanner)
    //   raced us to terminal — best-effort tear down, then 204. (ADR 0034:
    //   deleting mid-eviction works this way — the scanner's racing
    //   transition_session(Idle) then fails against the terminal row, fires
    //   abort_inflight_snapshot, and releases the lease.)
    //
    // An unknown id surfaces as 404 from `terminate_session`'s own
    // `get_session` — matching the get/exec contract — before any teardown.
    //
    // Capture the live sandbox binding (PG authority, read through the
    // per-replica cache) BEFORE the terminal transition clears it, so the
    // teardown below works on any replica (ADR 0047) — not just the pod
    // that cached the bind. Without this, a delete fielded by a non-owning
    // replica would `unbind` nothing and leak the sandbox (host reconcile
    // GC is the backstop, but we tear down promptly here).
    let bound_sandbox = state.resolve_sandbox(id).await;
    match state.services.meta.terminate_session(id).await {
        Ok(None) => return Ok(StatusCode::NO_CONTENT),
        Ok(Some((prev, target))) => {
            state
                .emit(
                    id,
                    SessionEvent::StatusChanged {
                        from: prev,
                        to: target,
                        at: chrono::Utc::now(),
                    },
                )
                .await?;
            // ADR 0023: drop the session's credential-broker token so a
            // terminated session can no longer mint git credentials.
            // ADR 0047: the PG row is the authority; the map is a cache.
            state.git_broker_tokens.remove(&id);
            if let Err(e) = state.services.meta.delete_broker_token(id).await {
                tracing::warn!(session_id = %id, error = %e,
                    "delete_broker_token failed; ON DELETE CASCADE is the backstop");
            }
        }
        Err(engram_core::MetaError::Conflict(msg)) => {
            tracing::info!(
                session_id = %id,
                error = %msg,
                "delete_session: state machine raced us (likely reconciler flipped to terminal first); returning 204 idempotently"
            );
            // Still tear down whatever's left for tidiness, then 204.
            if let Some(sandbox_id) = bound_sandbox {
                let _ = state.services.host.destroy(sandbox_id).await;
            }
            state.services.host.unbind_session(id).await;
            return Ok(StatusCode::NO_CONTENT);
        }
        Err(e) => return Err(e.into()),
    }

    // Status is now terminal; reconcile won't touch this row anymore.
    // Now tear down the sandbox and clear the routing columns.
    if let Some(sandbox_id) = bound_sandbox {
        if let Err(e) = state.services.host.destroy(sandbox_id).await {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during session delete; host-agent reconcile will GC",
            );
        }
        // ADR 0006: the host-agent unregisters its local proxy
        // entry as part of `destroy`. No coordinator-side cleanup.
    }

    // Clear sandbox_id since the sandbox is destroyed; the session
    // row is now terminal and won't be repopulated by startup
    // routing rebuild even if it kept the column set, but tidy
    // anyway so an audit query "what sandboxes does the coordinator
    // think exist" matches reality.
    //
    // Issue #211: guard the clear on the EXACT sandbox we read +
    // destroyed (`bound_sandbox`). The row is terminal, so a competing
    // rebind onto it can't happen anymore — but a stale read elsewhere
    // shouldn't be able to null a column that something else legitimately
    // re-populated either. `Some(bound_sandbox)` means "only clear if the
    // row still points at the sandbox I destroyed"; a `Conflict` (the
    // binding already moved on) is benign here, so it's logged not failed.
    if let Some(sandbox_id) = bound_sandbox {
        if let Err(e) = state
            .services
            .meta
            .assign_session_sandbox_guarded(id, None, Some(Some(sandbox_id)), &[])
            .await
        {
            tracing::debug!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "delete_session: guarded sandbox clear was a no-op (binding already \
                 changed) — leaving it for the new owner",
            );
        }
    }
    state.services.host.unbind_session(id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// ADR 0023: when a forge is configured and the image declares `[git]`,
/// mint a per-session credential-broker token (stored in
/// `state.git_broker_tokens`) and inject the forge env so the in-session
/// `GIT_ASKPASS` helper can fetch credentials + open PRs. No-op when no
/// forge is configured or the image has no `[git]` block. Re-minting on
/// resume is fine — the latest token wins.
/// Mint-or-reuse the session's broker token (stored in
/// `state.git_broker_tokens`). Idempotent: a later call from the
/// `/exec` path returns the SAME token rather than orphaning a fresh
/// one. ADR 0023 minted it for the forge seam; ADR 0026 reuses it for
/// the artifact-upload seam (the name is historical). The
/// `loopback_endpoint` (`http://127.0.0.1:<port>`) is returned for the
/// HostTcp/ProcessBackend path so the in-guest helper can reach the
/// coord on loopback; `None` on the vsock (Firecracker) path.
pub(crate) async fn get_or_mint_broker_token(
    state: &SharedState,
    session_id: SessionId,
) -> Option<String> {
    // Fast path: this pod already unsealed it.
    if let Some(existing) = state.git_broker_tokens.get(&session_id) {
        return Some(existing.value().clone());
    }
    // Read-through: another replica (or a prior life of this pod)
    // minted it — the PG row is the authority.
    match load_broker_token(state, session_id).await {
        Ok(Some(token)) => {
            state.git_broker_tokens.insert(session_id, token.clone());
            return Some(token);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "broker token read-through failed");
            return None;
        }
    }
    // Mint, seal, insert first-writer-wins; on a lost race read the
    // winner's token so every replica injects the SAME value.
    let minted = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = match cipher.seal(minted.as_bytes()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "broker token seal failed");
            return None;
        }
    };
    let row = engram_core::types::registry::SessionBrokerToken {
        session_id,
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
    };
    match state.services.meta.insert_broker_token(row).await {
        Ok(true) => {
            state.git_broker_tokens.insert(session_id, minted.clone());
            Some(minted)
        }
        Ok(false) => match load_broker_token(state, session_id).await {
            Ok(Some(token)) => {
                state.git_broker_tokens.insert(session_id, token.clone());
                Some(token)
            }
            Ok(None) | Err(_) => {
                tracing::warn!(%session_id, "lost the mint race but the winner's row is unreadable");
                None
            }
        },
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "insert_broker_token failed");
            None
        }
    }
}

/// Load + unseal the session's broker token from PG, if a row exists.
pub(crate) async fn load_broker_token(
    state: &SharedState,
    session_id: SessionId,
) -> Result<Option<String>, String> {
    let Some(row) = state
        .services
        .meta
        .get_broker_token(session_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let nonce: [u8; 12] = row
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| "broker token nonce is not 12 bytes".to_string())?;
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: row.wrapped_dek,
        nonce,
        ciphertext: row.ciphertext,
        key_id: row.key_id,
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plain = cipher.open(&sealed).await.map_err(|e| e.to_string())?;
    String::from_utf8(plain)
        .map(Some)
        .map_err(|_| "broker token is not utf-8".to_string())
}

fn loopback_endpoint(state: &SharedState) -> Option<String> {
    if matches!(
        state.services.host.harness_dial(),
        engram_core::traits::HarnessDial::HostTcp
    ) {
        state
            .cfg
            .bind_addr
            .rsplit_once(':')
            .map(|(_host, port)| format!("http://127.0.0.1:{port}"))
    } else {
        None
    }
}

/// Every env a freshly-spawned harness needs beyond `session_env`:
/// the forge broker token (git-gated) and the artifact-upload token
/// (universal — the `engram-share` skill is baked into every image).
///
/// This is THE injector for both harness-spawn paths — session
/// create and resume's harness rebuild. They diverged once (resume
/// missed the upload env, so `engram-share` broke after every
/// idle→resume hop until the next cold create — prod session
/// 5cfb90b8); a single shared entry point makes that class of skew
/// impossible. If you add an env here, both paths get it.
pub(crate) async fn inject_harness_env(
    state: &SharedState,
    session_id: SessionId,
    git: Option<&engram_core::types::image::GitConfig>,
    env: &mut HashMap<String, String>,
) {
    inject_forge_env(state, session_id, git, env).await;
    // ADR 0026: artifact-upload token, injected for every image
    // (not git-gated) so the baked `engram-share` skill always works.
    inject_upload_env(state, session_id, env).await;
}

pub(crate) async fn inject_forge_env(
    state: &SharedState,
    session_id: SessionId,
    git: Option<&engram_core::types::image::GitConfig>,
    env: &mut HashMap<String, String>,
) {
    let (Some(_forge), Some(git)) = (state.forge.as_ref(), git) else {
        return;
    };
    let Some(token) = get_or_mint_broker_token(state, session_id).await else {
        return;
    };
    env.insert("ENGRAM_FORGE_TOKEN".into(), token);
    if let Some(owner) = git.owner.as_deref() {
        env.insert("ENGRAM_FORGE_OWNER".into(), owner.to_string());
    }
    // Where the in-guest helper reaches the forge endpoints. HostTcp
    // (ProcessBackend / `--mode=all`): the guest shares host networking,
    // so it hits the coord on loopback. Vsock (Firecracker): the helper
    // uses the agentd forge-request bridge instead (ADR 0023 §5), so no
    // HTTP URL is injected here.
    if let Some(ep) = loopback_endpoint(state) {
        env.insert("ENGRAM_FORGE_ENDPOINT".into(), ep);
    }
}

/// ADR 0026: inject the artifact-upload env so the in-guest
/// `engram-share` helper can stream files out. Unlike
/// [`inject_forge_env`] this is **NOT git-gated** — every image gets it
/// (the skill is baked unconditionally). Reuses the same per-session
/// broker token (a distinct env name, same secret value); the upload
/// auth check (`session_auth::authorize_broker_token`) doesn't require a
/// forge to be configured.
pub(crate) async fn inject_upload_env(
    state: &SharedState,
    session_id: SessionId,
    env: &mut HashMap<String, String>,
) {
    let Some(token) = get_or_mint_broker_token(state, session_id).await else {
        return;
    };
    env.insert("ENGRAM_UPLOAD_TOKEN".into(), token);
    if let Some(ep) = loopback_endpoint(state) {
        env.insert("ENGRAM_UPLOAD_ENDPOINT".into(), ep);
    }
}

/// Resolve the image's baked harness (ADR 0021 P1.3) into the
/// [`AgentSpec`] the backend will spawn at `start_agent` time, or
/// `None` for the readiness-probe path. Returns `None` when:
///
/// - `session_mode` is [`SessionMode::DevVm`] — the user wants a pure
///   dev VM and any resident agent stays undriven.
/// - The image has no `[harness]` block.
/// - The image's `[harness]` block is somehow not launchable
///   (defence-in-depth; the validator catches this at bake time).
///
/// The argv builds off the manifest's resolved `exec` path (already
/// rooted at `/opt/engram/harness/...` for built-ins, or whatever the
/// custom-harness author's Dockerfile COPY'd), with backend-specific
/// dial flags appended:
/// - `HostTcp` (Process): `--connect <host:port>` against the
///   coord-owned harness listener.
/// - `Vsock` (FC/VZ): `--vsock-host <port>` for AF_VSOCK loopback into
///   the host. The manifest's optional `args` ride after the standard
///   flags so authors can pass adapter-specific switches.
pub(crate) fn resolve_harness(
    state: &SharedState,
    image_harness: Option<&engram_core::types::image::HarnessManifest>,
    session_mode: SessionMode,
    session_id: SessionId,
    initial_prompt: Option<&str>,
    session_env: HashMap<String, String>,
    workdir: Option<String>,
) -> Result<Option<engram_core::types::sandbox::AgentSpec>, ApiError> {
    if session_mode.is_dev_vm() {
        return Ok(None);
    }
    let Some(harness) = image_harness else {
        return Ok(None);
    };
    // Defence-in-depth — engram.toml validation should have already
    // rejected this at bake time.
    let (Some(_name), Some(exec)) = (harness.name.as_deref(), harness.exec.as_deref()) else {
        return Ok(None);
    };

    // Harness-only extras, layered on top of `session_env` for the harness
    // child. `session_env` already carries ENGRAM_SESSION_ID + the image
    // env + secrets, so they aren't repeated here; the forge broker token
    // is added by the caller (per-spawn, kept out of the cached env).
    let mut env: HashMap<String, String> = HashMap::new();
    if let Some(prompt) = initial_prompt {
        env.insert("ENGRAM_INITIAL_PROMPT".into(), prompt.to_string());
    }
    // The manifest `workdir` reaches agentd as a reserved env entry so
    // the harness child starts there instead of `/`. See
    // `HARNESS_CWD_ENV` for why this isn't a wire-struct field.
    if let Some(cwd) = workdir {
        env.insert(engram_harness_proto::HARNESS_CWD_ENV.to_string(), cwd);
    }

    let mut argv = match state.services.host.harness_dial() {
        engram_core::traits::HarnessDial::HostTcp => {
            let addr = match *state.harness_listen_addr.lock() {
                Some(addr) => addr,
                None => {
                    return Err(ApiError::Internal(
                        "harness listener not bound (HostTcp backend)".into(),
                    ));
                }
            };
            env.insert("ENGRAM_HARNESS_ADDR".into(), addr.to_string());
            vec![
                exec.to_string(),
                "--connect".into(),
                addr.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
        engram_core::traits::HarnessDial::Vsock => {
            let port = engram_harness_proto::HARNESS_VSOCK_PORT;
            vec![
                exec.to_string(),
                "--vsock-host".into(),
                port.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
    };
    // Author-supplied extra args (manifest `[harness] args = [...]`)
    // ride after the standard dial flags so they can specialise the
    // adapter without overriding the SDK contract.
    argv.extend(harness.args.iter().cloned());

    Ok(Some(engram_core::types::sandbox::AgentSpec {
        argv,
        env,
        session_env,
        host_ca_pem: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::traits::ResolvedSecret;
    use engram_core::types::image::{BrowserConfig, NetworkPolicy, SecretSchema};
    use engram_core::types::ImageManifest;
    use engram_core::SandboxId;

    /// ADR 0027: the browser memory floor is applied identically by base-
    /// snapshot capture and session restore (both call `resolved_memory_mib`),
    /// so FC's "restore mem_size must equal snapshot mem_size" holds. It only
    /// raises memory for browser images that asked for less than the floor.
    #[test]
    fn resolved_memory_mib_floors_only_browser_images_below_floor() {
        let mk = |browser: bool, mem: Option<u32>| {
            let mut m = ImageManifest {
                name: "x".into(),
                ..Default::default()
            };
            m.resources.suggested_memory_mib = mem;
            if browser {
                m.browser = Some(BrowserConfig { enabled: true });
            }
            m
        };
        // Non-browser: suggestion (or default) honored verbatim.
        assert_eq!(resolved_memory_mib(&mk(false, None)), DEFAULT_MEMORY_MIB);
        assert_eq!(resolved_memory_mib(&mk(false, Some(256))), 256);
        // Browser + below floor: floored up.
        assert_eq!(
            resolved_memory_mib(&mk(true, Some(256))),
            BROWSER_MEMORY_FLOOR_MIB
        );
        // Browser + already above floor: honored.
        assert_eq!(resolved_memory_mib(&mk(true, Some(8192))), 8192);
        // Browser + default (4 GiB): already above the floor.
        assert_eq!(resolved_memory_mib(&mk(true, None)), DEFAULT_MEMORY_MIB);
    }

    /// ADR 0039: the base-restore metadata points the host's memory-chunk
    /// prefetch at the canonical working-set trace, keyed off the base
    /// snapshot's *memory* manifest. Disk-only / cold-boot base snapshots
    /// (no memory manifest) get `None` and fall back to full-manifest
    /// prefetch.
    #[test]
    fn base_working_set_blob_key_points_at_canonical_trace() {
        use engram_core::types::manifest::ManifestRef;
        // FC base snapshot with a memory manifest → canonical trace key
        // keyed on the *manifest_id* (not the version — the trace is per
        // manifest lineage, stable across diff-chain version bumps).
        let mref = ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: 3,
        };
        let key = base_working_set_blob_key(Some(mref)).expect("memory manifest → Some key");
        assert_eq!(
            key,
            format!("traces/{}/canonical.json", mref.manifest_id),
            "key must be the canonical trace for the memory manifest lineage",
        );
        // Disk-only / cold-boot (VZ) base snapshot: no memory image → None.
        assert_eq!(base_working_set_blob_key(None), None);
    }

    /// ADR 0016 §A.1.7 regression guard. The pure assembly path
    /// must:
    ///
    /// 1. Stamp the *real* `guest_ip` onto the policy (was
    ///    `Ipv4Addr::UNSPECIFIED` pre-fix — root cause of the
    ///    `UnknownGuest` DNS denials on session 1edf09a3).
    /// 2. Carry the manifest's `network.allow_*` lists verbatim.
    /// 3. Emit one `EgressSecretEntry` per resolved secret that
    ///    has a corresponding placeholder in the env map, pairing
    ///    the env's placeholder with the bundle's real value and
    ///    the schema's per-secret allow lists.
    /// 4. Skip resolved secrets that lack a matching env entry —
    ///    that means `apply_secrets_to_env` filtered them (e.g.
    ///    the manifest declared a secret but the placeholder
    ///    machinery dropped it). Better to under-populate than
    ///    crash.
    #[test]
    fn assemble_resume_egress_policy_uses_real_guest_ip_and_allow_lists() {
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.7".parse().unwrap();

        // Manifest: one network allow-host, broker mode, one
        // declared secret with an allow-list of its own.
        let mut secrets_schema = HashMap::new();
        secrets_schema.insert(
            "ANTHROPIC_API_KEY".to_string(),
            SecretSchema {
                allow_hosts: vec!["api.anthropic.com".into()],
                allow_host_patterns: vec!["*.anthropic.com".into()],
                required: true,
                description: None,
                r#ref: None,
            },
        );
        let manifest = ImageManifest {
            name: "test-image".into(),
            description: None,
            env: HashMap::new(),
            workdir: None,
            secrets: secrets_schema,
            network: NetworkPolicy {
                default: engram_core::types::image::NetworkDefault::Deny,
                allow_hosts: vec!["registry.npmjs.org".into()],
                allow_host_patterns: vec!["*.openai.com".into()],
            },
            resources: Default::default(),
            secret_mode: SecretMode::Broker,
            harness: None,
            git: None,
            browser: None,
        };

        // Bundle: one resolved secret; the schema's allow_hosts must
        // round-trip into the EgressSecretEntry.
        let mut bundle_inner = HashMap::new();
        bundle_inner.insert(
            "ANTHROPIC_API_KEY".to_string(),
            ResolvedSecret {
                value: "sk-real-secret-do-not-leak".into(),
                schema: SecretSchema {
                    allow_hosts: vec!["api.anthropic.com".into()],
                    allow_host_patterns: vec!["*.anthropic.com".into()],
                    required: true,
                    description: None,
                    r#ref: None,
                },
            },
        );
        let bundle = SecretBundle {
            secrets: bundle_inner,
        };

        // env-with-placeholders: simulates Broker-mode
        // `apply_secrets_to_env` having inserted a placeholder.
        let mut env_with_ph = HashMap::new();
        env_with_ph.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "engram_ph_test_abcd1234".to_string(),
        );

        let policy = assemble_resume_egress_policy(
            session_id,
            sandbox_id,
            guest_ip,
            &bundle,
            &manifest,
            &env_with_ph,
        );

        // 1. Real IP, not UNSPECIFIED.
        assert_eq!(policy.guest_ip, guest_ip);
        assert_ne!(policy.guest_ip, std::net::Ipv4Addr::UNSPECIFIED);
        assert_eq!(policy.session_id, session_id);
        assert_eq!(policy.sandbox_id, sandbox_id);

        // 2. Network allow lists from manifest.
        assert_eq!(policy.network_allow_hosts, vec!["registry.npmjs.org"]);
        assert_eq!(policy.network_allow_host_patterns, vec!["*.openai.com"]);
        assert_eq!(policy.secret_mode, SecretMode::Broker);

        // 3. One secret entry, placeholder from env, real value from
        // bundle, allow-lists from the schema.
        assert_eq!(policy.secrets.len(), 1);
        let entry = &policy.secrets[0];
        assert_eq!(entry.placeholder, "engram_ph_test_abcd1234");
        assert_eq!(entry.real_value, "sk-real-secret-do-not-leak");
        assert_eq!(entry.allow_hosts, vec!["api.anthropic.com"]);
        assert_eq!(entry.allow_host_patterns, vec!["*.anthropic.com"]);
    }

    /// Resolved secrets without a placeholder in the env map are
    /// silently dropped from the egress policy — they can't be
    /// substituted on outbound traffic anyway because the in-VM
    /// env doesn't carry their placeholder. Under-populating the
    /// secrets list is strictly safer than panicking.
    #[test]
    fn assemble_resume_egress_policy_skips_secrets_missing_from_env() {
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.8".parse().unwrap();

        let manifest = ImageManifest {
            name: "test-image".into(),
            description: None,
            env: HashMap::new(),
            workdir: None,
            secrets: HashMap::new(),
            network: NetworkPolicy::default(),
            resources: Default::default(),
            secret_mode: SecretMode::Broker,
            harness: None,
            git: None,
            browser: None,
        };

        let mut bundle_inner = HashMap::new();
        bundle_inner.insert(
            "MISSING_FROM_ENV".to_string(),
            ResolvedSecret {
                value: "v".into(),
                schema: SecretSchema {
                    allow_hosts: vec!["x.example".into()],
                    allow_host_patterns: vec![],
                    required: true,
                    description: None,
                    r#ref: None,
                },
            },
        );
        let bundle = SecretBundle {
            secrets: bundle_inner,
        };
        let env_with_ph: HashMap<String, String> = HashMap::new();

        let policy = assemble_resume_egress_policy(
            session_id,
            sandbox_id,
            guest_ip,
            &bundle,
            &manifest,
            &env_with_ph,
        );

        assert_eq!(policy.guest_ip, guest_ip);
        assert!(
            policy.secrets.is_empty(),
            "secret with no placeholder in env must be skipped, got {:?}",
            policy.secrets,
        );
    }
}
