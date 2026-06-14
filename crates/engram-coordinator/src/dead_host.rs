//! Dead-host auto-detector.
//!
//! Background task that polls for hosts whose `last_heartbeat_at` is
//! older than the configured threshold and races other coordinator
//! replicas (via Postgres advisory locks) for the right to evacuate
//! each candidate. The winner:
//!
//! 1. Atomically marks the host `Dead` in Postgres and transitions
//!    every non-terminal session pointed at it to `HostLost` with
//!    `host_id` / `sandbox_id` cleared (ADR 0015 M2). `HostLost` is
//!    the explicit "host went away, decide what to do next" state —
//!    M4 (session migration) will turn it into a re-pick on a peer
//!    host; until then we drive the second-stage transition here.
//! 2. For each session: looks up the latest snapshot. If a
//!    recoverable snapshot exists → `HostLost -> Idle` (the user can
//!    /resume from the snapshot); otherwise → `HostLost -> Dead`.
//! 3. Emits per-session `StatusChanged` events for both transitions
//!    using the honest `from` returned by the bulk + the
//!    second-stage transition_session calls.
//! 4. Fires `pg_notify('host_dead', host_id::text)` so other replicas
//!    drop the host from their in-memory `HostRegistry` (handled in
//!    `pg_listener`).
//! 5. Unregisters the host locally.
//!
//! Active execs running on the dead host don't need explicit
//! synthesis: dropping the host's `RemoteSandboxBackend` cascades
//! through the WS demuxer's `Closed` state, the per-exec stream
//! channel drops, and the SSE handler in `api/exec.rs` emits
//! `SessionEvent::ExecCompleted{exit_status: None}` at the natural
//! end of its event loop — same path as a clean exec exit.
//!
//! Without this detector, sessions on a dead host stay `Active`
//! forever (with a `host_id` pointing at a host that won't respond);
//! operators can still `POST /sessions/:id/migrate` by hand.
//!
//! **ADR 0045 Phase A (retire reactive evac):** the second stage
//! routes a recoverable session (snapshot row OR
//! `sessions.live_disk_manifest_*`) to `HostLost → Idle` for lazy
//! `/resume` on next access, and `HostLost → Dead` when there is no
//! recoverable state. The detector no longer routes into
//! `Evacuating` — the reactive auto-evac was the documented bug
//! source (the resume-from-idle wedge, the deploy-storm cascade), so
//! `Evacuating` is now reached *only* via operator drain (ADR 0044
//! K3), and the `evac_resumer` scanner relocates only those.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::HostId;
use sqlx::postgres::PgPool;

use crate::state::{IndexedEvent, SessionEvent, SessionEventBus, SharedState};
use engram_core::SessionId;

/// Persist + publish a `StatusChanged{from, to}` event. Inline here
/// (rather than via `SharedState::emit`) so the dead-host detector
/// can run without a full Services struct — same shape the reconcile
/// pass uses. Errors are logged and swallowed because the persisted
/// state already changed; rolling back the eviction is worse than a
/// missed SSE event.
async fn emit_status_changed(
    meta: &Arc<dyn MetadataStore>,
    events: &Arc<SessionEventBus>,
    session_id: SessionId,
    from: SessionState,
    to: SessionState,
) {
    let event = SessionEvent::StatusChanged {
        from,
        to,
        at: Utc::now(),
    };
    let kind = event.kind();
    let payload = match serde_json::to_value(&event) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "serialize StatusChanged failed");
            return;
        }
    };
    match meta.append_session_event(session_id, kind, payload).await {
        Ok(idx) => {
            events.publish(session_id, IndexedEvent { idx, event });
        }
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "persist StatusChanged failed");
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeadHostConfig {
    /// How often to poll for stale hosts. The dead-host detection
    /// latency is `poll_interval + threshold` worst-case; with
    /// defaults that's 30s + 10s = 40s, just inside the
    /// DESIGN.md:700 deliverable target of 30s migration on
    /// `kill -9`. Tighten for stricter targets.
    pub poll_interval: Duration,
    /// A host is considered dead when its `last_heartbeat_at` is
    /// older than this. Default 30s (~6× the 5s heartbeat cadence)
    /// — comfortable margin for transient network blips, fast
    /// enough to catch real failures.
    pub stale_threshold: Duration,
}

