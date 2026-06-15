//! ADR 0018 commit 12c — Evacuating-session resumer.
//!
//! Background task that turns `Evacuating` sessions back into `Active`
//! sessions on a peer host. Sibling to [`crate::dead_host`]: same
//! polling shape, same shared-state surface, distinct entry point on
//! the state machine.
//!
//! ## Flow
//!
//! 1. Tick: read `sessions WHERE status = 'evacuating'` (with
//!    `evac_attempts`) via
//!    [`MetadataStore::list_evacuating_sessions`].
//! 2. For each candidate, if `evac_attempts >= max_attempts` →
//!    `Evacuating → Idle` and stop trying (user can `/resume`).
//! 3. Otherwise bump `evac_attempts` atomically, then run the
//!    relocation pipeline:
//!    - [`crate::evacuation::evacuate_dead_source`] picks a peer host,
//!      restores from the session's `live_disk_manifest` and/or latest
//!      snapshot, rebinds PG `(host_id, sandbox_id)`, transitions
//!      `Evacuating → Created`.
//!    - [`crate::api::snapshot::bind_session_routing`] registers the
//!      session→sandbox map on the target host-agent (the coordinator
//!      keeps no in-memory binding — `sessions.sandbox_id` is the
//!      authority, ADR 0047).
//!    - [`crate::api::snapshot::finish_resume_to_active`] runs the
//!      harness rebuild + drives `Created → Active`.
//! 4. On any error in the pipeline, the session is left at its current
//!    state — Evacuating (retry next tick) or Created (a later scanner
//!    tick re-picks it up via the operator-/exec-driven `/resume`
//!    path). The pre-bump idempotency lives in
//!    `evacuate_dead_source` (PG rebind is `assign_*` which tolerates
//!    re-runs) and in `bind_session_routing` (an idempotent host RPC).
//!
//! ## Why this pattern
//!
//! Per `[async_via_state_machine]` — drain is a multi-host, multi-step
//! operation. Synchronous orchestration would couple the source
//! handler to a known target and conflate retry domains; the
//! state-machine+scanner shape decouples them. Source writes
//! "session is ready to be continued"; scanner finds a healthy peer.
//! Operator-initiated drain (ADR 0044 K3) is the sole producer of
//! `Evacuating` — ADR 0045 Phase A retired the reactive dead-host /
//! NBD-loss producers (the dead-host detector now routes recoverable
//! sessions to `Idle` for lazy `/resume`).
//!
//! The scanner is single-coord-pod safe because each per-session
//! advance starts with `bump_evac_attempts` (atomic +1) followed by
//! `evacuate_dead_source`'s pick + restore + rebind. Two coord pods
//! racing on the same session would both observe `Evacuating`, both
//! attempt restore, and the second's `transition_session(Created)`
//! would see the row already at `Created` and surface `Conflict`.
//! That's a harmless duplicate sandbox on the target (cleaned up by
//! orphan reap) — same shape as the dead-host detector's existing
//! advisory-lock race. Tightening with an advisory lock per session
//! is a follow-up if duplicate-restore counts ever rise above zero.

use std::time::Duration;

use chrono::Utc;
use engram_core::types::{Session, SessionState};

use crate::api::snapshot::{bind_session_routing, finish_resume_to_active, FinishResumeOutcome};
use crate::evacuation::{evacuate_dead_source, resolve_cold_boot_spec, EvacError};
use crate::state::{SessionEvent, SharedState};

#[derive(Clone, Debug)]
pub struct EvacResumerConfig {
    /// How often to sweep for Evacuating sessions. The scanner picks
    /// up new entries from operator drains (ADR 0044 K3) — the sole
    /// producer of `Evacuating` since ADR 0045 Phase A retired the
    /// reactive triggers. Default 10s matches
    /// `DeadHostConfig::poll_interval` so the two scanners share the
    /// same operational cadence.
    pub poll_interval: Duration,
    /// Retry budget per session before falling back to `Idle`. At the
    /// default 10s cadence, 20 attempts is ~3 minutes — long enough
    /// to ride out a transient capacity / image-prefetch shortfall on
    /// peer hosts during a rolling restart, short enough that a truly
    /// stuck session surfaces to the user as `Idle` (manual /resume)
    /// before they assume it's gone.
    pub max_attempts: u32,
}

