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
use crate::host_registry::ScheduleContext;
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

    let vcpus = manifest.resources.suggested_vcpus.unwrap_or(DEFAULT_VCPUS);
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
/// In `Broker` mode, *placeholder* env vars land — the per-session egress
/// proxy substitutes the real value only on outbound HTTPS requests to the
/// secret's `allow_hosts`. The substitution IS wired: the create/resume
/// handlers build a [`SessionEgressPolicy`] (placeholder→real per secret)
/// and ship it to the host-agent's proxy registry via `start_agent` /
/// `apply_egress_policy` (ADR 0006). This function only sets the in-guest
/// placeholders; the policy build folds the same secrets into the
/// registered keyring.
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
            // The real value reaches the proxy via the SessionEgressPolicy
            // the create/resume handlers register (placeholder→real per
            // secret, keyed on guest IP); the proxy substitutes on outbound
            // requests whose host matches `schema.allow_hosts` /
            // `allow_host_patterns`. See `build_resume_egress_policy` and the
            // create-path policy build.
            tracing::debug!(
                %session,
                secret_count = bundle.secrets.len(),
                "broker-mode placeholders injected; proxy substitutes per the session egress policy",
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
) -> (
    Option<ResumeManifestBundle>,
    HashMap<String, String>,
    Option<engram_core::types::egress::EgressSecretEntry>,
) {
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
    //
    // ADR 0037: the harness credential is brokered — `inject_harness_broker_secret`
    // puts a (deterministic, so it matches the create-time one) placeholder in
    // `env` and hands back the `EgressSecretEntry`; we bubble it up so the
    // caller folds it into the rebuilt resume egress policy.
    let mut harness_broker_secret = None;
    if let (Some(rt), Some(uid_str)) = (state.auth.as_ref(), session.user_id.as_ref()) {
        if let Ok(uuid) = uid_str.parse::<uuid::Uuid>() {
            match rt.users.get_user(engram_core::UserId(uuid)).await {
                Ok(user) => {
                    let principal = user.to_principal();
                    env.insert("ENGRAM_USER_EMAIL".into(), principal.email.clone());
                    env.insert("ENGRAM_USER_NAME".into(), principal.git_name());
                    let harness = bundle.as_ref().and_then(|b| b.manifest.harness.as_ref());
                    harness_broker_secret = inject_harness_broker_secret(
                        state,
                        &principal,
                        harness,
                        session.mode,
                        &mut env,
                        session.id,
                    )
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

    (bundle, env, harness_broker_secret)
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
    harness_broker_secret: Option<&engram_core::types::egress::EgressSecretEntry>,
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
        harness_broker_secret,
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
    harness_broker_secret: Option<&engram_core::types::egress::EgressSecretEntry>,
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
    // ADR 0037: the brokered harness credential isn't a manifest secret, so
    // fold it in explicitly (its placeholder already rode into env via
    // resolve_session_env → inject_harness_broker_secret).
    if let Some(s) = harness_broker_secret {
        secrets.push(s.clone());
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
    // -------- 0. Validate orthogonal axes --------
    // ADR 0021 P1.3: `mode = DevVm + non-empty prompt` is meaningless
    // — even if the image carries a baked harness, a DevVm session
    // leaves it undriven, so there's nothing to receive the prompt.
    // Reject explicitly so the dashboard / CLI surfaces the mistake.
    if req.mode.is_dev_vm() && !req.prompt.as_deref().map(str::is_empty).unwrap_or(true) {
        return Err(ApiError::BadRequest(
            "`prompt` requires `mode = agent` — a dev-VM session has no agent to receive it".into(),
        ));
    }

    let image_uri: ImageRef = req.image.clone();
    let (image_repo, image_tag) = {
        let (r, t) = split_image_ref(&image_uri);
        (r.to_string(), t.to_string())
    };

    // -------- 1. Look up the manifest from `enabled_images` --------
    // The session's image must be enabled before sessions can
    // reference it. The `enabled_images` row carries the
    // manifest.toml fetched at enable time; session-create has zero
    // network dependency on the manifest path.
    let enabled = state
        .services
        .meta
        .get_enabled_image(&image_uri)
        .await
        .map_err(|e| ApiError::Internal(format!("enabled_images lookup: {e}")))?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "image `{image_uri}` is not enabled. \
                 Operators enable images via POST /api/enabled-images \
                 (or the dashboard's Settings → Images panel) before \
                 sessions can reference them."
            ))
        })?;
    let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
        ApiError::Internal(format!(
            "stored manifest for {image_uri} failed to parse: {e}"
        ))
    })?;

    // ADR 0021 P1.5b retired the `harness_pack_uri` plumbing
    // entirely — the harness travels in the rootfs now, so there's
    // no registry URI to thread.
    //
    // Pre-flight gate: a session that asked for `mode = Agent` against
    // a harness-less image is fine (it boots as if dev-VM), but we
    // surface a 400 only when an image's `[harness]` block is
    // malformed (`is_launchable() == false`). Manifests round-trip
    // through `HarnessManifest::validate_source` at bake time, so
    // this is defence-in-depth against drift.
    if let Some(h) = manifest.harness.as_ref() {
        if !h.is_launchable() && !req.mode.is_dev_vm() {
            return Err(ApiError::Internal(format!(
                "enabled image `{image_uri}` has a [harness] block that didn't resolve \
                 (name/exec unset) — re-bake the image"
            )));
        }
    }

    // -------- 2. Resolve secrets --------
    let secret_ctx = SecretContext {
        repo: &image_repo,
        image_tag: &image_tag,
    };
    if req.secrets.as_ref().map(|m| !m.is_empty()).unwrap_or(false)
        && manifest.secret_mode != engram_core::types::image::SecretMode::Literal
    {
        return Err(ApiError::BadRequest(
            "per-request `secrets` are only supported for `secret_mode = literal` images".into(),
        ));
    }
    let secret_bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest.secrets, req.secrets.as_ref())
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;

    // ADR 0005: workspace comes from the bake image; the platform
    // never clones a repo or materializes anything itself.
    let spec = SessionSpec {
        image: req.image.clone(),
        mode: req.mode,
        // ADR 0031: owner is the authenticated principal, server-stamped.
        user_id: Some(principal.user_id.to_string()),
    };

    // -------- 3. Mint a SessionId (no DB write yet) --------
    //
    // The row is persisted in Phase 5, AFTER scheduling succeeds —
    // so a scheduling failure (no capacity, image-not-found, host
    // crash mid-create) returns 503 with no zombie row. Phase 4
    // builds vm_spec / harness args using this id, which has to
    // exist before the sandbox does because those args ride INTO
    // the sandbox's env at create time.
    //
    // TODO(self-healing reconciler): when transient-capacity blips
    // become a real pattern (warm pools, multi-region cold-start),
    // flip this around: persist a Pending row with the full
    // vm_spec captured, and have a background reconciler retry
    // scheduling against later-arriving capacity. Today the
    // session row carries `image + harness + user_id` only —
    // not enough to reconstruct a vm_spec — so the v1 fix is
    // "fail loud, let the user retry" rather than "queue and
    // retry in the background". Schema work to capture the
    // full spec is the gating change.
    let session_id = SessionId::new();

    // -------- 4. Build the anonymous SandboxSpec --------
    // What every sandbox in this image pool gets. Session-specific
    // env (ENGRAM_SESSION_ID etc.) is injected at exec / start_agent
    // time so pooled sandboxes can serve any future session in the
    // bucket.
    let mut spec_env: HashMap<String, String> = manifest.env.clone();
    apply_secrets_to_env(
        &mut spec_env,
        &secret_bundle,
        manifest.secret_mode,
        session_id,
    );

    // Per-request secrets that don't appear in the image manifest's
    // schema (e.g. harness-supplied creds like CLAUDE_CODE_OAUTH_TOKEN
    // — the demo image declares no `[secrets.*]` blocks) wouldn't
    // otherwise reach the harness env: `SecretStore::resolve` only
    // iterates the schema, so override values for unknown names get
    // silently dropped. Fold them in here, overwriting any same-key
    // value injected by the manifest's defaults / the secret store —
    // the user explicitly typed a value into the dashboard for *this*
    // session, that intent dominates. The broker-mode gate upstream
    // (line ~204) guarantees overrides only show up under
    // `SecretMode::Literal`, so passing them through verbatim is
    // safe — the operator asserted the value, no proxy substitution
    // contract is implied.
    // Captured-but-deferred: the env injection is pure CPU and has to
    // happen before vm_spec construction, but the DB persist depends
    // on the session row existing (FK from session_secrets.session_id
    // → sessions.id). Under the post-aa794b8 flow the row isn't
    // written until *after* scheduling succeeds, so we defer the
    // persist call to that point. See `deferred_session_secrets`
    // below for where it actually fires.
    let deferred_session_secrets: Option<HashMap<String, String>> =
        if manifest.secret_mode == engram_core::types::image::SecretMode::Literal {
            if let Some(overrides) = req.secrets.as_ref() {
                for (name, value) in overrides {
                    spec_env.insert(name.clone(), value.clone());
                }
                // Persist sealed under the deployment KEK so resume can
                // rebuild the post-resume harness's launch env. Without
                // this, an idle-then-active transition (auto-resume on
                // the next prompt) respawns the harness child with no
                // secret env — Claude prompts the user to log in again.
                // Skipped when overrides is empty so we don't create
                // empty rows.
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

    // ADR 0021 P1.3: the harness identity (when any) comes from the
    // image manifest, not the session request. Carry the resolved
    // name through to the host-agent so the pooled_backend still
    // pins its mount-root subdir name correctly (legacy substrate
    // path; goes away with P1.5).
    if let Some(h) = manifest.harness.as_ref() {
        if let Some(name) = h.name.as_deref() {
            spec_env.insert("ENGRAM_SESSION_HARNESS_NAME".into(), name.to_string());
        }
    }

    // The durable session environment agentd holds at bind and applies as
    // the base env for every process it spawns — harness, `/exec`, and the
    // interactive shell. It's the image `[env]` + resolved secrets (already
    // in spec_env) plus the session id. The forge broker token is
    // deliberately NOT folded in: it's a short-lived per-request credential
    // (re-minted so it survives a coord restart), so it rides each spawn's
    // own env instead of the cached session env. See `AgentSpec::session_env`.
    let mut session_env = spec_env.clone();
    session_env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());

    // ADR 0031: attribute git commits inside the session to the initiating
    // user. `render_gitconfig` writes these into `/etc/gitconfig [user]`.
    session_env.insert("ENGRAM_USER_EMAIL".into(), principal.email.clone());
    session_env.insert("ENGRAM_USER_NAME".into(), principal.git_name());

    // ADR 0031: for built-in Claude sessions, auto-inject the user's saved
    // Claude Code OAuth token — we never prompt per-session. Best-effort: a
    // missing/unopenable token must not fail create (the harness then falls
    // back to its own login path; the web gates create on a saved token).
    // ADR 0037: the harness credential (if any) is brokered — `session_env`
    // gets a placeholder; the real value rides the egress policy (below) so
    // the proxy substitutes it on the wire.
    let harness_broker_secret = inject_harness_broker_secret(
        &state,
        &principal,
        manifest.harness.as_ref(),
        req.mode,
        &mut session_env,
        session_id,
    )
    .await;

    let mut agent_for_session = resolve_harness(
        &state,
        manifest.harness.as_ref(),
        req.mode,
        session_id,
        req.prompt.as_deref(),
        session_env.clone(),
        manifest.workdir.clone(),
    )?;
    // ADR 0023: mint the per-session forge broker token and hand it to the
    // harness as a per-spawn extra (`AgentSpec::env`, layered on top of
    // session_env). dev_vm sessions have no harness; their `/exec` path
    // mints the token per request instead.
    if let Some(agent) = agent_for_session.as_mut() {
        inject_harness_env(&state, session_id, manifest.git.as_ref(), &mut agent.env);
    }

    // Network policy: image manifest's `[network]` block, verbatim.
    // ADR 0005 retired the platform's git workspace (no clone URL to
    // auto-allowlist anymore); agents that need GitHub egress
    // declare `[network] allow_hosts = ["api.github.com"]` in their
    // image manifest like any other dependency.
    let network = manifest.network.clone();

    // Clone the env + network policy before they're moved into the
    // VmSpec — the post-create proxy registration needs to look up
    // placeholders by name and build the network allow-list. The
    // borrow checker would otherwise rightly complain.
    let spec_env_for_proxy = spec_env.clone();
    let network_for_proxy = network.clone();

    // -------- 5. Schedule: restore from the image's base snapshot --------
    //
    // ADR 0020: every enabled image has a base snapshot — enable is
    // transactional (an image can't be enabled without capturing one).
    // So session create ALWAYS restores; there is no cold-boot path here.
    // The single cold boot in the whole system is the capture itself
    // (`build_base_snapshot`, run once per image at enable). No row
    // exists yet; a restore failure returns 503 with Postgres untouched,
    // so a retry can succeed once capacity recovers.
    // The NOT NULL FK (migration 0038) guarantees a persisted enabled
    // image carries a base snapshot; `Option` here is only the
    // build-then-stamp artifact. `None` would mean a pre-0020 row the
    // migration should have cleared — surface loudly, don't cold-boot.
    let base_snapshot_id = enabled.base_snapshot_id.ok_or_else(|| {
        ApiError::Internal(format!(
            "enabled image `{}` has no base snapshot — re-enable it \
             (POST /api/enabled-images) to capture one",
            req.image
        ))
    })?;
    // ADR 0027: must match the floor `capture_and_record_base_snapshot`
    // applied — FC requires the restore `mem_size_mib` to equal the
    // snapshot's. The shared helper guarantees they agree.
    let memory_mib = resolved_memory_mib(&manifest);

    let (host_id, sandbox_id) = try_restore_base_snapshot(
        &state,
        base_snapshot_id,
        &image_repo,
        &image_tag,
        memory_mib,
        // Per-session sandbox env (manifest env + resolved secrets +
        // ENGRAM_SESSION_*). The shared base snapshot can't carry it, so
        // it's injected into the restored sandbox (cold-create baked it
        // into vm_spec.env).
        spec_env,
    )
    .await
    .map_err(|e| {
        tracing::warn!(
            image_uri = %req.image,
            snapshot_id = %base_snapshot_id,
            error = %e,
            "base-snapshot restore failed at scheduling; returning 503, no row persisted",
        );
        match e {
            engram_core::SandboxError::ImageNotReady(d) => ApiError::Unavailable(format!(
                "image `{}` (digest {d}) is not ready on any host yet; retry shortly",
                req.image
            )),
            other => ApiError::Unavailable(format!(
                "no host could restore this session's base snapshot right now: {other}. \
                 Retry shortly; capacity recovers as hosts register or sessions drain."
            )),
        }
    })?;

    // Atomic insert: row exists only once we have host_id + sandbox_id
    // bound. If THIS step fails, the sandbox is already running and
    // would orphan — tear it down before bubbling. Note the
    // `services.host.destroy` is best-effort; a failure here leaves
    // a sandbox running on the host until its owning host-agent's
    // reconcile pass flips it.
    if let Err(e) = state
        .services
        .meta
        .create_session_created(session_id, spec, host_id, sandbox_id)
        .await
    {
        tracing::error!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            host_id = %host_id,
            error = %e,
            "session row insert failed after sandbox create; tearing sandbox down",
        );
        if let Err(de) = state.services.host.destroy(sandbox_id).await {
            tracing::error!(
                session_id = %session_id,
                sandbox_id = %sandbox_id,
                error = %de,
                "sandbox teardown after insert failure also failed — host reconcile will GC",
            );
        }
        return Err(e.into());
    }

    // Now that the session row exists, the FK on session_secrets is
    // satisfiable. Best-effort: a failure here just means resume
    // re-prompts for credentials (the harness still gets them on the
    // initial boot via vm_spec.env).
    if let Some(overrides) = deferred_session_secrets {
        if let Err(e) = persist_session_secrets(&state, session_id, &overrides).await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "session secrets persistence failed; resume will lose secrets",
            );
        }
    }

    state.registry.bind(session_id, sandbox_id);
    // ADR 0006: ship per-session egress policy to the host-agent
    // that owns this sandbox. The host-agent applies it to its
    // local proxy registry; the WS frame and any subsequent
    // start_agent request are serialised on the same connection so
    // the policy is live before the harness can make egress calls.
    //
    // Build the egress policy from the resolved guest IP. None when
    // the backend has no guest IP for this sandbox (process backend,
    // VZ in some configs) — fine; host-agents without a proxy
    // ignore the policy field.
    let egress_policy = if let Some(guest_ip_str) = state.services.host.guest_ip(sandbox_id).await {
        if let Ok(guest_ip) = guest_ip_str.parse::<std::net::Ipv4Addr>() {
            let mut secrets = Vec::new();
            for (name, resolved) in &secret_bundle.secrets {
                let Some(placeholder) = spec_env_for_proxy.get(name).cloned() else {
                    continue;
                };
                secrets.push(engram_core::types::egress::EgressSecretEntry {
                    placeholder,
                    real_value: resolved.value.clone(),
                    allow_hosts: resolved.schema.allow_hosts.clone(),
                    allow_host_patterns: resolved.schema.allow_host_patterns.clone(),
                });
            }
            // ADR 0037: fold in the brokered harness credential (its
            // placeholder is what `inject_harness_broker_secret` put in
            // session_env) so the proxy substitutes the real value.
            if let Some(s) = &harness_broker_secret {
                secrets.push(s.clone());
            }
            Some(engram_core::types::egress::SessionEgressPolicy {
                session_id,
                sandbox_id,
                guest_ip,
                network_allow_hosts: network_for_proxy.allow_hosts.clone(),
                network_allow_host_patterns: network_for_proxy.allow_host_patterns.clone(),
                secrets,
                secret_mode: manifest.secret_mode,
            })
        } else {
            None
        }
    } else {
        None
    };
    // Bind the routing on the host that owns this sandbox *before*
    // the agent has a chance to dial. `backend.create()` returns with
    // the sandbox ready to accept exec but the agent (if any) NOT
    // yet spawned. HostClient dispatches by sandbox_id so this routes
    // to the right host (local in mode=all, WS in mode=coordinator).
    state
        .services
        .host
        .bind_session(session_id, sandbox_id)
        .await;

    // -------- 6. Start the agent --------
    //
    // ADR 0015 M1: every cold-create session goes through
    // `start_agent`, including `harness: none`. Agentd's
    // SpawnHarness handler treats empty argv as a readiness
    // probe (no child spawned, no error), so the same call
    // serves both cases — and as a side-effect "session is
    // Active" now actually implies "agentd is reachable on
    // vsock", which makes the early-eof race that the no-
    // harness path used to produce structurally impossible.
    let agent = agent_for_session.unwrap_or_else(|| engram_core::types::sandbox::AgentSpec {
        argv: Vec::new(),
        env: std::collections::HashMap::new(),
        // dev_vm readiness probe: no harness ever spawns, but agentd still
        // records this so the session's `/exec` and shell inherit it.
        session_env,
        // Host-agent fills `host_ca_pem` in from its local egress
        // state (ADR 0021 P1.2). Coord leaves it None.
        host_ca_pem: None,
    });
    let policy = egress_policy.unwrap_or_else(|| {
        // No guest IP yet → synthesize an unspecified-IP policy.
        // host-agents without a live proxy treat policy application
        // as a no-op; the agent can refine on a later
        // `apply_egress_policy` once the IP shows up.
        engram_core::types::egress::SessionEgressPolicy {
            session_id,
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: network_for_proxy.allow_hosts.clone(),
            network_allow_host_patterns: network_for_proxy.allow_host_patterns.clone(),
            // Keep the brokered harness credential even in the no-guest-IP
            // fallback policy (refined later via apply_egress_policy).
            secrets: harness_broker_secret.clone().into_iter().collect(),
            secret_mode: manifest.secret_mode,
        }
    });
    // ADR 0015 M2: emit the Pending→Created transition first so SSE
    // subscribers see the lifecycle moment when the row materialized
    // (the actual INSERT happened a few lines up). Pending is the
    // API-caller's pre-insert view; we never persisted it, but the
    // event log is the durable record of "request accepted, scheduler
    // returned, row exists at Created."
    let now_created = chrono::Utc::now();
    state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Pending,
                to: SessionState::Created,
                at: now_created,
            },
        )
        .await?;
    if let Err(e) = state
        .services
        .host
        .start_agent(sandbox_id, agent, policy)
        .await
    {
        tracing::error!(
            %session_id,
            %sandbox_id,
            %host_id,
            error = %e,
            "start_agent failed; marking session Failed",
        );
        let _ = state
            .services
            .meta
            .transition_session(session_id, SessionState::Failed)
            .await;
        state.services.host.unbind_session(session_id).await;
        return Err(e.into());
    }
    // ADR 0015 M2: start_agent returned OK, so agentd is reachable
    // and the harness (if any) is running. Now and only now does
    // `Active` actually hold its meaning. Transition + emit.
    let prev_for_active = state
        .services
        .meta
        .transition_session(session_id, SessionState::Active)
        .await?;
    state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: prev_for_active,
                to: SessionState::Active,
                at: chrono::Utc::now(),
            },
        )
        .await?;

    // If the request carried an initial prompt, record it as a
    // user-role message in the event log. The harness pulls the value
    // out of `ENGRAM_INITIAL_PROMPT` and runs it without echoing it
    // back through Claude's stream-json output, so subscribers
    // (transcripts, dashboards) only see the assistant's reply
    // otherwise. Mirror the per-prompt path in `prompt.rs`. Best-
    // effort: a failed emit doesn't roll back session creation.
    if let Some(text) = req.prompt.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = state
            .emit(
                session_id,
                SessionEvent::HarnessAgentMessage {
                    run_id: String::new(),
                    message_id: format!("user-{}", uuid::Uuid::new_v4()),
                    role: engram_harness_proto::AgentRole::User,
                    text: text.to_string(),
                    at: chrono::Utc::now(),
                },
            )
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "emit initial prompt event failed",
            );
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id,
            status: SessionState::Active.as_str(),
            image_version: image_tag,
            // ADR 0020: every session is a base-snapshot restore now.
            kind: "restored",
        }),
    ))
}

