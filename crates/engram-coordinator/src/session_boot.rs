//! ADR 0048 C5: the boot pipeline, factored out of `create_session` so
//! the queue scanner (C6) can boot a session from its durable row, not
//! just from a live HTTP request.
//!
//! `create_session` does request-parse → resolve (manifest / secrets /
//! env / harness, from the boot-bundle cache — issue #535 (a)) →
//! transactionally reserve-and-persist the WHOLE write-set (issue #535
//! (b)) → **boot on that host**. This module owns the last step —
//! everything after a host is reserved AND the row/satellites already
//! committed: overlap the restore RPC with the sandbox-independent
//! env/egress leg (issue #535 (c)) → flip the row to `Created` + bind
//! `sandbox_id` → ship egress → `start_agent` → `Active` + events →
//! deliver the (possibly initial) prompt over the wire (issue #535 (d)).
//! Both the create handler and the queue scanner build a [`BootInputs`]
//! and call [`boot_on_reserved_host`], so the launch env, the egress
//! policy, and the lifecycle events can't drift between the two paths.
//!
//! Failure disposition is the CALLER's, not this module's: a boot can
//! fail [`BootError::NotStarted`] (sandbox never came up / was torn down,
//! the row is still `pending` — requeue or release the reservation) or
//! [`BootError::Started`] (the sandbox booted but a later step failed,
//! the row reached `Created` — terminal, fail the session). The create
//! handler maps these to 503/500; the scanner to requeue/Failed. A
//! prompt-delivery failure past the reattach budget is also `Started`
//! (terminal) — a session that can't receive the prompt that created it
//! is broken.

use std::collections::HashMap;