impl Default for DeadHostConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            stale_threshold: Duration::from_secs(30),
        }
    }
}

/// Spawn the detector as a background task. Returns a JoinHandle the
/// caller can drop on shutdown. Runs forever; logs and continues on
/// per-tick errors so a transient Postgres blip doesn't stop the loop.
pub fn spawn(cfg: DeadHostConfig, pool: PgPool, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — the coordinator just
        // started and no host has had time to be considered stale.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &pool, &state).await {
                tracing::warn!(error = %e, "dead-host detector tick failed; will retry");
            }
        }
    })
}

async fn run_once(
    cfg: &DeadHostConfig,
    pool: &PgPool,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state
        .services
        .meta
        .list_stale_hosts(cfg.stale_threshold.as_secs())
        .await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "dead-host detector found stale candidates"
    );
    for host in candidates {
        let host_addr = host.host_addr.clone();
        if let Err(e) = evict_host(pool, state, host.id, host_addr).await {
            tracing::warn!(host_id = %host.id, error = %e, "evict failed; another replica may have it");
        }
    }
    Ok(())
}

/// Defense-in-depth liveness probe for the dead-host detector (issue
/// #231). The detector keys on `last_heartbeat_at`, which the heartbeat
/// handler advances best-effort-no-more. But the failure mode that
/// orphans a *healthy* host is cross-pod: pod A's pool saturates and
/// stops persisting host H's `last_heartbeat_at` (H now also gets 5xx
/// and backs off, but the row is already stale), while pod B's detector
/// — healthy PG — sees the stale row and is about to mark H dead and
/// orphan its sessions. Before doing that, B dials H directly: if H
/// answers a `Ping`, the row is stale but the host is alive, so we skip
/// the eviction and warn. Converts silent data loss into an alert.
///
/// Returns `true` only when the host *answered* the probe. An
/// unreachable host (the genuine dead-host case), or no client to
/// probe with, returns `false` so eviction proceeds as before.
async fn host_responds(client: &Arc<dyn engram_core::traits::HostClient>) -> bool {
    client.ping().await.is_ok()
}

/// The dead-host detector's stage-2 routing decision (ADR 0045 Phase
/// A). A session is recoverable — and routed to `Idle` for lazy
/// `/resume` on next access — if it has a memory snapshot OR a live
/// disk manifest (the latter still resumes via the cold-boot path).
/// With nothing to recover from, it goes to `Dead`. The detector no
/// longer routes into `Evacuating`; proactive relocation is operator
/// drain only (ADR 0044 K3).
fn recovery_target(has_snapshot: bool, has_live_manifest: bool) -> SessionState {
    if has_snapshot || has_live_manifest {
        SessionState::Idle
    } else {
        SessionState::Dead
    }
}