/// ADR 0020 P1: attempt to restore a session from the image's base
/// snapshot, late-binding the session harness. Builds the
/// `SnapshotMetadata` from the base snapshot's `snapshots` row (the
/// portable blob keys are deterministic from `snapshot_id`), picks a
/// host, and runs the combined restore + harness-swap op. Any error
/// bubbles to the caller, which falls back to a cold create — so this
/// never fails a session, it only declines to fast-path it.
async fn try_restore_base_snapshot(
    state: &SharedState,
    snapshot_id: engram_core::types::SnapshotId,
    image_repo: &str,
    image_tag: &str,
    memory_mib: u32,
    session_env: HashMap<String, String>,
) -> Result<(engram_core::HostId, engram_core::SandboxId), engram_core::SandboxError> {
    let record = state
        .services
        .meta
        .get_snapshot(snapshot_id)
        .await
        .map_err(|e| {
            engram_core::SandboxError::Snapshot(format!(
                "get_snapshot {snapshot_id} for base restore: {e}"
            ))
        })?
        .ok_or_else(|| {
            engram_core::SandboxError::Snapshot(format!(
                "enabled image references base snapshot {snapshot_id} but its snapshots row is gone"
            ))
        })?;
    // The state.bin / sidecar.json blob keys are deterministic from the
    // snapshot id (`snapshots/<id>/...`); the disk + memory chunked
    // manifests come off the snapshots row. Together this is the full
    // portable metadata the host's cross-host restore path consumes.
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        warm_harness: false,
        id: snapshot_id,
        size_bytes: record.size_bytes,
        created_at: record.created_at,
        image_version: record.image_version,
        disk_manifest: record.disk_manifest,
        memory_manifest: record.memory_manifest,
        source_sandbox_id: None,
        state_blob_key: Some(engram_chunk_store::snapshot_blob::state_blob_key(
            snapshot_id,
        )),
        sidecar_blob_key: Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(
            snapshot_id,
        )),
        rootfs_blob_key: None,
        // P1 leaves working-set prefetch off; P2 (ADR 0020) publishes the
        // bake-time trace and points UFFD prefetch at it.
        working_set_blob_key: None,
        // ADR 0035: the generations this base snapshot pins; the host
        // materializes any it's missing and (fresh flavor) swaps to
        // its current generation post-load.
        aux_bundles: record.aux_bundles,
    };
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: Some(snapshot_id),
        memory_mib: Some(memory_mib),
        // Base-snapshot chunks are pulled from BlobStorage on demand by
        // the restore path; no host needs to have prefetched the image.
        required_image_digest: None,
        exclude_host: None,
    };
    state
        .host_registry
        .restore_base_for_session(&ctx, metadata, session_env)
        .await
}

