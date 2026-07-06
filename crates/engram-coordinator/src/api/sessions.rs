use std::collections::HashMap;

use engram_core::traits::SecretContext;
use engram_core::types::session::{split_image_ref, ImageRef, SessionMode};
use engram_core::types::{ImageManifest, Session, SessionSpec, SessionState};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Default sandbox sizing for sessions created without explicit limits.
/// Phase 1 numbers — will move to per-repo `engram.toml` config later.
pub(crate) const DEFAULT_VCPUS: u32 = 2;
pub(crate) const DEFAULT_MEMORY_MIB: u32 = 4096;
pub(crate) const DEFAULT_DISK_GIB: u32 = 20;

/// Resolved guest memory (MiB) for an image: its `suggested_memory_mib` (or
/// the default). The single source of truth shared by base-snapshot capture
/// (`enabled_images`) and session restore — FC requires the restore
/// `mem_size_mib` to equal the snapshot's, so they MUST compute it
/// identically. ADR 0055: the base snapshot is sized once per image and is
/// skill-agnostic (skills bind via `patch_drive`, never resize memory), so
/// memory-heavy tooling (e.g. browser) is an image-sizing concern —
/// declare `suggested_memory_mib` on the image, not a per-session skill.
pub(crate) fn resolved_memory_mib(manifest: &engram_core::types::ImageManifest) -> u32 {
    manifest
        .resources
        .suggested_memory_mib
        .unwrap_or(DEFAULT_MEMORY_MIB)
}

/// ADR 0048: resolved guest vCPU count for an image. Enable-time
/// validation (`enabled_images::validate_manifest`) guarantees the
/// declaration is present for enabled images; `DEFAULT_VCPUS` is the
/// defensive fallback for the test / non-enabled paths, mirroring
/// `resolved_memory_mib`. This is the budget placement reserves.
pub(crate) fn resolved_vcpus(manifest: &engram_core::types::ImageManifest) -> u32 {
    manifest.resources.suggested_vcpus.unwrap_or(DEFAULT_VCPUS)
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
    // ADR 0057: network is no longer on the manifest. The caller supplies it —
    // base-snapshot capture uses allow-all (a trusted, ephemeral build step;
    // every session that later restores the snapshot gets its own policy
    // network), disk-only recovery passes the session's persisted policy network.
    network: engram_core::types::image::NetworkPolicy,
) -> engram_core::types::sandbox::SandboxSpec {
    use engram_core::types::sandbox::{AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};

    let vcpus = resolved_vcpus(manifest);
    let memory_mib = resolved_memory_mib(manifest);
    let disk_gib = manifest
        .resources
        .suggested_disk_gib
        .unwrap_or(DEFAULT_DISK_GIB);

    // ADR 0055: capture reserves a fixed pool of dynamic-mount slots, each
    // carrying the sentinel. Per-session creates `patch_drive` the selected
    // skills into slots in the paused restore window, so the base snapshot
    // stays skill-agnostic — one per image, not one per skill-combination.
    let aux_ro_drives = (0..AuxRoDrive::RESERVED_SLOTS)
        .map(AuxRoDrive::reserved_slot)
        .collect();

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
        network,
        aux_ro_drives,
    }
}

/// ADR 0057: resolve the session policy's secrets into (env additions, broker
/// egress entries). The image manifest no longer declares secrets — the
/// profile-compiled policy is the sole source.
///
/// Each secret's `secret_ref` is resolved via the composed `SecretStore` (the
/// org-secret backend layered ahead of the deployment one). A `literal` secret
/// is placed in the guest env; a `broker` secret installs a per-session
/// placeholder in the env + an egress entry the proxy substitutes on the
/// secret's `allow_hosts` (the guest never holds the value). A ref that doesn't
/// resolve is skipped + warn-logged — the session still boots, that one secret
/// is just absent (mirrors `resolve_inject_entries`).
///
/// Issue #535 (c): the per-secret `SecretStore` round trips are independent
/// (no secret's resolution depends on another's), so they run concurrently
/// via `join_all` instead of one-at-a-time — the result folds back into the
/// SAME order-insensitive (env map + entries vec) shape a serial loop would
/// have produced.
pub(crate) async fn resolve_policy_secrets(
    state: &SharedState,
    policy: Option<&engram_core::types::IntegrationPolicy>,
    ctx: &SecretContext<'_>,
    session: SessionId,
) -> (
    HashMap<String, String>,
    Vec<engram_core::types::egress::EgressSecretEntry>,
) {
    let mut env: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<engram_core::types::egress::EgressSecretEntry> = Vec::new();
    let Some(policy) = policy else {
        return (env, entries);
    };
    let schema = engram_core::types::image::SecretSchema::default();
    let resolved = futures::future::join_all(policy.secrets.iter().map(|s| {
        let schema = &schema;
        async move {
            let result = state.services.secrets.get(ctx, &s.secret_ref, schema).await;
            (s, result)
        }
    }))
    .await;
    for (s, result) in resolved {
        let value = match result {
            Ok(Some(v)) => v,
            Ok(None) => {
                tracing::warn!(secret_ref = %s.secret_ref, env_var = %s.env_var,
                    "policy secret ref not resolvable; skipping");
                continue;
            }
            Err(e) => {
                tracing::warn!(secret_ref = %s.secret_ref, error = %e,
                    "policy secret resolution failed; skipping");
                continue;
            }
        };
        install_policy_secret(&mut env, &mut entries, s, value, session);
    }
    (env, entries)
}

