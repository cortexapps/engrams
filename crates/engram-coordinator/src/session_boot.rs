//! ADR 0048 C5: the boot pipeline, factored out of `create_session` so
//! the queue scanner (C6) can boot a session from its durable row, not
//! just from a live HTTP request.
//!
//! `create_session` does request-parse → resolve (manifest / secrets /
//! env / harness) → reserve a host → **boot on that host**. This module
//! owns the last step — everything after a host is reserved:
//! restore the base snapshot → persist the `Created` row → bind routing
//! → ship egress → `start_agent` → `Active` + events. Both the create
//! handler and the queue scanner build a [`BootInputs`] and call
//! [`boot_on_reserved_host`], so the launch env, the egress policy, and
//! the lifecycle events can't drift between the two paths.
//!
//! Failure disposition is the CALLER's, not this module's: a boot can
//! fail [`BootError::NotStarted`] (sandbox never came up / was torn down,
//! the row is still `pending` — requeue or release the reservation) or
//! [`BootError::Started`] (the sandbox booted but a later step failed,
//! the row reached `Created` — terminal, fail the session). The create
//! handler maps these to 503/500; the scanner to requeue/Failed.

use std::collections::HashMap;

use engram_core::types::sandbox::AgentSpec;
use engram_core::types::session::{SessionSpec, SessionState};
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};

use crate::api::sessions::base_working_set_blob_key;
use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Everything [`boot_on_reserved_host`] needs once a host is reserved —
/// the product of resolving a session's manifest, secrets, env, and
/// harness. Built by the create handler from the request, or by the
/// queue scanner from the durable row.
pub(crate) struct BootInputs {
    pub session_id: SessionId,
    pub spec: SessionSpec,
    /// The image's base snapshot to restore from.
    pub base_snapshot_id: SnapshotId,
    /// The env baked into the restored sandbox (manifest `[env]` +
    /// resolved secrets + `ENGRAM_SESSION_ID`). Also the placeholder
    /// source for the egress policy.
    pub spec_env: HashMap<String, String>,
    /// The resolved harness to spawn. `None` for dev-VM / harness-less
    /// images — a readiness-probe `AgentSpec` is synthesized from
    /// `session_env`. The per-spawn forge/upload broker tokens are NOT
    /// injected here: their PG rows FK to `sessions.id`, which doesn't
    /// exist until `create_session_created` runs in `boot_on_reserved_host`,
    /// so the injection is deferred to there (after the row materializes).
    pub agent: Option<AgentSpec>,
    /// The image's `[git]` block, if any — carried so the deferred
    /// broker-token injection (after the session row exists) can stamp the
    /// forge owner. `None` for non-git images.
    pub git: Option<engram_core::types::image::GitConfig>,
    /// The durable session env agentd applies to harness, `/exec`, and
    /// the shell. Used to synthesize the readiness-probe agent when
    /// `agent` is `None`.
    pub session_env: HashMap<String, String>,
    /// Resolved secrets, for building the egress secret-substitution
    /// entries.
    pub secret_bundle: engram_core::traits::SecretBundle,
    pub network: engram_core::types::image::NetworkPolicy,
    pub secret_mode: engram_core::types::image::SecretMode,
    /// Per-request `secrets` overrides to seal into `session_secrets`
    /// once the row exists (so resume rebuilds the harness env). `None`
    /// when there are none.
    pub deferred_session_secrets: Option<HashMap<String, String>>,
    /// An initial prompt to record as a user-role message once Active.
    pub prompt: Option<String>,
    /// The session's resolved RAM budget (MiB) — carried so a queued
    /// create can report its exact demand without re-resolving.
    pub memory_mib: u32,
    /// The session's resolved vCPU budget.
    pub cpu_budget_vcpus: u32,
}

/// The product of resolving a session's manifest / secrets / env /
/// harness — [`BootInputs`] plus the reserve-side figures the caller
/// needs to place a host (and, for the create handler, the image tag for
/// its response). Built by `sessions::prepare_from_request` (from a live
/// request) or `sessions::prepare_from_row` (from a durable queued row).
pub(crate) struct PreparedBoot {
    pub inputs: BootInputs,
    pub memory_mib: u32,
    pub cpu_budget_vcpus: u32,
    pub image_repo: String,
    pub image_tag: String,
}