async fn evict_host(
    pool: &PgPool,
    state: &SharedState,
    host_id: HostId,
    host_addr: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let host_registry = &state.host_registry;
    let events = &state.events;
    // Pin a single connection so the advisory lock stays with us for
    // the duration of the eviction. `pg_try_advisory_lock` is a
    // session-scoped lock and auto-releases when the connection
    // closes — so even if we panic mid-eviction, the lock doesn't
    // strand the host.
    let mut conn = pool.acquire().await?;
    let lock_key: String = format!("dead-host:{host_id}");

    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&lock_key)
        .fetch_one(&mut *conn)
        .await?;
    if !got {
        // Another coordinator replica won the race for this host.
        // It will do the eviction; we just skip.
        tracing::debug!(host_id = %host_id, "advisory lock contested; skipping");
        return Ok(());
    }

    // Re-check the host's status *after* taking the lock — another
    // replica that already won may have flipped it to Dead in the
    // window between our `list_stale_hosts` and now.
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM hosts WHERE id = $1")
        .bind(host_id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if matches!(status.as_deref(), Some("dead") | None) {
        tracing::debug!(host_id = %host_id, "host already dead; releasing lock");
        sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(&lock_key)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    }

    // Defense-in-depth (issue #231): the row says stale, but is the host
    // actually gone? Dial it directly with a cheap `Ping` before we
    // orphan its sessions. The asymmetric-PG-failure mode — one coord
    // pod's pool saturates and stops advancing H's `last_heartbeat_at`
    // while THIS pod's detector is healthy — staled a *live* host's row;
    // marking it dead here would orphan every session on a host that's
    // up and loaded. If the host answers, skip the eviction and warn so
    // an operator catches the heartbeat-persistence fault instead of a
    // fleet section flapping mid-run.
    //
    // We probe via the in-memory pool entry when present, otherwise we
    // warm a fresh dial from the candidate's persisted `host_addr` (the
    // cross-pod case: this pod never saw H register, so its registry is
    // empty for H). No `host_addr` (pre-0013 row) ⇒ unprobeable ⇒ fall
    // through to eviction, exactly as before this guard existed.
    let probe_client: Option<Arc<dyn engram_core::traits::HostClient>> = match state
        .services
        .host_pool
        .get(host_id)
    {
        Ok(c) => Some(Arc::new(c)),
        Err(_) => match host_addr {
            Some(addr) => match state.services.host_pool.get_or_warm(host_id, addr).await {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    tracing::debug!(host_id = %host_id, error = %e, "dead-host probe: could not warm a dial; treating as unreachable");
                    None
                }
            },
            None => None,
        },
    };
    if let Some(client) = probe_client {
        if host_responds(&client).await {
            tracing::warn!(
                host_id = %host_id,
                "stale row but live host — host answered Ping while last_heartbeat_at is stale; SKIPPING eviction. Check heartbeat persistence (coord PG pool saturation?) — see engram_heartbeat_persist_failures_total (issue #231)",
            );
            sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
                .bind(&lock_key)
                .execute(&mut *conn)
                .await?;
            return Ok(());
        }
    }

    let affected = meta.mark_host_dead_and_orphan_sessions(host_id).await?;

    // Notify other replicas so they drop their HostRegistry entry.
    sqlx::query("SELECT pg_notify('host_dead', $1)")
        .bind(host_id.to_string())
        .execute(&mut *conn)
        .await?;

    // Stage 1: emit StatusChanged{prev -> HostLost} for every session
    // the bulk touched. The `prev` came back from the UPDATE so the
    // `from` is honest (not a hand-encoded `Active` that would lie if
    // the session had been Idle).
    for (session_id, prev) in &affected {
        emit_status_changed(meta, events, *session_id, *prev, SessionState::HostLost).await;
    }

    // Stage 2: per-session recoverability-aware second transition.
    //
    // ADR 0045 Phase A: route a recoverable session to `Idle` for
    // lazy `/resume` on next access; `Dead` when there's nothing to
    // recover. The detector no longer routes into `Evacuating` — the
    // reactive auto-evac is retired (it was the source of the
    // resume-from-idle wedge + deploy-storm cascade). Proactive
    // relocation now happens only via operator drain (ADR 0044 K3).
    //
    // Decision matrix:
    //
    // | snapshot | live_manifest | next state | who recovers it          |
    // |----------|---------------|------------|--------------------------|
    // | Some     | _             | Idle       | user/exec /resume        |
    // | None     | Some          | Idle       | /resume (disk-only cold) |
    // | None     | None          | Dead       | (no recoverable state)   |
    //
    // Failures of any query/transition are logged and skipped; the
    // row stays at HostLost and a future reconcile pass (or
    // operator action) can move it on.
    for (session_id, _) in &affected {
        let snapshot = match meta.latest_snapshot_for_session(*session_id).await {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %session_id,
                    "snapshot lookup failed; leaving session at HostLost"
                );
                continue;
            }
        };

        // Look up the session row for live_disk_manifest so a disk-only
        // session (no memory snapshot) stays recoverable via the
        // cold-boot `/resume` path. Failure leaves the row at HostLost.
        let has_live_manifest = match meta.get_session(*session_id).await {
            Ok(s) => s.live_disk_manifest.is_some(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %session_id,
                    "get_session failed during second-stage; leaving at HostLost",
                );
                continue;
            }
        };

        let target = recovery_target(snapshot.is_some(), has_live_manifest);

        match meta.transition_session(*session_id, target).await {
            Ok(prev) => {
                emit_status_changed(meta, events, *session_id, prev, target).await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %session_id,
                    ?target,
                    "HostLost second-stage transition failed; leaving session at HostLost"
                );
            }
        }
    }

    host_registry.unregister(host_id);
    tracing::info!(
        host_id = %host_id,
        sessions_orphaned = affected.len(),
        "host marked dead; sessions moved through HostLost to Idle/Dead",
    );

    sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&lock_key)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The detector's polling loop and advisory-lock dance are
    // Postgres-specific and require a live database to test
    // meaningfully. The trait-layer logic
    // (`mark_host_dead_and_orphan_sessions` semantics) is covered
    // by Mock-based tests in `tests/dead_host_mock.rs`. End-to-end
    // multi-replica behaviour is the live-Postgres test
    // (`#[ignore]`'d, gated behind dev-VM Docker compose).

    // ADR 0045 Phase A: the stage-2 routing decision. A recoverable
    // dead-host session goes to Idle (lazy /resume), never Evacuating
    // (the reactive auto-evac is retired); only the no-state case is
    // terminal.
    #[test]
    fn recovery_target_routes_recoverable_to_idle_never_evacuating() {
        // snapshot present → Idle (memory + disk resume).
        assert_eq!(recovery_target(true, false), SessionState::Idle);
        // disk-only (live manifest, no snapshot) → Idle (cold-boot resume).
        assert_eq!(recovery_target(false, true), SessionState::Idle);
        // both present → Idle.
        assert_eq!(recovery_target(true, true), SessionState::Idle);
        // nothing recoverable → Dead.
        assert_eq!(recovery_target(false, false), SessionState::Dead);

        // The reactive auto-evac target is gone: no input combination
        // routes a dead host's session into Evacuating.
        for (snap, manifest) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_ne!(recovery_target(snap, manifest), SessionState::Evacuating);
        }
    }

    // Issue #231: defense-in-depth probe. The detector keys on the
    // stale `last_heartbeat_at` row, but before orphaning a host's
    // sessions it dials the host directly. A host that ANSWERS the
    // probe is alive (the row went stale because some coord pod's PG
    // pool saturated and stopped advancing it) → eviction must be
    // skipped. A host that does NOT answer is the genuine dead-host
    // case → eviction proceeds. `host_responds` is the seam that
    // decides which; these tests pin both arms.
    use engram_core::traits::HostClient;
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::{SandboxError, SandboxId, SessionId};

    /// Minimal `HostClient` whose `list()` (and thus the default
    /// `ping()`) returns alive/unreachable on demand. Every other
    /// method is unreachable in this test — the probe only calls
    /// `ping()`.
    struct ProbeHost {
        alive: bool,
    }

    #[async_trait::async_trait]
    impl HostClient for ProbeHost {
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            if self.alive {
                Ok(Vec::new())
            } else {
                Err(SandboxError::Unavailable("host down".into()))
            }
        }
        async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            unimplemented!()
        }
        async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn exec_stream(
            &self,
            _id: SandboxId,
            _cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            unimplemented!()
        }
        async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            unimplemented!()
        }
        async fn restore(&self, _metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            unimplemented!()
        }
        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: AgentSpec,
            _policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn apply_egress_policy(
            &self,
            _policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
            unimplemented!()
        }
        async fn bind_session(&self, _session_id: SessionId, _sandbox_id: SandboxId) {
            unimplemented!()
        }
        async fn unbind_session(&self, _session_id: SessionId) {
            unimplemented!()
        }
        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _text: String,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn acquire_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn release_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn host_responds_skips_eviction_for_a_live_but_stale_host() {
        // The host answers the Ping → it's alive; the stale row is a
        // heartbeat-persistence artifact, NOT a dead host. evict_host
        // uses this to skip the orphaning.
        let live: Arc<dyn HostClient> = Arc::new(ProbeHost { alive: true });
        assert!(
            host_responds(&live).await,
            "a host that answers the probe must be treated as live (skip eviction)",
        );
    }

    #[tokio::test]
    async fn host_does_not_respond_when_unreachable() {
        // The genuine dead-host case: the probe fails → eviction
        // proceeds as before.
        let dead: Arc<dyn HostClient> = Arc::new(ProbeHost { alive: false });
        assert!(
            !host_responds(&dead).await,
            "an unreachable host must not be treated as live (eviction proceeds)",
        );
    }
}