/// Pure: install one resolved secret value into the env (`literal`) or as a
/// broker placeholder + egress entry. The placeholder written to the env is the
/// SAME string put on the entry (built once here), so broker substitution
/// authenticates by construction — what used to be a cross-module coupling
/// (`apply_secrets_to_env` ⇄ `egress_secret_entries`) is now one function.
fn install_policy_secret(
    env: &mut HashMap<String, String>,
    entries: &mut Vec<engram_core::types::egress::EgressSecretEntry>,
    secret: &engram_core::types::IntegrationSecret,
    value: String,
    session: SessionId,
) {
    match secret.mode {
        engram_core::types::image::SecretMode::Literal => {
            env.insert(secret.env_var.clone(), value);
        }
        engram_core::types::image::SecretMode::Broker => {
            let placeholder = format!(
                "engram_ph_{}_{}",
                session.as_uuid().simple(),
                short_hash(&secret.env_var),
            );
            env.insert(secret.env_var.clone(), placeholder.clone());
            entries.push(engram_core::types::egress::EgressSecretEntry {
                placeholder,
                real_value: value,
                allow_hosts: secret.allow_hosts.clone(),
                allow_host_patterns: secret.allow_host_patterns.clone(),
            });
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

/// Seal the per-request `secrets` overrides under the deployment KEK —
/// PURE crypto, no PG write. Issue #535 (b): the async KEK seal has no
/// place inside `reserve_and_persist_create`'s DB transaction, so it
/// happens here, BEFORE that call, and the sealed row rides
/// `SessionCreateWriteSet::sealed_secrets` into the transaction instead of
/// a separate post-insert `upsert_session_secrets` write. Caller is
/// expected to have validated `overrides` is non-empty.
pub(crate) async fn seal_session_secrets(
    state: &SharedState,
    session_id: SessionId,
    overrides: &HashMap<String, String>,
) -> Result<engram_core::types::registry::SessionSecrets, ApiError> {
    let plaintext = serde_json::to_vec(overrides)
        .map_err(|e| ApiError::Internal(format!("serialize session secrets: {e}")))?;
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher
        .seal(&plaintext)
        .await
        .map_err(|e| ApiError::Internal(format!("seal session secrets: {e}")))?;
    Ok(engram_core::types::registry::SessionSecrets {
        session_id,
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
        created_at: chrono::Utc::now(),
    })
}

/// Material returned by [`resume_manifest_bundle`] — everything the
/// resume path needs from the image manifest and its secret schema
/// to (1) build the post-resume launch env and (2) rebuild the
/// per-session egress policy (§A.1.7) without a second SecretStore
/// round-trip.
pub(crate) struct ResumeManifestBundle {
    pub manifest: ImageManifest,
    /// `manifest.env` with the session policy's secrets applied (ADR 0057:
    /// literal values / broker placeholders). Per-request overrides from
    /// `load_session_secrets` are NOT folded in here — the caller layers them on.
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
    // ADR 0057: the launch env's secrets come from the persisted session policy
    // (re-read from PG, like the egress rebuild), not the manifest. Deterministic
    // placeholders (env var + session id) match the egress entries the proxy
    // gets, so broker substitution authenticates after resume.
    let policy = load_session_policy(state, session.id).await;
    let (policy_secret_env, _egress) =
        resolve_policy_secrets(state, policy.as_ref(), &secret_ctx, session.id).await;
    let mut env: HashMap<String, String> = manifest.env.clone();
    env.extend(policy_secret_env);
    Ok(ResumeManifestBundle { manifest, env })
}

/// ADR 0057: re-read + parse the persisted per-session integration policy.
/// Shared by the resume env + egress rebuilds. A parse/lookup failure is
/// warn-logged and treated as "no policy" (deny-all network, no secrets) — the
/// session still resumes, just without its policy-derived access.
pub(crate) async fn load_session_policy(
    state: &SharedState,
    session_id: SessionId,
) -> Option<engram_core::types::IntegrationPolicy> {
    match state
        .services
        .meta
        .get_session_integration_policy(session_id)
        .await
    {
        Ok(Some(json)) => engram_core::types::IntegrationPolicy::parse(&json).unwrap_or_else(|e| {
            tracing::warn!(%session_id, error = %e,
                "persisted session policy failed to parse on resume; no network/secrets/injection");
            None
        }),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%session_id, error = %e,
                "session policy lookup failed on resume; no network/secrets/injection");
            None
        }
    }
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

    // ADR 0051: the coordinator no longer resolves human identity. Per-user git
    // attribution and the auto-injected Claude token came from `state.auth`,
    // which is removed — the orchestrator owns identity and supplies any such
    // env via the SecretService-injected `identity_env` at create time.

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
    image: &str,
) -> Option<engram_core::types::egress::SessionEgressPolicy> {
    let guest_ip = state.services.host.guest_ip(sandbox_id).await?;
    // ADR 0057: re-read the persisted session policy once → network + secrets +
    // injects (resolved host-side) + observes (pure), so a resumed session
    // re-derives its whole egress policy on the new host (same as create).
    let policy = load_session_policy(state, session_id).await;
    let (repo, tag) = split_image_ref(image);
    let secret_ctx = SecretContext {
        repo,
        image_tag: tag,
    };
    let (_policy_env, egress_secrets) =
        resolve_policy_secrets(state, policy.as_ref(), &secret_ctx, session_id).await;
    let network = policy
        .as_ref()
        .map(|p| p.network.clone())
        .unwrap_or_default();
    let injects =
        crate::session_boot::resolve_inject_entries(state, session_id, policy.as_ref(), image)
            .await;
    let observes = crate::session_boot::build_observe_entries(policy.as_ref());
    Some(assemble_resume_egress_policy(
        session_id,
        sandbox_id,
        guest_ip,
        egress_secrets,
        &network,
        injects,
        observes,
    ))
}

/// Pure synchronous assembly path for the resume egress policy.
/// Split out from [`build_resume_egress_policy`] so unit tests can
/// exercise the placeholder-matching + allow-list cloning logic
/// without standing up a SharedState. The behaviour mirrors the
/// create path's policy build (sessions.rs around line 635) by
/// construction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_resume_egress_policy(
    session_id: SessionId,
    sandbox_id: engram_core::SandboxId,
    guest_ip: std::net::Ipv4Addr,
    egress_secrets: Vec<engram_core::types::egress::EgressSecretEntry>,
    network: &engram_core::types::image::NetworkPolicy,
    injects: Vec<engram_core::types::egress::EgressInjectEntry>,
    observes: Vec<engram_core::types::egress::EgressObserveEntry>,
) -> engram_core::types::egress::SessionEgressPolicy {
    engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip,
        // ADR 0057: network + secrets come from the session policy, not the
        // manifest. Broker substitution entries were precomputed (with the same
        // deterministic placeholders the resumed env carries).
        network_allow_hosts: network.allow_hosts.clone(),
        network_allow_host_patterns: network.allow_host_patterns.clone(),
        allow_all: false,
        secrets: egress_secrets,
        // ADR 0056 (B′): the resolved Plane-B injections (from the persisted
        // policy), so a resumed session re-injects on the new host.
        injects,
        // ADR 0056 (Phase 4): observe specs (from the persisted policy), so a
        // resumed session keeps emitting assets on the new host.
        observes,
        // ADR 0057: per-secret mode; the proxy substitutes per entry. Vestigial.
        secret_mode: engram_core::types::image::SecretMode::Broker,
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
    /// ADR 0055: profile-selected skill bundle names; resolved to reserved-slot
    /// mounts at `prepare_inner` against the fleet's staged bundles. Empty for
    /// non-gRPC / legacy callers.
    #[serde(default)]
    pub selected_skills: Vec<String>,
    /// ADR 0056: profile-granted "provider:action[@resource]" capability
    /// strings, parsed + validated at `prepare_inner` and bound to the session
    /// after its row exists. Empty for non-gRPC / legacy callers.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// ADR 0056 (B′): the orchestrator-compiled per-session integration policy
    /// (gRPC path parses it from `integration_policy_json`). Its inject
    /// `secret_ref`s are resolved host-side into the egress policy at boot.
    /// `None` for non-gRPC / legacy callers.
    #[serde(default)]
    pub integration_policy: Option<engram_core::types::IntegrationPolicy>,
    /// ADR 0062: the selected harness name (a catalog key) for an agent-mode
    /// session — resolved at `prepare_inner` to the `dyn_0` catalog mount + the
    /// `argv` the backend execs. `None` for a dev-VM session.
    #[serde(default)]
    pub selected_harness: Option<String>,
}

#[derive(Serialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
    pub status: &'static str,
    pub image_version: String,
    /// Coarse create disposition (ADR 0020: every create is a
    /// base-snapshot restore): `"restored"` if the session booted,
    /// `"queued"` if it found no capacity and was enqueued (ADR 0048),
    /// `"unknown"` on a pre-boot error. Surfaced to the dashboard badge
    /// and used as the `kind` label on `engram_session_boot_seconds`
    /// without re-running the scheduling decision.
    pub kind: &'static str,
}

/// Shared `engram_session_boot_seconds` + `engram_session_create_total`
/// emission for both create entry points (axum + gRPC). Pulling this out
/// keeps the two paths from drifting on metric labels.
fn record_create_metrics(elapsed: f64, outcome: &'static str, kind: &'static str) {
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
}

/// Classify a create result into the metric `outcome` label.
fn create_outcome(result: &Result<CreateSessionResponse, ApiError>) -> &'static str {
    match result {
        Ok(_) => "success",
        Err(ApiError::BadRequest(_)) => "bad_request",
        Err(ApiError::NotFound(_)) => "image_not_enabled",
        Err(ApiError::Unavailable(_)) => "scheduling_rejected",
        Err(_) => "internal",
    }
}

/// ADR 0051: transport-agnostic create core the app-gRPC `CreateSession`
/// RPC delegates to. The orchestrator owns auth/authz, so there is no
/// human `Principal` here — `identity_env` carries any harness-secret
/// injection (e.g. `CLAUDE_CODE_OAUTH_TOKEN`) the SecretService resolved.
/// Shares the SAME hardened reserve / queue / detached-boot path
/// (`boot_prepared`) as the gRPC surface. The `_owner` parameter is inert
/// (the `sessions.user_id` column was dropped in the gRPC-only cutover);
/// it's retained on the signature so the app-gRPC `CreateSession` RPC needn't
/// change as orchestrator attribution moves entirely into the task model.
#[tracing::instrument(name = "session.create.grpc", skip_all)]
pub(crate) async fn create_session_core(
    state: &SharedState,
    identity_env: HashMap<String, String>,
    req: CreateSessionRequest,
    _owner: Option<String>,
) -> Result<CreateSessionResponse, ApiError> {
    let start = std::time::Instant::now();
    let prepared = match prepare_from_grpc(state, identity_env, &req).await {
        Ok(p) => p,
        Err(e) => {
            let result = Err(e);
            record_create_metrics(
                start.elapsed().as_secs_f64(),
                create_outcome(&result),
                "unknown",
            );
            return result;
        }
    };
    let result = boot_prepared(state, prepared, start).await;
    let kind = match &result {
        Ok(body) => body.kind,
        Err(_) => "unknown",
    };
    record_create_metrics(start.elapsed().as_secs_f64(), create_outcome(&result), kind);
    result
}