use engram_core::types::sandbox::AgentSpec;
use engram_core::types::session::{SessionSpec, SessionState};
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};

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
    /// Issue #535 (a): the snapshot row itself, resolved ONCE at boot-bundle
    /// fill time (bake or cache-refresh) rather than per create — subsumes
    /// the `get_snapshot` call `boot_on_reserved_host` used to make here.
    pub base_snapshot: engram_core::types::SnapshotRecord,
    /// The env baked into the restored sandbox (manifest `[env]` +
    /// resolved secrets + `ENGRAM_SESSION_ID`). Also the placeholder
    /// source for the egress policy.
    pub spec_env: HashMap<String, String>,
    /// The resolved harness to spawn. `None` for dev-VM / harness-less
    /// images — a readiness-probe `AgentSpec` is synthesized from
    /// `session_env`. The per-spawn forge/upload broker tokens are NOT
    /// injected here: minting is a per-spawn, not a durable, write, so it's
    /// deferred to `boot_on_reserved_host`'s overlapped env/egress leg
    /// (issue #535 (c)) — the FK it needs (`sessions.id`) has been
    /// satisfiable since `reserve_and_persist_create` committed, well
    /// before `prepare_inner` even returns this struct.
    pub agent: Option<AgentSpec>,
    /// The durable session env agentd applies to harness, `/exec`, and
    /// the shell. Used to synthesize the readiness-probe agent when
    /// `agent` is `None`.
    pub session_env: HashMap<String, String>,
    /// ADR 0057: the broker secret-substitution entries the proxy enforces
    /// (placeholder → real value, gated by allow_hosts), precomputed in
    /// `prepare_inner` from the session policy's secrets. Literal secrets are
    /// already folded into `spec_env`/`session_env` and don't appear here.
    pub egress_secrets: Vec<engram_core::types::egress::EgressSecretEntry>,
    /// ADR 0057: the egress network allow-list, sourced from the session
    /// policy (deny-all when the session has no policy). No longer the manifest.
    pub network: engram_core::types::image::NetworkPolicy,
    /// ADR 0055: per-session skills resolved from the profile + assigned to
    /// reserved slots (dyn_0..). Patched into the restored VM load-paused.
    pub selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    /// ADR 0055 / issue #535 (b): the RAW selected skill names (before slot
    /// resolution) — persisted into `sessions.selected_skills` by
    /// `reserve_and_persist_create` so a queued create's boot re-prepare
    /// (`prepare_from_row`) can reconstruct the selection (the
    /// `selected_mounts` above are already-resolved-to-slots and re-derived
    /// fresh on every prepare instead — the sha may have rolled while queued).
    pub selected_skills: Vec<String>,
    /// ADR 0056: the profile-granted capabilities (parsed + validated).
    /// Issue #535 (b): bound to `session_capabilities` by `reserve_and_
    /// persist_create` BEFORE `boot_on_reserved_host` ever runs — this field
    /// is carried on `BootInputs` only because `prepare_inner` is shared by
    /// both the live-request and queued-re-prepare paths, not because the
    /// boot pipeline binds it (it doesn't any more).
    pub capabilities: Vec<engram_core::types::Capability>,
    /// ADR 0056 (B′): the orchestrator-compiled integration policy, if any. Its
    /// inject `secret_ref`s are resolved host-side into the egress policy's
    /// inject entries at boot. `None` on the queued/resume re-prepare for now
    /// (persistence + re-inject is Phase 3b-2).
    pub integration_policy: Option<engram_core::types::IntegrationPolicy>,
    /// ADR 0062: the selected harness name (catalog key). Issue #535 (b):
    /// persisted by `reserve_and_persist_create`, not the boot pipeline.
    /// `None` for a dev-VM session.
    pub selected_harness: Option<String>,
    /// Per-request `secrets` overrides. Issue #535 (b): sealed (KEK, pure
    /// crypto) by `boot_prepared` BEFORE `reserve_and_persist_create`, which
    /// persists the sealed row in the same transaction as everything else —
    /// this field only carries the PLAINTEXT overrides through `prepare_
    /// inner`'s shared shape; nothing downstream of `boot_prepared` reads it
    /// any more.
    pub deferred_session_secrets: Option<HashMap<String, String>>,
    /// An initial prompt to record as a user-role message once Active.
    pub prompt: Option<String>,
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
    /// ADR 0036 amendment (issue #538): the enabled image's OCI manifest
    /// digest, so the reserve-side `ScheduleContext.required_image_digest`
    /// gates placement onto hosts that have actually prefetched this
    /// image's base snapshot — the per-host half of the fleet chunk-
    /// prestage invariant (the enable-scanner's `prestaging` stage is the
    /// other half).
    pub manifest_digest: String,
    /// ADR 0068: the enabled image's base snapshot carries a memory
    /// manifest — this create needs a host reporting a healthy FC UFFD
    /// substrate (`placement::CapabilityRequirements::needs_uffd_substrate`).
    pub needs_uffd_substrate: bool,
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