/// ADR 0031: inject the initiating user's saved Claude Code OAuth token into
/// the session env, but only when the resolved harness is built-in Claude and
/// the session drives it (agent mode). Best-effort — a missing or unopenable
/// token is logged, never fatal, so the harness can still fall back to its own
/// login path. Shared by the create and resume paths.
/// ADR 0037: broker a secret — generic, harness-agnostic. Puts a
/// deterministic placeholder into the guest env under `name` and returns
/// the [`EgressSecretEntry`] the caller folds into the session's egress
/// policy, so the proxy substitutes `real_value` on the wire only for
/// `allow_hosts`. The real value never enters guest RAM (or a snapshot of
/// it). The placeholder is deterministic in `(session, name)` — same shape
/// as `apply_secrets_to_env`'s — so create and resume, which resolve it
/// independently, agree and the proxy's table stays valid across an
/// idle→resume.
fn broker_secret(
    session_env: &mut HashMap<String, String>,
    session_id: SessionId,
    name: &str,
    real_value: String,
    allow_hosts: Vec<String>,
) -> engram_core::types::egress::EgressSecretEntry {
    let placeholder = format!(
        "engram_ph_{}_{}",
        session_id.as_uuid().simple(),
        short_hash(name),
    );
    session_env.insert(name.to_string(), placeholder.clone());
    engram_core::types::egress::EgressSecretEntry {
        placeholder,
        real_value,
        allow_hosts,
        allow_host_patterns: Vec::new(),
    }
}