/// The shared reserve-and-persist → queue-or-boot → detached-disposition
/// path. Both create entry points (axum + gRPC) hand it a fully-resolved
/// [`PreparedBoot`]; the hardening (ADR 0046 best-fit reservation, ADR 0048
/// queue-on-no-capacity, issue #210 boot detachment, panic backstop) lives
/// here once.
///
/// Issue #535 (b): `reserve_and_persist_create` commits the ENTIRE write-set
/// — the row (placed or queued) plus every satellite (secrets, capabilities,
/// integration policy, harness, selected skills) — in ONE transaction,
/// before any host RPC. The FK-ordering bug class (a satellite write that
/// silently no-ops because the row doesn't exist yet — the ADR 0051
/// forge-token regression) is dead by construction: nothing downstream of
/// this call can observe a partially-written session, so `boot_on_reserved_
/// host` no longer does ANY satellite writes of its own.
async fn boot_prepared(
    state: &SharedState,
    prepared: crate::session_boot::PreparedBoot,
    // Issue #535 (observability): `create_session_core`'s entry instant, so
    // the `coord_prepare` phase covers everything from the RPC landing
    // through the write-set commit — the coordinator-owned serial prefix
    // ahead of the (now-concurrent, host-side) restore work.
    create_start: std::time::Instant,
) -> Result<CreateSessionResponse, ApiError> {
    let crate::session_boot::PreparedBoot {
        inputs,
        memory_mib,
        cpu_budget_vcpus,
        image_repo,
        image_tag,
        manifest_digest,
        needs_uffd_substrate,
    } = prepared;
    let session_id = inputs.session_id;

    // -------- Candidates (ADR 0046 best-fit, ADR 0048 2D) --------
    // Issue #535 (a) "conscious divergence": kept as its own scan (unlike the
    // manifest/fleet-catalog reads folded into the boot bundle) — placement
    // needs a heartbeat-fresh host view, not a cached one.
    //
    // ADR 0036 amendment (issue #538): gate the candidate pool on the
    // image's manifest digest, the per-host half of the fleet chunk-
    // prestage invariant (the enable-scanner's `prestaging` stage is the
    // other half — an `enabled_images` row only exists once the eligible
    // fleet has staged it). `candidates_for` never surfaces
    // `PickError::ImageNotReady` — a digest match that filters every host
    // out just yields empty `RankedCandidates`, so `reserve_and_persist_
    // create` returns `Queued` below and this falls into the SAME queue arm
    // a capacity miss does. A straggler host that hasn't staged yet simply
    // isn't in the ranked pool; its next heartbeat un-gates it.
    let ctx = crate::placement::ScheduleContext {
        repo: &image_repo,
        image_version: &image_tag,
        // ADR 0078: a base snapshot is fleet-wide (prewarmed on many
        // hosts, #538), not a single-host fact — no authoritative
        // affinity on create. (The old `prefer_snapshot_id` tier keyed
        // on the never-populated `local_snapshots`, so this was already
        // a no-op in prod.)
        snapshot_host: None,
        memory_mib: Some(memory_mib),
        cpu_budget_vcpus: Some(cpu_budget_vcpus),
        required_image_digest: Some(engram_protocol::heartbeat::ManifestDigest::new(
            manifest_digest,
        )),
        exclude_host: None,
        prefer_host: None,
        // ADR 0068: a fresh create with an FC memory-manifest base
        // snapshot needs a host with a healthy UFFD substrate. No
        // `fc_snapshot_version` constraint on create — NOT because a
        // create is somehow exempt from the cross-`SNAPSHOT_VERSION`
        // corruption class (a create IS an FC restore of the base
        // snapshot, ADR 0020; there is no warm pool). Base-template rows
        // now DO carry a real `fc_snapshot_version`
        // (`enabled_images.rs::capture_and_record_base_snapshot`), but
        // this `PreparedBoot` assembly only has `enabled.
        // base_snapshot_memory_manifest`, not the base row's recorded
        // version — the query that builds `enabled` would need a new
        // column threaded through before a create could pair against it.
        // Deferred as a follow-up; not done here to keep this review-fix
        // pass scoped to the two `record_snapshot` call sites (PR #564
        // review findings 2/3).
        caps: crate::placement::CapabilityRequirements {
            needs_uffd_substrate,
            fc_snapshot_version: None,
        },
    };
    let candidates = crate::placement::candidates_for(state.services.meta.as_ref(), &ctx)
        .await
        .map_err(engram_core::SandboxError::from)?;
    // ADR 0068 (core-ops-batch correction pass): this path used to fall
    // silently into the `Queued` disposition below with zero visibility
    // into why every host was excluded — the same "no capacity with free
    // hosts" mystery mode `pick_for_session` already fixed on the
    // resume/evac path. Mirror it here.
    if candidates.hosts.is_empty() {
        crate::placement::log_empty_candidates(state.services.meta.as_ref(), &ctx, "create").await;
    }

    // -------- Seal secrets + serialize the policy BEFORE the transaction --------
    // Issue #535 (b): the KEK seal is async crypto with no place inside a DB
    // transaction — do it here, once, and hand the SEALED row (not the
    // plaintext) to `reserve_and_persist_create`.
    let sealed_secrets = match inputs.deferred_session_secrets.as_ref() {
        Some(overrides) => Some(seal_session_secrets(state, session_id, overrides).await?),
        None => None,
    };
    let integration_policy_json = match inputs.integration_policy.as_ref() {
        Some(policy) => Some(
            serde_json::to_string(policy)
                .map_err(|e| ApiError::Internal(format!("serialize integration policy: {e}")))?,
        ),
        None => None,
    };

    let write_set = engram_core::traits::SessionCreateWriteSet {
        session_id,
        spec: inputs.spec.clone(),
        mem_budget_mib: memory_mib as i64,
        cpu_budget_vcpus: cpu_budget_vcpus as i32,
        sealed_secrets,
        capabilities: inputs.capabilities.clone(),
        integration_policy_json,
        // ADR 0077 phase 3: the session's boot inputs as ONE persisted
        // document, written into `session_runtime_specs` inside the create
        // transaction (subsumes the retired `sessions.selected_skills`
        // column). `reserve_and_persist_create` also mirrors the harness key
        // into the pre-existing `sessions.harness` column.
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(
            inputs.selected_skills.clone(),
            inputs.selected_harness.clone(),
            // workdir re-derives from the stable image manifest at boot; it is
            // not a re-derivation-drift source, so it is not persisted here.
            None,
        ),
    };

    let disposition = state
        .services
        .meta
        .reserve_and_persist_create(write_set, &candidates.hosts, candidates.affinity_len)
        .await
        .map_err(|e| ApiError::Internal(format!("reserve_and_persist_create: {e}")))?;

    // ADR 0073 (completion): the create-time initial prompt rides the SAME
    // durable outbox path as every follow-up — enqueue it now that the session
    // row is committed. `send_prompt_core` writes the `prompt_received` receipt
    // + the user echo and INSERTs the outbox row; the delivery driver forwards
    // it once the harness attaches, deferring (retryable) while the session is
    // still Pending/Queued. This covers BOTH dispositions below (a Queued
    // session's prompt is delivered by the driver once the queue scanner boots
    // it) and retires the never-consumed `ENGRAM_INITIAL_PROMPT` env var — #542
    // stamped it but no in-guest consumer was ever written, so create-time
    // prompts were silently dropped. Deterministic `prompt_id` so a create
    // retry dedups against the same outbox row. Best-effort: a failure here
    // logs + proceeds (the session is still usable via a follow-up prompt); we
    // never fail the create over the initial-prompt enqueue.
    if let Some(text) = inputs.prompt.clone().filter(|s| !s.is_empty()) {
        if let Err(e) = crate::api::prompt::send_prompt_core(
            state,
            session_id,
            format!("create:{session_id}"),
            text,
        )
        .await
        {
            tracing::warn!(%session_id, error = %e, "enqueue create-time prompt failed");
        }
    }

    let host_id = match disposition {
        // ADR 0048: no host fits → QUEUE (FIFO) instead of 503 — the row +
        // every satellite already committed above, so there is nothing left
        // to persist here; just tell the caller. The queue scanner
        // re-attempts placement as capacity frees / the fleet scales up, and
        // drives the same boot path (`prepare_from_row` → `boot_on_reserved_
        // host`) once a host fits.
        engram_core::traits::CreateDisposition::Queued => {
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
            return Ok(CreateSessionResponse {
                session_id,
                status: SessionState::Queued.as_str(),
                image_version: image_tag,
                kind: "queued",
            });
        }
        engram_core::traits::CreateDisposition::Placed(host_id) => host_id,
    };

    // -------- Boot on the reserved host --------
    //
    // Issue #210: DETACH the boot pipeline from the cancellable request
    // future. `boot_on_reserved_host` makes a LIVE sandbox in
    // `restore_base_on_host` (session_boot.rs) and only later records it via
    // `transition_session_created`; the compensating `host.destroy` runs
    // solely on that update's `Err` arm, never on a dropped future. Awaited
    // INLINE, a client disconnect between the restore and the update (a slow
    // restore, up to a 240s deadline) drops the future: a running sandbox is
    // left with no recorded binding AND the disposition below — which
    // releases the `pending` reservation / fails the session — never runs,
    // so the reserved capacity stays pinned too.
    //
    // Mirror the resume lane (and ADR 0034 / the #208 teleport detachment):
    // run the boot AND its full disposition in a `tokio::spawn`ed task so the
    // sandbox is always recorded or torn down and the reservation is always
    // released, regardless of the request's fate. The handler awaits the
    // JoinHandle only to shape the connected client's response.
    //
    // Issue #535 (observability): `coord_prepare` ends HERE — everything
    // from `create_session_core` entry through the write-set commit, right
    // before the restore RPC dispatches inside the spawned task.
    metrics::histogram!(crate::metrics::SESSION_BOOT_SECONDS, "phase" => "coord_prepare")
        .record(create_start.elapsed().as_secs_f64());
    // ADR 0019 / telemetry restoration (#526): `tokio::spawn` severs the
    // tracing context — a span created inside this future would otherwise
    // become a new orphaned trace root instead of a child of
    // `session.create.grpc`. `.instrument(Span::current())` re-parents the
    // detached body onto the request span; it changes span context only,
    // not task lifetime, so the detach-for-cancellation-safety property
    // from #210 is unaffected.
    let st = state.clone();
    let boot_span = tracing::Span::current();
    let boot_handle = tokio::spawn(
        async move {
            match crate::session_boot::boot_on_reserved_host(&st, inputs, host_id).await {
                Ok(()) => Ok(()),
                Err(crate::session_boot::BootError::NotStarted(e)) => {
                    // The sandbox never came up; release the reservation row so
                    // the host's free capacity is restored at once (reconcile
                    // would also reap it). No Failed transition — nothing usable
                    // ever existed.
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
        }
        .instrument(boot_span),
    );

    let boot_result = boot_handle.await.map_err(|join_err| {
        // The boot task panicked: it did NOT run its disposition, so the
        // pending reservation may still be pinned. The placement reconcile /
        // stale-pending guard (10 min) reclaims it; surface a 500 so the
        // client doesn't believe the session is live.
        ApiError::Internal(format!("session boot task panicked: {join_err}"))
    })?;

    boot_result.map(|()| CreateSessionResponse {
        session_id,
        status: SessionState::Active.as_str(),
        image_version: image_tag,
        // ADR 0020: every session is a base-snapshot restore now.
        kind: "restored",
    })
}

/// ADR 0051: resolve a session's boot inputs for the app-gRPC create. The
/// orchestrator owns auth/authz, so there is no human `Principal` — it passes
/// an `identity_env` carrying any harness-secret injection (e.g.
/// `CLAUDE_CODE_OAUTH_TOKEN`) the SecretService unsealed. Strict image lookup
/// (a create may only target a LIVE enabled image). Shares [`prepare_inner`]
/// with the queue scanner's [`prepare_from_row`] so the two paths can't drift.
pub(crate) async fn prepare_from_grpc(
    state: &SharedState,
    identity_env: HashMap<String, String>,
    req: &CreateSessionRequest,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    // Issue #535 (a): the cache always resolves the soft-delete-TOLERANT
    // (`_any`) view — this, the strict live-create path, rejects a
    // soft-deleted row itself instead of forking the cache's fill logic
    // (the queued path's `prepare_from_row` accepts it below).
    let bundle = state
        .boot_bundles
        .bundle_for(state.services.meta.as_ref(), &req.image)
        .await?;
    if bundle.enabled.soft_deleted_at.is_some() {
        return Err(ApiError::BadRequest(format!(
            "image `{}` is not enabled. Operators enable images via \
             POST /api/enabled-images before sessions can reference them.",
            req.image
        )));
    }
    prepare_inner(
        state,
        identity_env,
        &req.image,
        req.mode,
        req.prompt.clone(),
        req.secrets.clone(),
        SessionId::new(),
        bundle,
        req.selected_skills.clone(),
        req.capabilities.clone(),
        req.integration_policy.clone(),
        req.selected_harness.clone(),
    )
    .await
}

/// ADR 0048 C6: resolve the same boot inputs from a durable `queued` row,
/// so the queue scanner can boot a session no live request is holding.
/// Secret overrides come from the sealed `session_secrets` row. The
/// create-time prompt is NOT re-delivered here — it was enqueued to the
/// durable outbox when the session was first created (ADR 0073), so the
/// delivery driver forwards it once this boot brings the harness up.
/// Tolerant image lookup (the image may have been soft-deleted while the
/// session waited — the lineage is still pinned).
pub(crate) async fn prepare_from_row(
    state: &SharedState,
    session: &Session,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    let overrides = load_session_secrets(state, session.id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(session_id = %session.id, error = %e,
            "prepare_from_row: secret overrides unavailable; continuing without them");
            None
        });
    // Issue #535 (a): the queued path stays tolerant of a soft-deleted image
    // (the lineage is still pinned) — the cache's `_any` fill is exactly this.
    // Propagate the cache's own error verbatim (missing row / bad manifest /
    // missing snapshot each carry a distinct message already).
    let bundle = state
        .boot_bundles
        .bundle_for(state.services.meta.as_ref(), &session.image)
        .await?;
    // ADR 0056 (B′): re-read the persisted integration policy so the queued
    // boot re-injects (`resolve_inject_entries` resolves its refs again on the
    // new host, via `boot_on_reserved_host`'s overlapped env/egress leg).
    // A malformed/absent blob → None (no injection).
    let integration_policy = match state
        .services
        .meta
        .get_session_integration_policy(session.id)
        .await
    {
        Ok(Some(json)) => engram_core::types::IntegrationPolicy::parse(&json).unwrap_or_else(|e| {
            tracing::warn!(session_id = %session.id, error = %e,
                    "persisted integration policy failed to parse; booting without injection");
            None
        }),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(session_id = %session.id, error = %e,
                "integration policy lookup failed; booting without injection");
            None
        }
    };
    // ADR 0077 phase 3: read the persisted RuntimeSpec so the queued /
    // resumed boot re-resolves the session's dynamic skill NAMES against
    // the current fleet stamp — fixing the ADR 0055 TODO(P1-D) where the
    // scanner booted queued sessions with base skills only. A pre-0077
    // session (no spec row) yields an empty list, i.e. the old behavior —
    // but a READ ERROR (PG blip, spec decode failure) must PROPAGATE, not
    // degrade to "no skills": FC needs skills pre-staged at boot, so a
    // swallowed error here boots the session's whole life without its
    // mounts, silently. The caller's retry (queue scanner tick / resume
    // re-dispatch) is the correct recovery.
    let persisted_skills = state
        .services
        .meta
        .get_session_runtime_spec(session.id)
        .await
        .map_err(|e| {
            ApiError::Internal(format!(
                "runtime spec read for {} failed (retryable — refusing to boot skill-less): {e}",
                session.id
            ))
        })?
        .map(|rs| rs.selected_skills)
        .unwrap_or_default();
    prepare_inner(
        state,
        HashMap::new(),
        &session.image,
        session.mode,
        // The initial prompt already lives in the outbox (enqueued at create);
        // the queued reboot only needs to bring the harness up so the delivery
        // driver can forward it.
        None,
        overrides,
        session.id,
        bundle,
        // ADR 0077 phase 3: the persisted skill NAMES from the RuntimeSpec
        // (the TODO(P1-D) fix) — re-resolved against the current fleet catalog
        // below, since the sha may have rolled while queued.
        persisted_skills,
        // ADR 0056: a queued session's capabilities were already bound to
        // `session_capabilities` at create/enqueue (issue #535 (b): now in
        // the SAME transaction as the row); the re-prepare carries an empty
        // set purely because `capabilities` is no longer read by the boot
        // pipeline at all (see `BootInputs::capabilities` docs).
        Vec::new(),
        // ADR 0056 (B′): the integration policy persisted at create/enqueue,
        // re-read above so the queued boot re-injects on the new host.
        integration_policy,
        // ADR 0062: the harness persisted at create/enqueue, re-read so the
        // queued boot mounts + execs the same harness.
        state
            .services
            .meta
            .get_session_harness(session.id)
            .await
            .map_err(|e| ApiError::Internal(format!("get_session_harness: {e}")))?,
    )
    .await
}