impl Default for EvacResumerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            max_attempts: 20,
        }
    }
}

/// Spawn the resumer as a background task. Caller holds the JoinHandle
/// for the process lifetime; dropping aborts the loop. Mirrors
/// [`crate::dead_host::spawn`].
pub fn spawn(cfg: EvacResumerConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the first immediate tick — coord just started, give
        // hosts a beat to heartbeat in before we pick.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "evac-resumer tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so live-PG tests can drive the
/// scanner deterministically without `tokio::spawn`-ing the loop.
/// Production code uses [`spawn`] which calls this on a timer.
pub(crate) async fn run_once(
    cfg: &EvacResumerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state.services.meta.list_evacuating_sessions().await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "evac-resumer found Evacuating sessions"
    );
    for (session, attempts) in candidates {
        if let Err(e) = advance_one(cfg, state, session, attempts).await {
            // Keep going — one wedged session shouldn't stall the
            // sweep. The per-session log already carries `error =
            // %e`; this is the loop-level swallow.
            tracing::warn!(error = %e, "evac-resumer per-session advance failed");
        }
    }
    Ok(())
}

async fn advance_one(
    cfg: &EvacResumerConfig,
    state: &SharedState,
    session: Session,
    attempts: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    // ADR 0045 C1 (the named follow-up in the module docs): take the
    // session lease before driving a resume — a live migration's
    // synchronous verb (or a peer pod's pipeline) may be mid-flight on
    // this session; without the lease two actors can double-restore.
    // Skip-if-held: the holder owns the session; we re-scan next tick.
    let Some(_lease) = crate::idle_evictor::SessionLeaseGuard::try_acquire(state, session_id, None)
        .await
        .map_err(|e| format!("evac-resumer lease acquire: {e}"))?
    else {
        tracing::debug!(%session_id, "evac-resumer: session lease held; skipping this tick");
        return Ok(());
    };
    // Retry budget exhausted → fall back to Idle so the user can
    // `/resume` manually. Idle is a legal target from Evacuating per
    // the legality table; the row's snapshot lineage is already
    // durable (it was captured before the pipeline marked the
    // session Evacuating in `evict_session_to_state`), so /resume
    // from Idle restores cleanly.
    if attempts >= cfg.max_attempts {
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::Idle)
            .await
        {
            Ok(prev) => {
                tracing::warn!(
                    %session_id,
                    attempts,
                    max_attempts = cfg.max_attempts,
                    "evac-resumer budget exhausted; session left at Idle for user /resume",
                );
                let _ = state
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Idle,
                            at: Utc::now(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "evac-resumer fallback transition Evacuating→Idle failed",
                );
            }
        }
        // Gave up relocating — drop any teleport pin so a later manual
        // /resume isn't constrained to the (evidently unavailable) target.
        let _ = state
            .services
            .meta
            .set_teleport_target(session_id, None)
            .await;
        return Ok(());
    }

    // Bump pre-pipeline. A pipeline failure leaves the counter
    // incremented and the session at Evacuating — next tick retries
    // until the budget runs out. Bumping post-success isn't needed
    // because `transition_session(Evacuating)` resets the counter on
    // every entry per migration 0037's CASE expression.
    let new_attempts = state.services.meta.bump_evac_attempts(session_id).await?;
    tracing::info!(
        %session_id,
        attempt = new_attempts,
        max_attempts = cfg.max_attempts,
        "evac-resumer: starting resume attempt",
    );

    run_resume_pipeline(state, session).await?;
    Ok(())
}