/// Resolve and broker the *harness's* credential into `session_env`,
/// returning the policy entry (or `None` if there's nothing to broker).
/// The brokering itself is generic ([`broker_secret`]); the only
/// per-harness knowledge is which credential the harness needs and where
/// it may be sent — currently the built-in Claude harness's per-user
/// OAuth token (ADR 0031), bound to `api.anthropic.com`. New built-in
/// harnesses add a match arm here; the cleaner end state is declaring
/// `(env var, allow_hosts, source)` in the harness manifest so this is
/// pure data (follow-up).
///
/// Best-effort: a missing/unopenable credential returns `None` (harness
/// falls back to its own login); never fails create.
async fn inject_harness_broker_secret(
    state: &SharedState,
    principal: &engram_core::types::user::Principal,
    harness: Option<&engram_core::types::image::HarnessManifest>,
    mode: SessionMode,
    session_env: &mut HashMap<String, String>,
    session_id: SessionId,
) -> Option<engram_core::types::egress::EgressSecretEntry> {
    if mode.is_dev_vm() {
        return None;
    }
    match harness.and_then(|h| h.name.as_deref()) {
        Some("claude") => {
            let rt = state.auth.as_ref()?;
            let kind = engram_core::types::user::UserToken::KIND_CLAUDE_OAUTH;
            let plain = match rt.users.get_user_token(principal.user_id, kind).await {
                Ok(Some(tok)) => {
                    match engram_auth::open_user_token(state.services.kek.as_ref(), &tok).await {
                        Ok(plain) => plain,
                        Err(e) => {
                            tracing::warn!(user_id = %principal.user_id, error = %e, "could not open saved Claude token");
                            return None;
                        }
                    }
                }
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(user_id = %principal.user_id, error = %e, "could not load saved Claude token");
                    return None;
                }
            };
            Some(broker_secret(
                session_env,
                session_id,
                "CLAUDE_CODE_OAUTH_TOKEN",
                plain,
                vec!["api.anthropic.com".to_string()],
            ))
        }
        _ => None,
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
    // Confirm the session exists *before* tearing anything down so
    // unknown ids return 404 (matching the get/exec contract) instead
    // of silently succeeding.
    let session = state.services.meta.get_session(id).await?;

    // Idempotent: a session already in a terminal state has nothing
    // to tear down. Skip straight to 204 instead of trying to drive
    // a `Completed`/`Failed`/`Dead` → `Completed` transition (which
    // the state machine would reject as an illegal terminal-state
    // move).
    if session.status.is_terminal() {
        return Ok(StatusCode::NO_CONTENT);
    }

    // ADR 0015 M2: transition to Completed *before* destroying the
    // sandbox. The reconcile pass that runs on every heartbeat looks
    // for `status='active'` sessions whose `sandbox_id` is missing
    // from the host's running set; moving to Completed first removes
    // this row from that view so reconcile can't race us into
    // HostLost while we're waiting for the host RPC. The DB UPDATE
    // is fast (single locked row); the destroy that follows is
    // best-effort and can take seconds.
    //
    // If a sibling path (preemption drain, dead-host detector) flipped
    // the row first, our transition fails with Conflict — treat that
    // as idempotent success, the session is already on its way out.
    //
    // ADR 0034: deleting mid-eviction works the same way —
    // Evicting → Completed is legal, and the eviction scanner's
    // racing pipeline then fails its own transition_session(Idle)
    // against the terminal row, fires abort_inflight_snapshot, and
    // releases the session lease. No special-casing needed here.
    match state
        .services
        .meta
        .transition_session(id, SessionState::Completed)
        .await
    {
        Ok(prev) => {
            state
                .emit(
                    id,
                    SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Completed,
                        at: chrono::Utc::now(),
                    },
                )
                .await?;
            // ADR 0023: drop the session's credential-broker token so a
            // terminated session can no longer mint git credentials.
            state.git_broker_tokens.remove(&id);
        }
        Err(engram_core::MetaError::Conflict(msg)) => {
            tracing::info!(
                session_id = %id,
                error = %msg,
                "delete_session: state machine raced us (likely reconciler flipped to terminal first); returning 204 idempotently"
            );
            // Still tear down whatever's left for tidiness, then 204.
            if let Some(sandbox_id) = state.registry.unbind(id) {
                let _ = state.services.host.destroy(sandbox_id).await;
            }
            state.services.host.unbind_session(id).await;
            return Ok(StatusCode::NO_CONTENT);
        }
        Err(e) => return Err(e.into()),
    }

    // Status is Completed; reconcile won't touch this row anymore.
    // Now tear down the sandbox and clear the routing columns.
    if let Some(sandbox_id) = state.registry.unbind(id) {
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
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
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
pub(crate) fn get_or_mint_broker_token(state: &SharedState, session_id: SessionId) -> String {
    match state.git_broker_tokens.get(&session_id) {
        Some(existing) => existing.value().clone(),
        None => {
            let minted = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            state.git_broker_tokens.insert(session_id, minted.clone());
            minted
        }
    }
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
pub(crate) fn inject_harness_env(
    state: &SharedState,
    session_id: SessionId,
    git: Option<&engram_core::types::image::GitConfig>,
    env: &mut HashMap<String, String>,
) {
    inject_forge_env(state, session_id, git, env);
    // ADR 0026: artifact-upload token, injected for every image
    // (not git-gated) so the baked `engram-share` skill always works.
    inject_upload_env(state, session_id, env);
}

pub(crate) fn inject_forge_env(
    state: &SharedState,
    session_id: SessionId,
    git: Option<&engram_core::types::image::GitConfig>,
    env: &mut HashMap<String, String>,
) {
    let (Some(_forge), Some(git)) = (state.forge.as_ref(), git) else {
        return;
    };
    let token = get_or_mint_broker_token(state, session_id);
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
pub(crate) fn inject_upload_env(
    state: &SharedState,
    session_id: SessionId,
    env: &mut HashMap<String, String>,
) {
    let token = get_or_mint_broker_token(state, session_id);
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

    /// ADR 0037: the generic brokering primitive puts a *placeholder* in
    /// the guest env (never the real value) and returns the policy entry;
    /// the placeholder is deterministic in (session, name) so create and
    /// resume agree across an idle→resume.
    #[test]
    fn broker_secret_injects_placeholder_not_real_value() {
        let sid = SessionId::new();
        let mut env = HashMap::new();
        let entry = broker_secret(
            &mut env,
            sid,
            "CLAUDE_CODE_OAUTH_TOKEN",
            "sk-real-do-not-leak".to_string(),
            vec!["api.anthropic.com".to_string()],
        );
        let ph = env
            .get("CLAUDE_CODE_OAUTH_TOKEN")
            .expect("placeholder injected into env");
        assert!(ph.starts_with("engram_ph_"), "placeholder shape: {ph}");
        assert_ne!(ph, "sk-real-do-not-leak", "real value must never be in env");
        assert_eq!(&entry.placeholder, ph);
        assert_eq!(entry.real_value, "sk-real-do-not-leak");
        assert_eq!(entry.allow_hosts, vec!["api.anthropic.com".to_string()]);
        // Deterministic in (session, name): the resume-time call reproduces it.
        let mut env2 = HashMap::new();
        let entry2 = broker_secret(
            &mut env2,
            sid,
            "CLAUDE_CODE_OAUTH_TOKEN",
            "other".into(),
            vec![],
        );
        assert_eq!(entry.placeholder, entry2.placeholder);
    }

    /// ADR 0037: the brokered harness credential (which is NOT a manifest
    /// `[secrets]` entry) is folded into the resume egress policy so the
    /// proxy substitutes it.
    #[test]
    fn assemble_resume_egress_policy_folds_harness_broker_secret() {
        let manifest = ImageManifest {
            name: "test".into(),
            secret_mode: SecretMode::Literal,
            ..Default::default()
        };
        let bundle = SecretBundle {
            secrets: HashMap::new(),
        };
        let harness_secret = engram_core::types::egress::EgressSecretEntry {
            placeholder: "engram_ph_x_y".into(),
            real_value: "sk-real".into(),
            allow_hosts: vec!["api.anthropic.com".into()],
            allow_host_patterns: vec![],
        };
        let policy = assemble_resume_egress_policy(
            SessionId::new(),
            SandboxId::new(),
            "10.200.0.7".parse().unwrap(),
            &bundle,
            &manifest,
            &HashMap::new(),
            Some(&harness_secret),
        );
        assert_eq!(policy.secrets.len(), 1, "harness broker secret folded in");
        assert_eq!(policy.secrets[0].placeholder, "engram_ph_x_y");
        assert_eq!(policy.secrets[0].real_value, "sk-real");
        assert_eq!(
            policy.secrets[0].allow_hosts,
            vec!["api.anthropic.com".to_string()]
        );
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
            None,
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
            None,
        );

        assert_eq!(policy.guest_ip, guest_ip);
        assert!(
            policy.secrets.is_empty(),
            "secret with no placeholder in env must be skipped, got {:?}",
            policy.secrets,
        );
    }
}