/// Why a boot failed, carrying the caller-facing error and — crucially —
/// whether the sandbox/row advanced past the reservation.
pub(crate) enum BootError {
    /// The sandbox never came up (restore failed) or was torn down
    /// before the row left `pending` (row-insert failed). The `pending`
    /// reservation is still the row's state — the caller may requeue it
    /// or release the reservation. No orphaned sandbox.
    NotStarted(ApiError),
    /// The sandbox booted and the row reached `Created`, but a later
    /// step failed (`start_agent`). Terminal: the caller fails the
    /// session. The sandbox has been unbound (host reconcile GCs it).
    Started(ApiError),
}

/// Restore + launch a session on an ALREADY-RESERVED host (the create
/// path reserved it via `reserve_placement`; the queue scanner via
/// `place_queued_session`). Drives the row `pending → created → active`
/// and emits the matching lifecycle events. See the module docs for the
/// failure contract.
pub(crate) async fn boot_on_reserved_host(
    state: &SharedState,
    inputs: BootInputs,
    host_id: HostId,
) -> Result<(), BootError> {
    let BootInputs {
        session_id,
        spec,
        base_snapshot_id,
        spec_env,
        mut agent,
        git,
        session_env,
        secret_bundle,
        network,
        secret_mode,
        deferred_session_secrets,
        prompt,
        memory_mib: _,
        cpu_budget_vcpus: _,
    } = inputs;

    // ---- restore the base snapshot on the reserved host ----
    let record = match state.services.meta.get_snapshot(base_snapshot_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(BootError::NotStarted(ApiError::Internal(format!(
                "enabled image references base snapshot {base_snapshot_id} but its row is gone"
            ))));
        }
        Err(e) => {
            return Err(BootError::NotStarted(ApiError::Internal(format!(
                "get_snapshot {base_snapshot_id} for base restore: {e}"
            ))));
        }
    };
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        base_memory_manifest: None,
        migration_source: None,
        id: base_snapshot_id,
        size_bytes: record.size_bytes,
        created_at: record.created_at,
        image_version: record.image_version,
        disk_manifest: record.disk_manifest,
        memory_manifest: record.memory_manifest,
        source_sandbox_id: None,
        state_blob_key: Some(engram_chunk_store::snapshot_blob::state_blob_key(
            base_snapshot_id,
        )),
        sidecar_blob_key: Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(
            base_snapshot_id,
        )),
        rootfs_blob_key: None,
        working_set_blob_key: base_working_set_blob_key(record.memory_manifest),
        aux_bundles: record.aux_bundles,
    };

    let sandbox_id = match state
        .host_registry
        .restore_base_on_host(host_id, metadata, spec_env.clone())
        .await
    {
        Ok(sb) => sb,
        Err(e) => {
            return Err(BootError::NotStarted(map_restore_error(e)));
        }
    };

    // ---- persist the Created row (pending → created upsert) ----
    if let Err(e) = state
        .services
        .meta
        .create_session_created(session_id, spec, host_id, sandbox_id)
        .await
    {
        tracing::error!(
            %session_id, %sandbox_id, %host_id, error = %e,
            "session row insert failed after sandbox create; tearing sandbox down",
        );
        if let Err(de) = state.services.host.destroy(sandbox_id).await {
            tracing::error!(
                %session_id, %sandbox_id, error = %de,
                "sandbox teardown after insert failure also failed — host reconcile will GC",
            );
        }
        return Err(BootError::NotStarted(e.into()));
    }

    // Seal per-request secret overrides now the FK is satisfiable.
    if let Some(overrides) = deferred_session_secrets {
        if let Err(e) =
            crate::api::sessions::persist_session_secrets(state, session_id, &overrides).await
        {
            tracing::warn!(
                %session_id, error = %e,
                "session secrets persistence failed; resume will lose secrets",
            );
        }
    }

    // Inject the per-spawn forge/upload broker tokens NOW the session row
    // exists. These mint a `session_broker_tokens` row that FKs to
    // `sessions.id`, so doing it any earlier (e.g. while `prepare_inner`
    // builds the AgentSpec, before the row is committed) fails the FK and is
    // silently swallowed to a no-op — which is exactly how git credential
    // injection broke on the gRPC create path (ADR 0051 + the ADR 0047
    // PG-backed broker tokens). Mirrors the secret-persistence above: same
    // row, same FK, same "after create_session_created" placement.
    if let Some(a) = agent.as_mut() {
        crate::api::sessions::inject_harness_env(state, session_id, git.as_ref(), &mut a.env).await;
    }

    // ADR 0047: the session→sandbox binding is persisted by
    // `create_session_created` above; no in-memory registry to update.

    // Egress policy from the resolved guest IP (None on backends without one).
    let egress_policy = build_egress_policy(
        state,
        session_id,
        sandbox_id,
        &secret_bundle,
        &spec_env,
        &network,
        secret_mode,
    )
    .await;

    state
        .services
        .host
        .bind_session(session_id, sandbox_id)
        .await;

    // ---- start the agent ----
    let agent = agent.unwrap_or_else(|| AgentSpec {
        argv: Vec::new(),
        env: HashMap::new(),
        session_env,
        host_ca_pem: None,
    });
    let policy = egress_policy.unwrap_or_else(|| engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
        network_allow_hosts: network.allow_hosts.clone(),
        network_allow_host_patterns: network.allow_host_patterns.clone(),
        secrets: Vec::new(),
        secret_mode,
    });

    // Emit pending → created (the row materialized at Created above).
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Pending,
                to: SessionState::Created,
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        // The row is at Created; an event-emit hiccup shouldn't unwind
        // the boot. Log and press on (subscribers reconcile from the row).
        tracing::warn!(%session_id, error = %e, "emit pending→created failed; continuing");
    }

    if let Err(e) = state
        .services
        .host
        .start_agent(sandbox_id, agent, policy)
        .await
    {
        tracing::error!(
            %session_id, %sandbox_id, %host_id, error = %e,
            "start_agent failed",
        );
        state.services.host.unbind_session(session_id).await;
        return Err(BootError::Started(e.into()));
    }

    // ---- created → active ----
    let prev = match state
        .services
        .meta
        .transition_session(session_id, SessionState::Active)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return Err(BootError::Started(ApiError::Internal(format!(
                "transition to Active failed after start_agent: {e}"
            ))));
        }
    };
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: prev,
                to: SessionState::Active,
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(%session_id, error = %e, "emit created→active failed; continuing");
    }

    // Record the initial prompt as a user-role message (best-effort).
    if let Some(text) = prompt.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = state
            .emit(
                session_id,
                SessionEvent::HarnessAgentMessage {
                    run_id: String::new(),
                    message_id: format!("user-{}", uuid::Uuid::new_v4()),
                    role: engram_harness_proto::AgentRole::User,
                    text: text.to_string(),
                    // Initial-prompt client id threading is deferred (see
                    // create_request_from_proto); the web loads the session
                    // view from server events, so there's no optimistic
                    // first-bubble to dedupe against.
                    prompt_id: None,
                    at: chrono::Utc::now(),
                },
            )
            .await
        {
            tracing::warn!(%session_id, error = %e, "emit initial prompt event failed");
        }
    }

    Ok(())
}