/// Shared resolution for both boot entry points (ADR 0048 C5/C6). Mirrors
/// the original create phases: harness-launchable gate → resolve secrets →
/// build spec_env / session_env / agent / network → resolve the base
/// snapshot + budgets. `identity_env` (ADR 0051) carries an
/// orchestrator-resolved harness-secret injection (e.g.
/// `CLAUDE_CODE_OAUTH_TOKEN`) folded into the session env — empty for the
/// queue path, populated only on the gRPC create.
#[allow(clippy::too_many_arguments)]
/// ADR 0055: resolve profile-selected skill bundle names to reserved-slot mount
/// specs. A name resolves against the **fleet stamp ∪ the org-shared upload
/// catalog** (ADR 0055 P2): the fleet bakes identical bundles, so any active
/// host's `current_bundles` (name -> staged sha) is the baked admin catalog, and
/// a name the fleet doesn't carry is looked up in the `mount_catalog` table.
/// Each skill gets a reserved slot (dyn_0..) + the staged sha the host
/// `patch_drive`s in.
async fn resolve_selected_skills(
    state: &SharedState,
    names: &[String],
) -> Result<Vec<engram_core::types::sandbox::AuxRoDrive>, ApiError> {
    use engram_core::types::sandbox::AuxRoDrive;
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // Cap up front so an over-cap request doesn't trigger N catalog lookups.
    // Slot 0 is the harness (ADR 0062), so skills get RESERVED_SLOTS - 1.
    if names.len() > AuxRoDrive::MAX_SKILL_SLOTS {
        return Err(ApiError::BadRequest(format!(
            "session requested {} skills but only {} skill slots exist (slot {} is the harness)",
            names.len(),
            AuxRoDrive::MAX_SKILL_SLOTS,
            AuxRoDrive::HARNESS_SLOT_INDEX,
        )));
    }
    // Start from the fleet stamp (baked admin bundles)…
    let mut catalog = fleet_bundle_catalog(state).await?;
    // …then fall through to the org-shared upload catalog for any selected name
    // the fleet doesn't carry (ADR 0055 P2).
    for name in names {
        if catalog.contains_key(name) {
            continue;
        }
        if let Some(skill) = state
            .services
            .meta
            .get_skill_by_name(name)
            .await
            .map_err(|e| ApiError::Internal(format!("catalog lookup for skill `{name}`: {e}")))?
        {
            catalog.insert(name.clone(), skill.sha256);
        }
    }
    let view: std::collections::HashMap<&str, &str> = catalog
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assign_skill_slots(&view, names)
}