/// Restore + launch a session on an ALREADY-RESERVED host (the create path
/// reserved it via `reserve_and_persist_create`; the queue scanner via
/// `place_queued_session`). Drives the row `pending → created → active` and
/// emits the matching lifecycle events. See the module docs for the failure
/// contract.
///
/// Issue #535 (b): the row AND every satellite (secrets, capabilities,
/// integration policy, harness, selected skills) are already committed by
/// the time this runs — `reserve_and_persist_create` wrote them all in one
/// transaction before any host RPC. This function does exactly ONE write of
/// its own before `start_agent`: `transition_session_created` (a slim
/// `pending → created` + `sandbox_id` bind), because the sandbox doesn't
/// exist until the restore RPC returns. The FK-ordering bug class this
/// replaces (a satellite write racing the row's own insert) is dead by
/// construction, not by "call it after the row" convention.
pub(crate) async fn boot_on_reserved_host(
    state: &SharedState,
    inputs: BootInputs,
    host_id: HostId,
) -> Result<(), BootError> {
    let BootInputs {
        session_id,
        spec,
        base_snapshot_id,
        base_snapshot: record,
        spec_env,
        mut agent,
        session_env,
        egress_secrets,
        network,
        selected_mounts,
        selected_skills: _,
        capabilities: _,
        integration_policy,
        selected_harness: _,
        deferred_session_secrets: _,
        prompt,
    } = inputs;

    // ADR 0056: the image ref doubles as the SecretContext for resolving the
    // integration policy's inject `secret_ref`s.
    let image_ref = spec.image.clone();

    // Issue #535 (c): overlap the two independent legs of the boot instead
    // of serializing them behind the restore RPC.
    //
    // - Restore leg: `restore_base_on_host` — the ~0.4-0.7s VM-side work.
    // - Env/egress leg: mint the per-spawn forge/upload broker token
    //   (`inject_harness_env`) + resolve the integration policy's Plane-B
    //   injections (`resolve_inject_entries`, which can round-trip an
    //   external provider API for a mint-mode connector). BOTH need only
    //   `session_id` / `image_ref` / `integration_policy` — none of them
    //   touch the sandbox, and the session row has existed (at `pending`)
    //   since `reserve_and_persist_create` committed, well before this
    //   function ever ran, so the broker-token FK has been satisfiable the
    //   whole time. This is where the external mint round trip moves OFF
    //   the serial tail (fully-lazy minting at first proxied use is
    //   explicitly out of scope — this is overlap only).
    //
    // A restore failure discards whatever the env/egress leg produced —
    // cheap, and no different from today's "resolve then maybe fail later"
    // shape.
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
        // The canonical working-set trace is never produced, so the host
        // always falls back to full-manifest memory-chunk prefetch.
        working_set_blob_key: None,
        aux_bundles: record.aux_bundles,
        // Issue #529: restore-side reconstruction, not a fresh capture.
        paused_at: None,
    };
    let restore_leg = state.host_registry.restore_base_on_host(
        host_id,
        metadata,
        spec_env.clone(),
        selected_mounts,
    );
    let env_egress_leg = async {
        if let Some(a) = agent.as_mut() {
            crate::api::sessions::inject_harness_env(state, session_id, &mut a.env).await;
        }
        resolve_inject_entries(state, session_id, integration_policy.as_ref(), &image_ref).await
    };
    // Issue #535 correction: neither `coord_prepare` nor `coord_finalize`
    // covers this join itself — `coord_finalize` only starts once it
    // returns (below) — so a slow env/egress leg (an external mint-mode
    // connector round trip) was invisible to both. Time the whole overlap
    // unconditionally; a restore failure still paid for this wall time
    // before erroring out below.
    let overlap_start = std::time::Instant::now();
    let (restore_result, injects) = tokio::join!(restore_leg, env_egress_leg);
    ::metrics::histogram!(crate::metrics::COORD_BOOT_OVERLAP_SECONDS)
        .record(overlap_start.elapsed().as_secs_f64());
    // Issue #535 (observability): `coord_finalize` starts HERE — restore
    // returned, whatever its outcome. The phase ends at the Active
    // transition below (a failure returns before recording it — this phase
    // measures the successful tail only, matching `coord_prepare`'s
    // success-path framing).
    let finalize_start = std::time::Instant::now();

    let sandbox_id = match restore_result {
        Ok(sb) => sb,
        Err(e) => {
            return Err(BootError::NotStarted(map_restore_error(e)));
        }
    };

    // ---- flip the (already-committed) row to Created + bind sandbox_id ----
    // Issue #535 (c): the row itself, and every satellite, are already
    // committed (by `reserve_and_persist_create`, before this function ever
    // ran) — this is a single slim UPDATE, not an upsert.
    if let Err(e) = state
        .services
        .meta
        .transition_session_created(session_id, sandbox_id)
        .await
    {
        tracing::error!(
            %session_id, %sandbox_id, %host_id, error = %e,
            "session row transition-to-created failed after sandbox create; tearing sandbox down",
        );
        if let Err(de) = state.services.host.destroy(sandbox_id).await {
            tracing::error!(
                %session_id, %sandbox_id, error = %de,
                "sandbox teardown after transition failure also failed — host reconcile will GC",
            );
        }
        return Err(BootError::NotStarted(e.into()));
    }

    // Egress policy from the resolved guest IP (None on backends without
    // one) + the injects/observes already resolved by the overlapped leg.
    let observes = build_observe_entries(integration_policy.as_ref());
    let egress_policy = assemble_egress_policy(
        state,
        session_id,
        sandbox_id,
        egress_secrets,
        &network,
        injects,
        observes,
    )
    .await;

    // ADR 0073: mint the binding generation for this fresh-spawn bind.
    // The epoch fences out any surviving older-generation harness for
    // this session (Superseded at attach) and rides both the durable
    // host record (bind below) and the harness spawn env (spec stamp).
    let binding_epoch = match state.services.meta.mint_binding_epoch(session_id).await {
        Ok(e) => e,
        Err(e) => {
            return Err(BootError::Started(ApiError::Internal(format!(
                "mint binding epoch: {e}"
            ))));
        }
    };
    state
        .services
        .host
        .bind_session(session_id, sandbox_id, binding_epoch)
        .await;

    // ---- start the agent ----
    let mut agent = agent.unwrap_or_else(|| AgentSpec {
        argv: Vec::new(),
        env: HashMap::new(),
        session_env,
        host_ca_pem: None,
        binding_epoch: 0,
    });
    agent.binding_epoch = binding_epoch;
    let policy = egress_policy.unwrap_or_else(|| engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
        network_allow_hosts: network.allow_hosts.clone(),
        network_allow_host_patterns: network.allow_host_patterns.clone(),
        allow_all: false,
        secrets: Vec::new(),
        injects: Vec::new(),
        observes: Vec::new(),
        // ADR 0057: vestigial wire field; substitution is per-entry.
        secret_mode: engram_core::types::image::SecretMode::Broker,
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

    // Issue #535 (observability): `coord_finalize` ends here — restore
    // returned → Active, the coordinator-owned tail after the host handed
    // back a live sandbox.
    ::metrics::histogram!(crate::metrics::SESSION_BOOT_SECONDS, "phase" => "coord_finalize")
        .record(finalize_start.elapsed().as_secs_f64());

    // ADR 0073: the initial prompt is delivered to the harness via the
    // `ENGRAM_INITIAL_PROMPT` env var the create path stamps into the
    // spawn env (see `sessions.rs`), which the in-guest harness consumes
    // at startup — NOT via a synchronous `deliver_prompt`/reattach round
    // trip. (Issue #535 (d) wanted the initial prompt off a bespoke path
    // and onto the same one follow-ups use; ADR 0073's durable model is
    // that path — the env var for the create-time prompt, the outbox for
    // every follow-up — so the fragile synchronous `deliver_prompt` band-
    // aid it introduced is dropped.) Here we only RECORD the user echo in
    // the session event log so the web renders it immediately; a PG hiccup
    // is non-fatal (the harness still runs the prompt from its env).
    if let Some(text) = prompt.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = state
            .emit(
                session_id,
                crate::state::SessionEvent::HarnessAgentMessage {
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

/// Issue #535 (c): the SANDBOX-INDEPENDENT half of what `build_egress_
/// policy` used to compute inline — `injects` (`resolve_inject_entries`,
/// which can round-trip an external mint provider) and `observes` are both
/// resolved by the overlapped env/egress leg in `boot_on_reserved_host`,
/// concurrently with the restore RPC, since neither needs a `sandbox_id`.
/// This function is the remaining sandbox-DEPENDENT half: fetch `guest_ip`
/// and assemble the final policy. `None` when the backend exposes no guest
/// IP (process backend / some VZ configs) — the caller synthesizes an
/// unspecified-IP fallback.
async fn assemble_egress_policy(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    egress_secrets: Vec<engram_core::types::egress::EgressSecretEntry>,
    network: &engram_core::types::image::NetworkPolicy,
    injects: Vec<engram_core::types::egress::EgressInjectEntry>,
    observes: Vec<engram_core::types::egress::EgressObserveEntry>,
) -> Option<engram_core::types::egress::SessionEgressPolicy> {
    let guest_ip = state.services.host.guest_ip(sandbox_id).await?;
    Some(engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip,
        network_allow_hosts: network.allow_hosts.clone(),
        network_allow_host_patterns: network.allow_host_patterns.clone(),
        allow_all: false,
        // ADR 0057: precomputed in `prepare_inner`/resume from the policy secrets
        // (broker entries only; literals are already in the guest env).
        secrets: egress_secrets,
        injects,
        observes,
        // ADR 0057: per-secret mode replaces a session-level mode; the proxy
        // substitutes per `EgressSecretEntry`. Kept Broker for the (vestigial)
        // wire field — substitution is driven by the entries, not this flag.
        secret_mode: engram_core::types::image::SecretMode::Broker,
    })
}

/// ADR 0056 (Phase 4): translate an integration policy's response-observation
/// specs into host-side egress entries. Unlike injections these carry no
/// secret (the asset map operates on the response), so this is a pure copy —
/// no `SecretStore` lookup, hence sync.
pub(crate) fn build_observe_entries(
    integration_policy: Option<&engram_core::types::IntegrationPolicy>,
) -> Vec<engram_core::types::egress::EgressObserveEntry> {
    let Some(policy) = integration_policy else {
        return Vec::new();
    };
    policy
        .observes
        .iter()
        .map(|o| engram_core::types::egress::EgressObserveEntry {
            allow_hosts: o.hosts.clone(),
            allow_host_patterns: Vec::new(),
            methods: o.methods.clone(),
            path_globs: o.path_globs.clone(),
            provider: o.provider.clone(),
            asset_kind: o.asset_kind.clone(),
            surface: o.surface.clone(),
            success_status_class: o.success_status_class.clone(),
            // ADR 0059: GraphQL observe gating/success ride straight through (no
            // secret to resolve — the asset map is pure).
            success_no_graphql_errors: o.success_no_graphql_errors,
            graphql_operation: o.graphql_operation.clone(),
            graphql_field: o.graphql_field.clone(),
            data: o.data.clone(),
            fetchable: o.fetchable.clone(),
        })
        .collect()
}

/// ADR 0056 (B′): resolve an integration policy's Plane-B injections into
/// host-side egress entries. Each inject's `secret_ref` is resolved via the
/// deployment `SecretStore` (the session's image ref is the lookup context);
/// the resolved value rides the policy to the host, never to the orchestrator
/// or guest. A ref that doesn't resolve is skipped + logged (the connector
/// gates the request regardless, but without a credential it would fail
/// upstream — so we drop it rather than inject an empty header).
pub(crate) async fn resolve_inject_entries(
    state: &SharedState,
    session_id: SessionId,
    integration_policy: Option<&engram_core::types::IntegrationPolicy>,
    image: &str,
) -> Vec<engram_core::types::egress::EgressInjectEntry> {
    let Some(policy) = integration_policy else {
        return Vec::new();
    };
    let (repo, image_tag) = {
        let (r, t) = engram_core::types::session::split_image_ref(image);
        (r.to_string(), t.to_string())
    };
    let ctx = engram_core::traits::SecretContext {
        repo: &repo,
        image_tag: &image_tag,
    };
    let schema = engram_core::types::image::SecretSchema::default();
    // ADR 0056 amendment: mint entries scope their token to the session's bound
    // capabilities. Fetch them once, only when a mint entry is actually present.
    let caps = if policy.injects.iter().any(|i| !i.mint_provider.is_empty()) {
        state
            .services
            .meta
            .get_session_capabilities(session_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut out = Vec::with_capacity(policy.injects.len());
    for inj in &policy.injects {
        let entry = if !inj.mint_provider.is_empty() {
            // Minted: the GATING is policy-owned (this entry's hosts/methods/paths),
            // but the HEADER (name + rendered value) is the integration's — so the
            // auth scheme is the provider's, not hardcoded by the policy compiler.
            // The scoped credential never enters the guest.
            match mint_inject_header(state, &inj.mint_provider, &caps).await {
                Some(h) => engram_core::types::egress::EgressInjectEntry {
                    secret: h.value,
                    header_name: h.name,
                    header_template: "{}".to_string(),
                    allow_hosts: inj.hosts.clone(),
                    allow_host_patterns: Vec::new(),
                    methods: inj.methods.clone(),
                    path_globs: inj.path_globs.clone(),
                    // ADR 0059: the same minted token authorizes REST + GraphQL on
                    // the provider's host; the GraphQL matcher rides through.
                    graphql_operation: inj.graphql_operation.clone(),
                    graphql_field: inj.graphql_field.clone(),
                },
                None => continue, // mint_inject_header logged the reason
            }
        } else {
            // Static secret: value from the SecretStore, header from the config.
            let secret = match state
                .services
                .secrets
                .get(&ctx, &inj.secret_ref, &schema)
                .await
            {
                Ok(Some(v)) => v,
                Ok(None) => {
                    tracing::warn!(
                        secret_ref = %inj.secret_ref,
                        "integration inject secret_ref not resolvable; skipping injection",
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        secret_ref = %inj.secret_ref, error = %e,
                        "integration inject secret_ref resolution failed; skipping injection",
                    );
                    continue;
                }
            };
            engram_core::types::egress::EgressInjectEntry {
                secret,
                header_name: inj.header_name.clone(),
                header_template: inj.header_template.clone(),
                allow_hosts: inj.hosts.clone(),
                allow_host_patterns: Vec::new(),
                methods: inj.methods.clone(),
                path_globs: inj.path_globs.clone(),
                // ADR 0059: a static-token connector can also gate GraphQL ops.
                graphql_operation: inj.graphql_operation.clone(),
                graphql_field: inj.graphql_field.clone(),
            }
        };
        out.push(entry);
    }
    out
}

/// ADR 0056 amendment: resolve a *mint* provider's egress inject header — mint a
/// credential scoped to the session's caps, then let the integration render it
/// into a header (`Integration::inject_header`, e.g. github → `Bearer`). The
/// scoped credential never enters the guest. `None` (logged) when the provider
/// isn't resolvable, minting fails, or the credential isn't header-injectable
/// (e.g. AWS SigV4).
async fn mint_inject_header(
    state: &SharedState,
    provider: &str,
    caps: &[engram_core::types::Capability],
) -> Option<engram_core::traits::InjectHeader> {
    let engine = state
        .integrations
        .resolve(provider, &state.services.secrets)
        .await?;
    // Scope the mint to this provider's caps; owner from a cap's `@owner/repo`.
    let scoped: Vec<engram_core::types::Capability> = caps
        .iter()
        .filter(|c| c.provider == provider)
        .cloned()
        .collect();
    let owner = scoped
        .iter()
        .find_map(|c| c.resource.as_deref())
        .map(|r| r.split('/').next().unwrap_or(r).to_string());
    let hint = engram_core::traits::CredentialHint {
        // API injection: the integration's served-host check (e.g. github.com, NOT
        // api.github.com) skips on None — see the ADR 0056 amendment + #404.
        served_host: None,
        owner,
    };
    let cred = match engine.mint_credential(&scoped, &hint).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(provider, error = %e, "mint inject: minting failed; skipping injection");
            return None;
        }
    };
    match engine.inject_header(&cred) {
        Some(h) => Some(h),
        None => {
            tracing::warn!(
                provider,
                "mint inject: credential is not header-injectable (e.g. SigV4); skipping"
            );
            None
        }
    }
}

// Issue #535 (b): `persist_integration_policy` (ADR 0056 B′) retired — the
// compiled policy is now serialized once in `boot_prepared` and persisted by
// `reserve_and_persist_create`'s transaction, alongside the row and every
// other satellite. `MetadataStore::bind_session_integration_policy` stays on
// the trait (still directly exercised by the checkpoint/reconcile PG tests).

// ADR 0057: `egress_secret_entries` (manifest-secret → egress pairing) is
// retired — the per-secret broker entries are built in
// `crate::api::sessions::resolve_policy_secrets` from the session policy.