/// Map a restore-side `SandboxError` to the user-facing 503 the create
/// path returned (kept identical so the API contract is unchanged).
fn map_restore_error(e: engram_core::SandboxError) -> ApiError {
    match e {
        engram_core::SandboxError::ImageNotReady(d) => ApiError::Unavailable(format!(
            "image (digest {d}) is not ready on any host yet; retry shortly"
        )),
        other => ApiError::Unavailable(format!(
            "no host could restore this session's base snapshot right now: {other}. \
             Retry shortly; capacity recovers as hosts register or sessions drain."
        )),
    }
}

/// Build the per-session egress policy from the resolved guest IP, or
/// `None` when the backend exposes no guest IP (process backend / some
/// VZ configs) — the caller synthesizes an unspecified-IP fallback.
async fn build_egress_policy(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    secret_bundle: &engram_core::traits::SecretBundle,
    spec_env: &HashMap<String, String>,
    network: &engram_core::types::image::NetworkPolicy,
    secret_mode: engram_core::types::image::SecretMode,
) -> Option<engram_core::types::egress::SessionEgressPolicy> {
    let guest_ip_str = state.services.host.guest_ip(sandbox_id).await?;
    let guest_ip = guest_ip_str.parse::<std::net::Ipv4Addr>().ok()?;
    let mut secrets = Vec::new();
    for (name, resolved) in &secret_bundle.secrets {
        let Some(placeholder) = spec_env.get(name).cloned() else {
            continue;
        };
        secrets.push(engram_core::types::egress::EgressSecretEntry {
            placeholder,
            real_value: resolved.value.clone(),
            allow_hosts: resolved.schema.allow_hosts.clone(),
            allow_host_patterns: resolved.schema.allow_host_patterns.clone(),
        });
    }
    Some(engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip,
        network_allow_hosts: network.allow_hosts.clone(),
        network_allow_host_patterns: network.allow_host_patterns.clone(),
        secrets,
        secret_mode,
    })
}