/// The fleet's baked bundle catalog: any active host's `current_bundles`
/// (name -> staged sha256). All hosts bake the same generations, so the first
/// non-empty report is authoritative; empty if no host has reported yet. Shared
/// by the session-create resolver and `RegisterSkill`'s fleet-name collision
/// check (ADR 0055 P2: catalog names may not shadow a fleet bundle name).
pub(crate) async fn fleet_bundle_catalog(
    state: &SharedState,
) -> Result<std::collections::HashMap<String, String>, ApiError> {
    // Issue #535 (a): read-through the cache instead of a fresh
    // `list_active_hosts` scan every call — invalidated by `pg_listener` on
    // an actual `current_bundles` stamp change (host roll), not per create.
    let catalog = state
        .boot_bundles
        .fleet_catalog(state.services.meta.as_ref())
        .await?;
    Ok((*catalog).clone())
}

/// Pure half of skill resolution (no I/O): assign each selected skill name to a
/// reserved slot (dyn_0..) carrying its staged content sha from `catalog` (the
/// combined fleet ∪ upload-catalog view). Caps at `RESERVED_SLOTS`; a name in
/// neither source is a 400.
fn assign_skill_slots(
    catalog: &std::collections::HashMap<&str, &str>,
    names: &[String],
) -> Result<Vec<engram_core::types::sandbox::AuxRoDrive>, ApiError> {
    use engram_core::types::sandbox::AuxRoDrive;
    if names.len() > AuxRoDrive::MAX_SKILL_SLOTS {
        return Err(ApiError::BadRequest(format!(
            "session requested {} skills but only {} skill slots exist (slot {} is the harness)",
            names.len(),
            AuxRoDrive::MAX_SKILL_SLOTS,
            AuxRoDrive::HARNESS_SLOT_INDEX,
        )));
    }
    let mut mounts = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        // Skills occupy dyn_1.. — slot 0 is reserved for the harness (ADR 0062).
        let slot = AuxRoDrive::HARNESS_SLOT_INDEX + 1 + i;
        let sha = catalog.get(name.as_str()).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "skill `{name}` is unknown (not a staged fleet bundle and not in the \
                 upload catalog), or no host has reported its bundle yet"
            ))
        })?;
        mounts.push(AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(slot),
            guest_mount: AuxRoDrive::slot_guest_mount(slot),
            fs_type: "squashfs".into(),
            sha256: Some((*sha).to_string()),
        });
    }
    Ok(mounts)
}