async fn run_resume_pipeline(
    state: &SharedState,
    session: Session,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    let snapshot = state
        .services
        .meta
        .latest_snapshot_for_session(session_id)
        .await?;

    // ADR 0028 A.log: capture the rung-1 rewind cursor before the
    // snapshot moves into evacuate_dead_source. Only a coherent
    // checkpoint (memory present) rewinds; the receipt's
    // `EvacLoss::None` confirms rung-1 actually happened.
    let rewind_cursor = snapshot
        .as_ref()
        .filter(|s| s.memory_manifest.is_some())
        .and_then(|s| s.events_cursor);

    // ADR 0028 Fix B: pre-resolve the disk-only cold-boot spec. Only
    // consulted when no coherent memory snapshot is usable; a `None`
    // there fails structurally rather than burning the budget.
    let cold_boot_spec = resolve_cold_boot_spec(&state.services.meta, &session).await;

    // ADR 0045 Phase F: an operator-pinned teleport destination, if any.
    // Honored strictly (a bad pin retries then falls back to Idle, never
    // silently lands elsewhere); cleared below once the session resolves.
    let require_host = match state.services.meta.get_teleport_target(session_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%session_id, error = %e,
                "get_teleport_target failed; treating as unpinned");
            None
        }
    };

    let receipt = match evacuate_dead_source(
        &state.host_registry,
        &state.services.meta,
        session.clone(),
        snapshot,
        cold_boot_spec,
        require_host,
        // No origin preference: every scanner producer (drain, dead
        // host, migration parachute) is moving AWAY from the source.
        None,
    )
    .await
    {
        Ok(r) => r,
        // ADR 0028 Fix B fail-fast: structural errors can never be
        // fixed by retrying — the pre-Fix-B behavior of letting the
        // budget loop burn 20 attempts (~3 min of RestoreFailed churn
        // in the cf4d4afd incident) just delayed the honest terminal
        // state. NoRecoverableState (nothing to restore from) → Dead;
        // ColdBootUnavailable (disk exists, image gone) → Idle, so
        // re-enabling the image + /resume can still recover the disk.
        Err(e) if e.is_structural() => {
            let target = match &e {
                EvacError::NoRecoverableState => SessionState::Dead,
                _ => SessionState::Idle,
            };
            tracing::warn!(
                %session_id,
                error = %e,
                target = %target.as_str(),
                "evac-resumer: structural failure — failing fast instead of burning budget",
            );
            match state
                .services
                .meta
                .transition_session(session_id, target)
                .await
            {
                Ok(prev) => {
                    let _ = state
                        .emit(
                            session_id,
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: target,
                                at: Utc::now(),
                            },
                        )
                        .await;
                }
                Err(te) => {
                    tracing::warn!(
                        %session_id,
                        error = %te,
                        "evac-resumer: structural fail-fast transition failed",
                    );
                }
            }
            // Session left Evacuating terminally — drop any teleport pin.
            let _ = state
                .services
                .meta
                .set_teleport_target(session_id, None)
                .await;
            return Ok(());
        }
        Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    };

    // Resolved onto a peer (Created) — the teleport pin is consumed.
    let _ = state
        .services
        .meta
        .set_teleport_target(session_id, None)
        .await;

    tracing::info!(
        %session_id,
        new_host = %receipt.new_host_id,
        new_sandbox = %receipt.new_sandbox_id,
        loss = receipt.loss.as_str(),
        "evac-resumer: rebound to peer at Created — finishing harness rebuild",
    );

    // StatusChanged{prev → Created} so SSE subscribers see the move.
    // `evacuate_dead_source` already committed the transition to
    // Created in PG, so we emit synthesized event with from=Evacuating.
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Evacuating,
                to: SessionState::Created,
                at: Utc::now(),
            },
        )
        .await;

    bind_session_routing(state, session_id, receipt.new_sandbox_id).await;

    // ADR 0028 A.log: warm rung-1 recovery — rewind the transcript to
    // the checkpoint's cursor + emit the recovery boundary. Gated on
    // EvacLoss::None (memory was actually restored); a rung-2 cold
    // boot carries no cursor and skips this. No-op if the checkpoint
    // was the head.
    // ADR 0045 F1: this path is only reached via operator drain /
    // teleport (Phase A retired the reactive dead-host producer), so the
    // rewind is a planned relocation, not a host failure.
    if receipt.loss == engram_core::types::evacuation::EvacLoss::None {
        crate::api::snapshot::apply_rung1_rewind(
            state,
            session_id,
            rewind_cursor,
            crate::state::RecoveryCause::PlannedRelocation,
        )
        .await;
    }

    // Refresh the session row so finish_resume_to_active sees the
    // freshly-bound host_id + sandbox_id.
    let session_refreshed = state.services.meta.get_session(session_id).await?;
    match finish_resume_to_active(state, &session_refreshed, receipt.new_sandbox_id, true).await {
        Ok(FinishResumeOutcome::Active) => {
            tracing::info!(
                %session_id,
                "evac-resumer: session reached Active on peer host",
            );
        }
        Ok(FinishResumeOutcome::CreatedHarnessFailed) => {
            tracing::warn!(
                %session_id,
                "evac-resumer: harness rebuild failed on peer; session left at Created — \
                 user /resume retries from there. Scanner won't re-pick (status != Evacuating).",
            );
        }
        Err(e) => {
            tracing::warn!(
                %session_id,
                error = %e,
                "evac-resumer: finish_resume_to_active errored; session left at Created",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // The scanner's full loop exercises Postgres + HostRegistry +
    // finish_resume_to_active; the per-step plumbing is unit-tested
    // via the existing evacuation / api::snapshot tests. End-to-end
    // coverage lives in:
    //   - `admin_evac_live_pg` (commit 12i): operator-drain triggers
    //     Evacuating, scanner picks it up, session reaches Active on
    //     peer.
    //   - `evac_resumer_budget_falls_back_to_idle` (commit 12i):
    //     simulate 20 failed bumps, observe Evacuating → Idle
    //     fallback.
    //   - dev-vm integration-evac-test.sh: real two-host drain.
    //
    // ADR 0028 Fix B adds the structural fail-fast tests below: a
    // structurally-unrecoverable session must reach its terminal
    // state on the FIRST attempt, not after burning the 20-attempt
    // budget (~3 min of churn in the cf4d4afd incident).

    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::MetadataStore;
    use engram_core::types::session::SessionMode;
    use std::sync::Arc;

    fn evacuating_session(live_disk: Option<engram_core::types::manifest::ManifestRef>) -> Session {
        Session {
            user_id: None,
            id: engram_core::SessionId::new(),
            status: SessionState::Evacuating,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evac-test".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: live_disk,
        }
    }

    fn build_state(session: Session) -> (SharedState, Arc<MiniMeta>) {
        let tmp = std::env::temp_dir().join(format!("evac-resumer-test-{}", session.id));
        std::fs::create_dir_all(&tmp).unwrap();
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                tmp.join("blobs"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.join("blobs")),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: tmp,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
        )
    }

    /// ADR 0045 C1: a held session lease (a live migration's
    /// synchronous verb, a peer pod's pipeline) makes the scanner SKIP
    /// the session this tick — no transition, no restore attempt, no
    /// double-driving.
    #[tokio::test]
    async fn advance_one_skips_when_session_lease_held() {
        let session = evacuating_session(None);
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());
        assert!(meta
            .try_acquire_session_lease(session_id, None, "rival-pod")
            .await
            .unwrap());

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("skip is not an error");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Evacuating,
            "lease-held session must be left untouched for the holder",
        );
    }

    /// No snapshot + no live disk manifest → NoRecoverableState →
    /// Dead on attempt 1.
    #[tokio::test]
    async fn structural_no_state_fails_fast_to_dead() {
        let session = evacuating_session(None);
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("advance_one swallows structural failures");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Dead,
            "structurally unrecoverable session must fail fast to Dead, not retry",
        );
    }

    /// Live disk manifest present but the image is not enabled (no
    /// cold-boot spec derivable) → ColdBootUnavailable → Idle on
    /// attempt 1, preserving the re-enable-then-/resume path.
    #[tokio::test]
    async fn structural_disk_only_without_image_fails_fast_to_idle() {
        let live = engram_core::types::manifest::ManifestRef {
            manifest_id: uuid::Uuid::from_u128(0xD15C),
            version: 4,
        };
        let session = evacuating_session(Some(live));
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("advance_one swallows structural failures");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Idle,
            "disk-only session with no enabled image must land at Idle \
             (re-enable image + /resume recovers), not burn the budget",
        );
    }
}