#[allow(clippy::too_many_arguments)] // cohesive session-create inputs; threading a struct buys nothing
async fn prepare_inner(
    state: &SharedState,
    identity_env: HashMap<String, String>,
    image_uri: &str,
    mode: SessionMode,
    prompt: Option<String>,
    secret_overrides: Option<HashMap<String, String>>,
    session_id: SessionId,
    // Issue #535 (a): the per-enabled-image boot bundle (manifest already
    // parsed, base snapshot already fetched) — a read-through cache fill,
    // not per-create I/O. `Arc` because the cache hands out shared handles.
    bundle: std::sync::Arc<crate::boot_bundle::BootBundle>,
    // ADR 0055: profile-selected skill names; resolved to reserved-slot mounts.
    selected_skills: Vec<String>,
    // ADR 0056: profile-granted "provider:action[@resource]" capability strings.
    capabilities: Vec<String>,
    // ADR 0056 (B′): the orchestrator-compiled integration policy, if any.
    integration_policy: Option<engram_core::types::IntegrationPolicy>,
    // ADR 0062: the selected harness name (catalog key) — resolved to the dyn_0
    // catalog mount + argv, and persisted so queue/resume can reconstruct it.
    selected_harness: Option<String>,
) -> Result<crate::session_boot::PreparedBoot, ApiError> {
    // ADR 0021 P1.3: a dev-VM session leaves any baked harness undriven,
    // so a prompt is meaningless — reject it explicitly.
    if mode.is_dev_vm() && !prompt.as_deref().map(str::is_empty).unwrap_or(true) {
        return Err(ApiError::BadRequest(
            "`prompt` requires `mode = agent` — a dev-VM session has no agent to receive it".into(),
        ));
    }

    // ADR 0056: parse + validate the capability strings now (a malformed one
    // is a create-time 400, mirroring the skills cap check). Nothing enforces
    // them yet; they are bound to the session after its row exists.
    let capabilities = capabilities
        .iter()
        .map(|s| engram_core::types::Capability::parse(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ApiError::BadRequest(format!("invalid capability: {e}")))?;

    let (image_repo, image_tag) = {
        let (r, t) = split_image_ref(image_uri);
        (r.to_string(), t.to_string())
    };
    // Issue #535 (a): the manifest was parsed ONCE at bundle-fill time (bake
    // or cache-refresh), not per create — no `toml::from_str` on this path.
    let manifest = &bundle.manifest;
    // ADR 0062: the harness is no longer baked into the image — it's selected
    // per session and resolved from the catalog below (`resolve_harness`). The
    // image's `[harness]` block, if any legacy one survives, is ignored here.

    // -------- ADR 0057: resolve the session policy's secrets --------
    // The image manifest no longer declares secrets; the profile-compiled
    // policy is the sole source. Literal secrets fold into the guest env;
    // broker secrets become egress substitution entries (placeholder in env).
    let secret_ctx = SecretContext {
        repo: &image_repo,
        image_tag: &image_tag,
    };
    let (policy_secret_env, egress_secrets) =
        resolve_policy_secrets(state, integration_policy.as_ref(), &secret_ctx, session_id).await;

    let spec = SessionSpec {
        image: image_uri.to_string(),
        mode,
    };

    // -------- Build the sandbox env --------
    // Base = image manifest `[env]`; then the policy's secrets (literal values /
    // broker placeholders) layered on.
    let mut spec_env: HashMap<String, String> = manifest.env.clone();
    spec_env.extend(policy_secret_env);

    // The map that gets sealed into `session_secrets` and replayed on resume.
    // It folds together (a) Literal-mode per-request user overrides and (b) the
    // orchestrator-resolved `identity_env` (ADR 0051) — independent of
    // `secret_mode`, because identity_env is the trusted orchestrator's harness
    // injection, NOT a user override. identity_env wins on key collision.
    // `deferred_session_secrets` stays `None` when the merged map is empty, so
    // the queue path (empty identity_env, no Literal overrides) keeps exactly
    // today's behavior.
    let mut deferred_map: HashMap<String, String> = HashMap::new();
    // Per-request `secrets` overrides are literal env injected verbatim (and
    // sealed for resume). ADR 0057: no manifest `secret_mode` gate any more —
    // an override is inherently a literal value the caller supplied.
    if let Some(overrides) = secret_overrides.as_ref() {
        for (name, value) in overrides {
            spec_env.insert(name.clone(), value.clone());
            deferred_map.insert(name.clone(), value.clone());
        }
    }

    let mut session_env = spec_env.clone();
    session_env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());

    // ADR 0051: fold the orchestrator-resolved harness identity env (e.g.
    // CLAUDE_CODE_OAUTH_TOKEN) into the session env for the live launch, AND
    // into the deferred map so it is sealed into `session_secrets` and
    // replayed on resume. identity_env wins on key collision (over Literal
    // overrides). Empty for the queue path; populated only on the gRPC create
    // where the orchestrator resolved it. NEVER log these values.
    for (k, v) in identity_env {
        session_env.insert(k.clone(), v.clone());
        deferred_map.insert(k, v);
    }

    let deferred_session_secrets: Option<HashMap<String, String>> = if deferred_map.is_empty() {
        None
    } else {
        Some(deferred_map)
    };

    // The forge/upload broker-token injection is NOT done here: minting is a
    // per-spawn, not a durable, write, so it's deferred to `boot_on_reserved_
    // host`'s overlapped env/egress leg (issue #535 (c)) — by the time that
    // runs, the row has existed since `reserve_and_persist_create` committed
    // the whole write-set transactionally (issue #535 (b)), so the FK it
    // mints against (`session_broker_tokens` → `sessions.id`) is always
    // satisfiable. This used to be a hazard here (the pre-#535 shape minted
    // straight off `prepare_inner`, before any row existed at all — the
    // git-credential-injection regression on the gRPC create path); it's
    // dead by construction now, not by convention. The git config rides
    // `BootInputs` so the deferred injection can stamp the forge owner.
    // ADR 0062: resolve the per-session harness from the catalog → the AgentSpec
    // the backend execs + the `dyn_0` mount carrying the current catalog
    // generation. `None` for a dev VM (no harness, dyn_0 stays sentinel).
    let (agent, harness_mount) = match resolve_harness(
        state,
        selected_harness.as_deref(),
        mode,
        session_id,
        session_env.clone(),
        manifest.workdir.clone(),
    )
    .await?
    {
        Some((spec, mount)) => (Some(spec), Some(mount)),
        None => (None, None),
    };

    // ADR 0057: egress network comes from the session policy (deny-all when the
    // session has no policy — e.g. a direct/CLI create), never the manifest.
    let network = integration_policy
        .as_ref()
        .map(|p| p.network.clone())
        .unwrap_or_default();

    // -------- Base snapshot + budgets --------
    // Issue #535 (a): the record was fetched ONCE at bundle-fill time
    // (`bundle_for` already errors if the enabled image has no base
    // snapshot) — no per-create `get_snapshot` on this path.
    let base_snapshot_id = bundle.base_snapshot.id;
    // ADR 0055: resolve the profile's selected skill names to reserved-slot
    // mounts against the fleet's staged bundles (name -> sha). Capped at
    // RESERVED_SLOTS; an unknown skill name is a 400.
    let mut selected_mounts = resolve_selected_skills(state, &selected_skills).await?;
    // ADR 0062: the harness catalog rides `dyn_0` alongside the skills (dyn_1..),
    // bound through the identical paused-window patch_drive path.
    if let Some(mount) = harness_mount {
        selected_mounts.push(mount);
    }
    // Issue #535 (a): already resolved at bundle-fill time; reuse rather
    // than recompute (identical inputs, so identical outputs).
    let memory_mib = bundle.memory_mib;
    let cpu_budget_vcpus = bundle.cpu_budget_vcpus;
    // ADR 0036 amendment (issue #538): carried so `boot_prepared` can gate
    // the reserve-side `ScheduleContext` on `required_image_digest`.
    let manifest_digest = bundle.enabled.manifest_digest.clone();

    Ok(crate::session_boot::PreparedBoot {
        inputs: crate::session_boot::BootInputs {
            session_id,
            spec,
            base_snapshot_id,
            base_snapshot: bundle.base_snapshot.clone(),
            spec_env,
            agent,
            session_env,
            egress_secrets,
            network,
            selected_mounts,
            // ADR 0077 phase 3: the raw skill names, persisted in the
            // RuntimeSpec so a queued re-prepare / resume re-resolves them.
            selected_skills,
            capabilities,
            integration_policy,
            selected_harness,
            deferred_session_secrets,
            prompt: prompt.filter(|s| !s.is_empty()),
        },
        memory_mib,
        cpu_budget_vcpus,
        image_repo,
        image_tag,
        manifest_digest,
        // ADR 0068: this create restores the enabled image's base
        // snapshot — the placement gate needs the UFFD substrate iff
        // that base snapshot carries a memory manifest (an FC image;
        // VZ/Process enabled-image rows never set this).
        needs_uffd_substrate: bundle.enabled.base_snapshot_memory_manifest.is_some(),
    })
}
/// ADR 0051: fetch a session by id (gRPC `GetSession`). 404 on unknown id.
pub(crate) async fn get_session_core(
    state: &SharedState,
    id: SessionId,
) -> Result<Session, ApiError> {
    Ok(state.services.meta.get_session(id).await?)
}

/// ADR 0051: the trusted-caller session list (gRPC `ListSessions`): ALL
/// active sessions, no owner scoping. Owner annotation (`owner_email` /
/// `owner_name`) is left empty — the orchestrator resolves identities from
/// its own task model.
pub(crate) async fn list_sessions_core(
    state: &SharedState,
) -> Result<ListSessionsResponse, ApiError> {
    let all = state.services.meta.list_active_sessions().await?;
    let sessions = all
        .into_iter()
        .map(|session| SessionListItem {
            session,
            owner_email: None,
            owner_name: None,
        })
        .collect();
    Ok(ListSessionsResponse { sessions })
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

/// ADR 0051: terminate a session + tear down its sandbox (gRPC
/// `DeleteSession`). Idempotent: an already-terminal or unknown-then-raced
/// session returns `Ok(())`. Holds the SAME hardened terminate-first /
/// CAS-guarded teardown logic as the axum `delete_session` handler — only
/// the return shape changed (`StatusCode` → `()`).
pub(crate) async fn delete_session_core(
    state: &SharedState,
    id: SessionId,
) -> Result<(), ApiError> {
    // Drive the session to its FSM-legal terminal BEFORE destroying the
    // sandbox. `terminate_session` reads the current state and picks the
    // terminal `SessionState::terminal_target` permits — `Completed` for
    // states that ran, `Failed` for the early states (Pending / Created)
    // that never became usable (this is what fixes the
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
        Ok(None) => return Ok(()),
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
            return Ok(());
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
    Ok(())
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
    env: &mut HashMap<String, String>,
) {
    inject_forge_env(state, session_id, env).await;
    // ADR 0026: artifact-upload token, injected for every image
    // (not git-gated) so the baked `engram-share` skill always works.
    inject_upload_env(state, session_id, env).await;
}

pub(crate) async fn inject_forge_env(
    state: &SharedState,
    session_id: SessionId,
    env: &mut HashMap<String, String>,
) {
    // ADR 0056 P2: forge env is injected when the session holds a capability for a
    // provider that (a) resolves a mint engine AND (b) declares a git-forge host
    // (`Integration::git_forge_host`). De-hardcodes the old `"github"` literal — any
    // git-forge integration (github today; gitlab/gitea later) gets the askpass env.
    // The manifest `[git]` block is retired — git binding rides the profile's caps.
    let caps = state
        .services
        .meta
        .get_session_capabilities(session_id)
        .await
        .unwrap_or_default();
    // The session's bound caps that belong to a git-forge provider.
    let mut git_caps: Vec<&engram_core::types::Capability> = Vec::new();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for provider in caps.iter().map(|c| c.provider.as_str()) {
        if !seen.insert(provider) {
            continue;
        }
        let is_git_forge = state
            .integrations
            .resolve(provider, &state.services.secrets)
            .await
            .and_then(|e| e.git_forge_host())
            .is_some();
        if is_git_forge {
            git_caps.extend(caps.iter().filter(|c| c.provider == provider));
        }
    }
    if git_caps.is_empty() {
        return;
    }
    let Some(token) = get_or_mint_broker_token(state, session_id).await else {
        // A git-forge-capable session with no broker token means the in-guest
        // gitconfig gets no credential helper and every git op fails with
        // "could not read Username". This used to be silent (the mint FK-failed
        // before the session row existed); it must never be quiet again.
        tracing::error!(
            %session_id,
            "git-forge-capable session got no broker token; git credentials will be UNAVAILABLE in-guest",
        );
        return;
    };
    env.insert("ENGRAM_FORGE_TOKEN".into(), token);
    // Owner hint: the org segment of the first git-forge capability that scopes a
    // resource (`github:contents:write@owner/repo` → `owner`). Omitted otherwise —
    // the single-installation App resolves the installation without it.
    if let Some(owner) = git_caps.iter().find_map(|c| {
        c.resource
            .as_deref()
            .and_then(|r| r.split('/').next())
            .filter(|o| !o.is_empty())
    }) {
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

/// ADR 0062: resolve the session's **selected** harness into the [`AgentSpec`]
/// the backend spawns at `start_agent` + the `dyn_0` catalog mount it execs
/// from. Returns `None` for a dev VM (the catalog isn't mounted; any resident
/// agent stays undriven). For an agent-mode session it returns
/// `Some((agent_spec, harness_mount))`, where `harness_mount` is the current
/// catalog generation bound on `dyn_0` (the SAME sha for every session at a
/// given catalog version — selection lives in `argv`, not the drive content).
///
/// `argv[0] = /opt/engram/dyn/0/<name>/<exec>` (the selected harness's launch
/// entry within the shared catalog squashfs), with backend-specific dial flags:
/// - `HostTcp` (Process): `--connect <host:port>` against the coord-owned
///   harness listener.
/// - `Vsock` (FC/VZ): `--vsock-host <port>` for AF_VSOCK loopback into the host.
///
/// The harness's descriptor `args` ride after the standard flags.
pub(crate) async fn resolve_harness(
    state: &SharedState,
    selected_harness: Option<&str>,
    session_mode: SessionMode,
    session_id: SessionId,
    session_env: HashMap<String, String>,
    workdir: Option<String>,
) -> Result<
    Option<(
        engram_core::types::sandbox::AgentSpec,
        engram_core::types::sandbox::AuxRoDrive,
    )>,
    ApiError,
> {
    use engram_core::types::sandbox::AuxRoDrive;

    if session_mode.is_dev_vm() {
        return Ok(None);
    }
    let Some(name) = selected_harness else {
        return Err(ApiError::BadRequest(
            "an agent-mode session requires a harness selection (none provided)".into(),
        ));
    };

    // Resolve the harness to (descriptor, squashfs sha) — mirroring
    // resolve_selected_skills' "fleet stamp ∪ catalog" lookup. A built-in rides the
    // host-image `current_bundles` stamp + an embedded descriptor; a custom harness
    // rides the `harness_catalog` (its own squashfs, materialized like an uploaded
    // skill). Built-ins win, so a custom row can never shadow one.
    let (descriptor, harness_sha) = if let Some(builtin) = crate::builtin_harness::builtin(name) {
        let descriptor = builtin.descriptor().map_err(|e| {
            ApiError::Internal(format!("built-in harness `{name}` descriptor: {e}"))
        })?;
        let sha = fleet_bundle_catalog(state)
            .await?
            .get(builtin.stamp_key)
            .cloned()
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "built-in harness `{name}` squashfs (`{}`) is not staged on any host yet",
                    builtin.stamp_key
                ))
            })?;
        (descriptor, sha)
    } else if let Some(row) = state
        .services
        .meta
        .get_harness_by_name(name)
        .await
        .map_err(|e| ApiError::Internal(format!("harness catalog lookup for `{name}`: {e}")))?
    {
        let descriptor = row.descriptor().map_err(|e| {
            ApiError::Internal(format!(
                "stored harness.toml for `{name}` failed to parse: {e}"
            ))
        })?;
        (descriptor, row.squashfs_sha256)
    } else {
        return Err(ApiError::BadRequest(format!(
            "harness `{name}` is not a built-in and is not registered in the catalog"
        )));
    };

    // argv[0] = the harness's launch entry within its own squashfs on dyn_0.
    let exec = format!(
        "{}/{}",
        AuxRoDrive::slot_guest_mount(AuxRoDrive::HARNESS_SLOT_INDEX).display(),
        descriptor.exec_path(),
    );

    // Harness-only extras, layered on top of `session_env` for the harness
    // child. `session_env` already carries ENGRAM_SESSION_ID + the image env +
    // secrets, so they aren't repeated here; the forge broker token is added by
    // the caller (per-spawn, kept out of the cached env).
    //
    // ADR 0073 (completion): the create-time initial prompt is NOT stamped into
    // the spawn env — it rides the durable outbox exactly like every follow-up
    // (enqueued in `create_session_core` right after the session row commits;
    // the delivery driver forwards it once the harness attaches). #542 stamped
    // an `ENGRAM_INITIAL_PROMPT` here that no in-guest consumer ever read, so
    // create-time prompts were silently dropped — that env var is retired.
    let mut env: HashMap<String, String> = HashMap::new();
    // The manifest `workdir` reaches agentd as a reserved env entry so the
    // harness child starts there instead of `/`. See `HARNESS_CWD_ENV` for why
    // this isn't a wire-struct field.
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
                exec,
                "--connect".into(),
                addr.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
        engram_core::traits::HarnessDial::Vsock => {
            let port = engram_harness_proto::HARNESS_VSOCK_PORT;
            vec![
                exec,
                "--vsock-host".into(),
                port.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
    };
    // Harness descriptor `args` ride after the standard dial flags so they can
    // specialise the adapter without overriding the SDK contract.
    argv.extend(descriptor.args.iter().cloned());

    let agent = engram_core::types::sandbox::AgentSpec {
        // ADR 0073: stamped with the real minted epoch in session_boot
        // (the mint happens once the sessions row exists); 0 = unstamped.
        binding_epoch: 0,
        argv,
        env,
        session_env,
        host_ca_pem: None,
    };
    let mount = AuxRoDrive {
        drive_id: AuxRoDrive::slot_drive_id(AuxRoDrive::HARNESS_SLOT_INDEX),
        guest_mount: AuxRoDrive::slot_guest_mount(AuxRoDrive::HARNESS_SLOT_INDEX),
        fs_type: "squashfs".into(),
        sha256: Some(harness_sha),
    };
    Ok(Some((agent, mount)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::ImageManifest;
    use engram_core::SandboxId;

    /// ADR 0055: memory is purely the image's `suggested_memory_mib` (or the
    /// default) — the base snapshot is sized once per image and skills bind via
    /// `patch_drive` without resizing it. Capture and restore both call this so
    /// FC's "restore mem_size must equal snapshot mem_size" holds.
    #[test]
    fn resolved_memory_mib_is_suggested_or_default() {
        let mk = |mem: Option<u32>| {
            let mut m = ImageManifest {
                name: "x".into(),
                ..Default::default()
            };
            m.resources.suggested_memory_mib = mem;
            m
        };
        // Unset → default.
        assert_eq!(resolved_memory_mib(&mk(None)), DEFAULT_MEMORY_MIB);
        // Set → honored verbatim, both below and above the default.
        assert_eq!(resolved_memory_mib(&mk(Some(256))), 256);
        assert_eq!(resolved_memory_mib(&mk(Some(8192))), 8192);
    }

    /// ADR 0057: broker substitution authenticates only because the placeholder
    /// `install_policy_secret` writes into the guest env is the SAME string it
    /// puts on the egress entry the proxy substitutes against. Built once in one
    /// function now (no longer split across modules), but locked here regardless.
    /// The proxy half (placeholder → real_value on an allowed host) is covered by
    /// `engram-egress-proxy/tests/intercept_e2e.rs`.
    #[test]
    fn broker_policy_secret_env_placeholder_matches_egress_entry() {
        use engram_core::types::{image::SecretMode, IntegrationSecret};
        let session = SessionId::new();

        // Broker: env carries a placeholder (never the real value); the egress
        // entry carries the SAME placeholder + the real value + allow_hosts.
        let broker = IntegrationSecret {
            secret_ref: "openai-key".into(),
            env_var: "OPENAI_API_KEY".into(),
            mode: SecretMode::Broker,
            allow_hosts: vec!["api.openai.com".into()],
            allow_host_patterns: vec![],
        };
        let mut env = HashMap::new();
        let mut entries = Vec::new();
        install_policy_secret(
            &mut env,
            &mut entries,
            &broker,
            "sk-real-value".into(),
            session,
        );
        let env_ph = env.get("OPENAI_API_KEY").expect("placeholder injected");
        assert_ne!(
            env_ph, "sk-real-value",
            "broker must not leak the real value into env"
        );
        assert!(env_ph.starts_with("engram_ph_"));
        assert_eq!(entries.len(), 1);
        assert_eq!(
            &entries[0].placeholder, env_ph,
            "the proxy's substitution target must equal the env placeholder"
        );
        assert_eq!(entries[0].real_value, "sk-real-value");
        assert_eq!(entries[0].allow_hosts, vec!["api.openai.com".to_string()]);

        // Literal: env carries the real value; no egress entry (the guest holds it).
        let literal = IntegrationSecret {
            secret_ref: "db-url".into(),
            env_var: "DATABASE_URL".into(),
            mode: SecretMode::Literal,
            allow_hosts: vec![],
            allow_host_patterns: vec![],
        };
        let mut env_lit = HashMap::new();
        let mut entries_lit = Vec::new();
        install_policy_secret(
            &mut env_lit,
            &mut entries_lit,
            &literal,
            "postgres://x".into(),
            session,
        );
        assert_eq!(env_lit.get("DATABASE_URL").unwrap(), "postgres://x");
        assert!(
            entries_lit.is_empty(),
            "literal secrets produce no egress entry"
        );
    }

    /// ADR 0055: the pure half of skill resolution assigns each selected name a
    /// reserved slot (dyn_0..) carrying its staged sha, caps at RESERVED_SLOTS,
    /// and rejects unknown names. The host-list → catalog half is exercised by
    /// the e2e stack; this covers the slot-assignment + validation logic.
    #[test]
    fn assign_skill_slots_maps_caps_and_rejects() {
        use engram_core::types::sandbox::AuxRoDrive;
        let catalog: std::collections::HashMap<&str, &str> =
            [("skills", "sha_a"), ("browser", "sha_b")]
                .into_iter()
                .collect();

        // Empty selection → empty mounts.
        assert!(assign_skill_slots(&catalog, &[]).unwrap().is_empty());

        // ADR 0062: slot 0 is the harness, so skills start at dyn_1. Two skills
        // → two drives at dyn_1 / dyn_2 with the catalog shas, in request order.
        let mounts = assign_skill_slots(&catalog, &["skills".into(), "browser".into()]).unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].drive_id, AuxRoDrive::slot_drive_id(1));
        assert_eq!(mounts[0].guest_mount, AuxRoDrive::slot_guest_mount(1));
        assert_eq!(mounts[0].fs_type, "squashfs");
        assert_eq!(mounts[0].sha256.as_deref(), Some("sha_a"));
        assert_eq!(mounts[1].drive_id, AuxRoDrive::slot_drive_id(2));
        assert_eq!(mounts[1].sha256.as_deref(), Some("sha_b"));

        // Unknown skill → 400.
        let err = assign_skill_slots(&catalog, &["nope".into()]).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        // Over the skill-slot cap (RESERVED_SLOTS - 1, since the harness takes
        // slot 0) → 400, even if every name is known.
        let too_many: Vec<String> = (0..AuxRoDrive::MAX_SKILL_SLOTS + 1)
            .map(|_| "skills".to_string())
            .collect();
        let err = assign_skill_slots(&catalog, &too_many).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    /// ADR 0016 §A.1.7 / ADR 0057 regression guard. The pure assembly path must
    /// stamp the real `guest_ip` (was UNSPECIFIED pre-fix), carry the policy's
    /// `network.allow_*` lists, and pass the precomputed broker egress entries
    /// (placeholder + real value + per-secret allow lists) straight through.
    #[test]
    fn assemble_resume_egress_policy_uses_real_guest_ip_and_allow_lists() {
        use engram_core::types::egress::EgressSecretEntry;
        use engram_core::types::image::{NetworkDefault, NetworkPolicy};
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.7".parse().unwrap();

        let network = NetworkPolicy {
            default: NetworkDefault::Deny,
            allow_hosts: vec!["registry.npmjs.org".into()],
            allow_host_patterns: vec!["*.openai.com".into()],
        };
        let egress_secrets = vec![EgressSecretEntry {
            placeholder: "engram_ph_test_abcd1234".into(),
            real_value: "sk-real-secret-do-not-leak".into(),
            allow_hosts: vec!["api.anthropic.com".into()],
            allow_host_patterns: vec!["*.anthropic.com".into()],
        }];

        let policy = assemble_resume_egress_policy(
            session_id,
            sandbox_id,
            guest_ip,
            egress_secrets,
            &network,
            Vec::new(),
            Vec::new(),
        );

        // Real IP, not UNSPECIFIED.
        assert_eq!(policy.guest_ip, guest_ip);
        assert_ne!(policy.guest_ip, std::net::Ipv4Addr::UNSPECIFIED);
        assert_eq!(policy.session_id, session_id);
        assert_eq!(policy.sandbox_id, sandbox_id);
        // Network allow lists come from the policy, not the manifest.
        assert_eq!(policy.network_allow_hosts, vec!["registry.npmjs.org"]);
        assert_eq!(policy.network_allow_host_patterns, vec!["*.openai.com"]);
        // The broker entry passes through verbatim.
        assert_eq!(policy.secrets.len(), 1);
        let entry = &policy.secrets[0];
        assert_eq!(entry.placeholder, "engram_ph_test_abcd1234");
        assert_eq!(entry.real_value, "sk-real-secret-do-not-leak");
        assert_eq!(entry.allow_hosts, vec!["api.anthropic.com"]);
        assert_eq!(entry.allow_host_patterns, vec!["*.anthropic.com"]);
    }

    /// ADR 0057: a session with no policy (e.g. a direct/CLI create) resumes to
    /// a deny-all network with no secrets — the secure default, no manifest.
    #[test]
    fn assemble_resume_egress_policy_empty_policy_is_deny_all() {
        use engram_core::types::image::NetworkPolicy;
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        let guest_ip: std::net::Ipv4Addr = "10.200.0.8".parse().unwrap();
        let policy = assemble_resume_egress_policy(
            session_id,
            sandbox_id,
            guest_ip,
            Vec::new(),
            &NetworkPolicy::default(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(policy.guest_ip, guest_ip);
        assert!(policy.network_allow_hosts.is_empty());
        assert!(policy.network_allow_host_patterns.is_empty());
        assert!(policy.secrets.is_empty());
    }
}
