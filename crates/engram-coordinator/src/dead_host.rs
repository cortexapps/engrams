//! Dead-host auto-detector.
//!
//! Background task that polls for hosts whose `last_heartbeat_at` is
//! older than the configured threshold and races other coordinator
//! replicas — via a PG leasing row (`dead_host_inflight`,
//! ADR 0098 D4; the repo convention: leasing row over advisory lock)
//! — for the right to evacuate each candidate. The winner:
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
//! 4. Broadcasts `host_dead` via `MetadataStore::notify_host_dead`
//!    (Postgres: `pg_notify`) so other replicas drop the host from
//!    their in-memory `HostRegistry` (handled in `pg_listener`).
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
//! **Probe strikes + rescue grace (2026-07-09 rk28 incident):** a stale
//! row alone never kills a host — the issue-#231 liveness probe must
//! ALSO fail `min_probe_failures` consecutive ticks, and any answered
//! probe (a "rescue") arms a `probe_rescue_grace` window during which
//! failures defer instead of evict. A single failed Ping in the middle
//! of a post-roll http2 flap used to orphan live sessions twenty
//! seconds after the same detector had proven the host alive; a
//! genuinely dead host just pays `(min_probe_failures − 1)` extra
//! ticks (~20 s at defaults).
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

use chrono::{DateTime, Utc};
use engram_core::traits::{MetadataStore, SessionFence};
use engram_core::types::SessionState;
use engram_core::{HostId, MetaError};

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
    now: DateTime<Utc>,
) {
    let event = SessionEvent::StatusChanged { from, to, at: now };
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
            events.publish(
                session_id,
                IndexedEvent {
                    idx,
                    event,
                    ephemeral: false,
                },
            );
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
    /// Consecutive FAILED liveness probes (one per detector tick)
    /// required before a stale-row host is actually marked dead.
    /// A genuinely dead host fails every probe, so this only adds
    /// `(min_probe_failures - 1) × poll_interval` (~20s at defaults)
    /// to real detection; a host mid-flap gets the strikes forgiven
    /// the moment one probe lands. Prod incident 2026-07-09 (rk28):
    /// a single failed Ping during a ~2-minute post-roll http2 flap
    /// orphaned two live sessions — twenty seconds after the SAME
    /// detector's probe had rescued the host.
    pub min_probe_failures: u32,
    /// A host that ANSWERED a probe (a "rescue") this recently cannot
    /// be marked dead by a subsequent probe failure — a host proven
    /// alive seconds ago is overwhelmingly mid-flap, not dead. The
    /// grace is measured from the last rescue, so a truly-dead host
    /// still gets evicted once it expires (with `min_probe_failures`
    /// long since accumulated).
    pub probe_rescue_grace: Duration,
    /// An eviction lease older than this is presumed abandoned (the
    /// claiming pod crashed mid-eviction) and may be taken over by any
    /// replica. Comfortably larger than a full eviction pass; small
    /// enough that a crashed pod delays a genuinely-dead host's
    /// eviction by at most this long.
    pub lease_stale_after: Duration,
    /// Issue #777 "ask-the-host": how many consecutive sweep cycles the
    /// straggler sweep will DEFER destroying a still-bound sandbox whose
    /// host still reports it serving (a live VM under a HostLost row —
    /// the >60s partition/desync window) before giving up and
    /// destroying+settling anyway. The cap keeps the sweep convergent
    /// (never parks forever, the #762/#769 wedge) while giving the
    /// reattach machinery a bounded window to recover the live VM in
    /// place. Default 3 (~3 sweep cycles).
    pub straggler_serving_strike_cap: u32,
}

impl Default for DeadHostConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            stale_threshold: Duration::from_secs(30),
            min_probe_failures: 3,
            probe_rescue_grace: Duration::from_secs(120),
            lease_stale_after: Duration::from_secs(180),
            straggler_serving_strike_cap: 3,
        }
    }
}

/// Per-host probe history the detector keeps in memory. Per-replica
/// (deliberately not persisted): with two replicas racing the eviction
/// lease, each counts its own strikes, so eviction can take up to 2× the
/// strike window — a bounded, conservative error in the safe direction
/// (never evicts EARLIER than a single replica would).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeMemory {
    /// Consecutive failed probes, one per detector tick. Reset by any
    /// answered probe.
    consecutive_failures: u32,
    /// When this host last answered a probe while its row was stale, as
    /// a `Clock::now_mono()` mark (ADR 0098 D1: monotonic marks are
    /// stored as `Duration`, not opaque `Instant`s).
    last_rescue: Option<Duration>,
}

/// Pure verdict for the probe-failure path: is this failure enough
/// evidence to orphan the host's sessions? `mem` has already been
/// updated with the current failure. Two gates, both must pass:
/// enough consecutive strikes, and no recent rescue (a host that
/// answered a probe `probe_rescue_grace` ago is mid-flap until proven
/// otherwise for the grace duration).
fn probe_failure_permits_eviction(
    mem: &ProbeMemory,
    now: Duration,
    min_probe_failures: u32,
    probe_rescue_grace: Duration,
) -> bool {
    if mem.consecutive_failures < min_probe_failures {
        return false;
    }
    match mem.last_rescue {
        Some(rescued_at) => now.saturating_sub(rescued_at) >= probe_rescue_grace,
        None => true,
    }
}

/// Spawn the detector as a background task. Returns a JoinHandle the
/// caller can drop on shutdown. Runs forever; logs and continues on
/// per-tick errors so a transient Postgres blip doesn't stop the loop.
/// Per-host probe history, keyed by host id. Owned by the caller of
/// [`run_once`] so strikes persist across sweeps.
pub type ProbeMemoryMap = std::collections::HashMap<HostId, ProbeMemory>;

/// Per-session serving-strike history for the straggler sweep (issue
/// #777 "ask-the-host"), keyed by session id. Counts consecutive sweep
/// cycles on which the host still reported a still-bound sandbox as
/// serving, so the sweep can defer the destroy up to
/// `straggler_serving_strike_cap` cycles before giving up. Owned by the
/// caller of [`run_once`] so it persists across sweeps and pruned to the
/// current HostLost set each cycle.
///
/// Per-replica and deliberately in-memory (the same choice as
/// [`ProbeMemory`]): the running-sandbox SET is not persisted in PG, so
/// there is no natural column to mirror; and this is a per-pod backstop,
/// not cross-pod truth. With replicas racing, each counts its own strikes
/// — a bounded, conservative error in the safe direction (it can only
/// DELAY a destroy, never destroy a live VM earlier than a single replica
/// would), and a settle by ANY replica ends the deferral for all via the
/// #211 CAS + `Conflict`-idempotent transition.
pub type StragglerStrikeMap = std::collections::BTreeMap<SessionId, u32>;

pub fn spawn(cfg: DeadHostConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Same claimant identity convention as the enable scanner: the
        // pod hostname, falling back for local/dev runs.
        let claimant = std::env::var("HOSTNAME").unwrap_or_else(|_| "coord".into());
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — the coordinator just
        // started and no host has had time to be considered stale.
        tick.tick().await;
        // Probe history across ticks (strikes + rescue grace); pruned
        // to the current candidate set each sweep, so a host whose
        // heartbeats recover starts its next staleness episode fresh.
        let mut probe_memory = ProbeMemoryMap::new();
        // Serving-strike history across ticks for the straggler sweep
        // (issue #777 ask-the-host); pruned to the current HostLost set
        // inside the sweep.
        let mut straggler_strikes = StragglerStrikeMap::new();
        loop {
            tick.tick().await;
            if let Err(e) = run_once(
                &cfg,
                &state,
                &claimant,
                &mut probe_memory,
                &mut straggler_strikes,
            )
            .await
            {
                tracing::warn!(error = %e, "dead-host detector tick failed; will retry");
            }
        }
    })
}

/// One detector sweep. `pub` so tests and the DST harness (engram-dst,
/// ADR 0098 D5) drive it directly without the timer loop.
pub async fn run_once(
    cfg: &DeadHostConfig,
    state: &SharedState,
    claimant: &str,
    probe_memory: &mut ProbeMemoryMap,
    straggler_strikes: &mut StragglerStrikeMap,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state
        .services
        .meta
        .list_stale_hosts(cfg.stale_threshold.as_secs())
        .await?;
    host_lost_straggler_sweep(cfg, state, straggler_strikes).await?;
    // A host that stopped being a candidate recovered (its heartbeats
    // are landing again) — drop its strikes/rescue history.
    let ids: std::collections::HashSet<HostId> = candidates.iter().map(|h| h.id).collect();
    probe_memory.retain(|id, _| ids.contains(id));
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "dead-host detector found stale candidates"
    );
    for host in candidates {
        let host_addr = host.host_addr.clone();
        if let Err(e) = evict_host(cfg, state, claimant, host.id, host_addr, probe_memory).await {
            tracing::warn!(host_id = %host.id, error = %e, "evict failed; another replica may have it");
        }
    }
    Ok(())
}

/// Settle HostLost rows whose inline second-stage transition never ran
/// or failed. This is deliberately a delayed backstop, not the normal
/// HostLost path.
///
/// **Convergence (oracle #8's shape):** every arm terminates in a settle
/// within bounded cycles. A row younger than the 60s min-age is skipped
/// (a later cycle handles it); an unbound row settles immediately; a
/// bound row whose host is gone/silent (probe fails / no backend) settles
/// immediately; a bound row whose host still reports the sandbox SERVING
/// is deferred at most `straggler_serving_strike_cap` cycles (the
/// ask-the-host defer, #777) and then settles. No arm parks forever — the
/// #762/#769 eternal-wedge is not reintroduced.
pub async fn host_lost_straggler_sweep(
    cfg: &DeadHostConfig,
    state: &SharedState,
    straggler_strikes: &mut StragglerStrikeMap,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let sessions = meta.list_host_lost_sessions().await?;

    // Prune serving-strike history to the rows still HostLost — a session
    // that settled (or a competing replica settled) drops its strikes, so
    // a fresh HostLost episode starts clean.
    let host_lost_ids: std::collections::HashSet<SessionId> =
        sessions.iter().map(|s| s.id).collect();
    straggler_strikes.retain(|sid, _| host_lost_ids.contains(sid));

    for session in sessions {
        let now = state.services.clock.now_utc();
        // Keep this sweep a backstop: flip_missing and evict_host normally
        // settle HostLost inline. Only rows stranded for more than a tick's
        // grace should be repaired here.
        if now - session.last_active_at <= chrono::Duration::seconds(60) {
            continue;
        }

        if let Some(sandbox_id) = session.sandbox_id {
            // ADR 0098 Phase 3 / #777 "ask-the-host": before destroying a
            // still-bound sandbox, consult HOST TRUTH. The running-sandbox
            // SET is not persisted in PG (only a count + last_heartbeat_at),
            // so we use the same direct probe reconcile's ADR 0068 belt uses
            // — `probe_sandbox` → `process_alive`. A live-and-serving VM
            // under a HostLost row is the >60s partition/desync window
            // (#776 review): the reattach machinery may still recover it in
            // place, so DEFER the destroy and bank a serving-strike rather
            // than kill the live VM. Only a probe that FAILS (host gone /
            // unreachable / `Unsupported` from an old host-agent), a
            // process that is NOT alive, or the strike cap being reached
            // proceeds to destroy — mirroring reconcile's "only an explicit
            // process_alive == true rescues" asymmetry. No backend in the
            // registry ⇒ the host is already gone ⇒ nothing to protect ⇒
            // proceed.
            let serving = match session
                .host_id
                .and_then(|host_id| state.host_registry.backend_of(host_id))
            {
                Some(backend) => {
                    matches!(backend.probe_sandbox(sandbox_id).await, Ok(p) if p.process_alive)
                }
                None => false,
            };
            if serving {
                let strikes = straggler_strikes.entry(session.id).or_default();
                *strikes += 1;
                if *strikes < cfg.straggler_serving_strike_cap {
                    ::metrics::counter!(crate::metrics::HOST_LOST_STRAGGLER_DEFERRED_SERVING_TOTAL)
                        .increment(1);
                    tracing::warn!(
                        session_id = %session.id,
                        %sandbox_id,
                        strikes = *strikes,
                        cap = cfg.straggler_serving_strike_cap,
                        "host-lost straggler: host still reports the sandbox SERVING — deferring \
                         destroy+settle for the reattach machinery (ask-the-host, #777)",
                    );
                    continue;
                }
                tracing::warn!(
                    session_id = %session.id,
                    %sandbox_id,
                    strikes = *strikes,
                    "host-lost straggler: serving-strike cap reached — destroying and settling \
                     (bounded convergence, #777)",
                );
            }
            // Not serving, or the strike cap is reached: proceed to destroy
            // + settle as before. Drop any strike history for this row.
            straggler_strikes.remove(&session.id);

            state.host_registry.invalidate_sandbox(sandbox_id);

            if let Some(backend) = session
                .host_id
                .and_then(|host_id| state.host_registry.backend_of(host_id))
            {
                if let Err(e) = backend.destroy(sandbox_id, SessionFence::unfenced()).await {
                    tracing::warn!(
                        error = %e,
                        session_id = %session.id,
                        %sandbox_id,
                        "host-lost straggler destroy failed; continuing settlement",
                    );
                }
            }

            if let Err(e) = meta
                .assign_session_sandbox_guarded(session.id, None, Some(Some(sandbox_id)), &[])
                .await
            {
                if !matches!(e, MetaError::Conflict(_)) {
                    tracing::warn!(
                        error = %e,
                        session_id = %session.id,
                        "host-lost straggler sandbox clear failed",
                    );
                }
                continue;
            }
        }

        if session.host_id.is_some() {
            if let Err(e) = meta.assign_session_host(session.id, None).await {
                tracing::warn!(
                    error = %e,
                    session_id = %session.id,
                    "host-lost straggler host clear failed",
                );
                continue;
            }
        }

        let snapshot = match meta.latest_snapshot_for_session(session.id).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session.id,
                    "host-lost straggler snapshot lookup failed",
                );
                continue;
            }
        };
        let has_recoverable_snapshot = snapshot.as_ref().is_some_and(|s| s.recoverable);
        let target = recovery_target(
            has_recoverable_snapshot,
            session.live_disk_manifest.is_some(),
        );
        note_unrecoverable_if_dead(target, snapshot.as_ref(), session.id);
        match meta.transition_session(session.id, target).await {
            Ok(prev) => {
                emit_status_changed(meta, &state.events, session.id, prev, target, now).await;
                ::metrics::counter!(crate::metrics::HOST_LOST_STRAGGLERS_SETTLED_TOTAL)
                    .increment(1);
                tracing::info!(
                    session_id = %session.id,
                    ?target,
                    "host-lost straggler settled",
                );
            }
            Err(MetaError::Conflict(_)) => {}
            Err(e) => tracing::warn!(
                error = %e,
                session_id = %session.id,
                ?target,
                "host-lost straggler second-stage transition failed",
            ),
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

/// THE HostLost stage-2 routing predicate (ADR 0045 Phase A; unified in
/// issue #777, ADR 0098 Phase 3 "honest-Dead"). A session is recoverable
/// — routed to `Idle` for lazy `/resume` on next access — iff it has a
/// **recoverable** memory snapshot OR a live disk manifest (the latter
/// still resumes via the cold-boot path). With nothing recoverable it
/// goes to `Dead` — never an `Idle` that lies about resumability.
///
/// `has_recoverable_snapshot` is the honest predicate: it is the latest
/// snapshot's `recoverable` flag (the BlobStorage HEAD result at
/// snapshot-take time), NOT the mere presence of a snapshot row. Before
/// #777 the two dead-host sites keyed on `snapshot.is_some()`, disagreeing
/// with `reconcile::flip_missing`, which already keyed on `recoverable`;
/// the filtered predicate is now the one true stage-2 decision, shared by
/// every site.
///
/// The detector no longer routes into `Evacuating`; proactive relocation
/// is operator drain only (ADR 0044 K3).
pub(crate) fn recovery_target(
    has_recoverable_snapshot: bool,
    has_live_manifest: bool,
) -> SessionState {
    if has_recoverable_snapshot || has_live_manifest {
        SessionState::Idle
    } else {
        SessionState::Dead
    }
}

/// The "snapshot rows exist but none is recoverable" signal (issue #777,
/// ADR 0098 Phase 3 honest-Dead). When stage 2 routes a session to `Dead`
/// while a snapshot row DID exist, the snapshot was un-recoverable (its
/// BlobStorage HEAD failed at take-time) and there was no live disk
/// manifest either. Emit a distinct warn + counter so a bad-capture
/// pipeline stays visible instead of hiding behind a generic Dead — a
/// no-op when the target is `Idle` (recoverable) or when there was no
/// snapshot at all (a genuinely never-checkpointed session).
pub(crate) fn note_unrecoverable_if_dead(
    target: SessionState,
    latest_snapshot: Option<&engram_core::types::snapshot::SnapshotRecord>,
    session_id: SessionId,
) {
    if target == SessionState::Dead && latest_snapshot.is_some() {
        ::metrics::counter!(crate::metrics::HOST_LOST_UNRECOVERABLE_SNAPSHOT_TOTAL).increment(1);
        tracing::warn!(
            session_id = %session_id,
            "HostLost stage 2: snapshot row(s) exist but none is recoverable and no live disk \
             manifest — routing to Dead (bad-capture signal, issue #777). Check snapshot \
             durability (BlobStorage HEAD at capture time)",
        );
    }
}

async fn evict_host(
    cfg: &DeadHostConfig,
    state: &SharedState,
    claimant: &str,
    host_id: HostId,
    host_addr: Option<String>,
    probe_memory: &mut std::collections::HashMap<HostId, ProbeMemory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    if !meta
        .try_acquire_dead_host_lease(host_id, claimant, cfg.lease_stale_after)
        .await?
    {
        // Another coordinator replica holds a live lease on this host.
        // It will do the eviction; we just skip.
        tracing::debug!(host_id = %host_id, "eviction lease contested; skipping");
        return Ok(());
    }
    // Run the eviction with the lease held, releasing on EVERY outcome
    // (including errors — the lease is not a lock; a leaked row would
    // only delay a retry by `lease_stale_after`, but there is no reason
    // to pay that on a clean error path).
    let result = evict_host_locked(cfg, state, host_id, host_addr, probe_memory).await;
    if let Err(e) = meta.release_dead_host_lease(host_id, claimant).await {
        tracing::warn!(
            host_id = %host_id,
            error = %e,
            "dead-host lease release failed; stale takeover will reap it",
        );
    }
    result
}

async fn evict_host_locked(
    cfg: &DeadHostConfig,
    state: &SharedState,
    host_id: HostId,
    host_addr: Option<String>,
    probe_memory: &mut std::collections::HashMap<HostId, ProbeMemory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let host_registry = &state.host_registry;
    let events = &state.events;

    // Re-check the host's status *after* taking the lease — another
    // replica that already won may have flipped it to Dead in the
    // window between our `list_stale_hosts` and now.
    let status = meta.host_status(host_id).await?;
    if matches!(
        status,
        Some(engram_core::types::host::HostStatus::Dead) | None
    ) {
        tracing::debug!(host_id = %host_id, "host already dead; skipping");
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
    // Probe via the client THIS pod already holds for the host — the same
    // `host_registry` seam reconcile and the straggler sweep probe through
    // (`backend_of`), not a second, lower-level channel cache. The primary
    // #231 failure mode is exactly the one where THIS pod is healthy: a
    // PEER pod's PG pool saturates and stops persisting H's
    // `last_heartbeat_at`, staling the row, while this pod still receives
    // H's heartbeats and so holds a live registry client — probing it is
    // the most direct "is H actually gone?" test. (Before ADR 0098 Phase 3,
    // this path went straight to `host_pool.get`, a seam the coordinator
    // otherwise never uses for host RPCs and that the DST harness leaves
    // unpopulated, so the probe silently never ran in-sim and a live-but-
    // stale host was orphaned on mere heartbeat staleness — issue #787.)
    //
    // Only when the registry has nothing for H (the cross-pod case: this
    // pod never saw H register) do we fall back to warming a fresh dial
    // from the persisted `host_addr`. No client anywhere and no addr
    // (pre-0013 row) ⇒ unprobeable ⇒ fall through to eviction, exactly as
    // before this guard existed. `Some(answered)` = we had something to
    // probe with (a dial that failed to warm counts as a FAILED probe —
    // unreachability evidence, same as a failed Ping); `None` = unprobeable.
    let probe_outcome: Option<bool> = if let Some(client) = state.host_registry.backend_of(host_id)
    {
        Some(host_responds(&client).await)
    } else {
        match state.services.host_pool.get(host_id) {
            Ok(c) => {
                let client: Arc<dyn engram_core::traits::HostClient> = Arc::new(c);
                Some(host_responds(&client).await)
            }
            Err(_) => match host_addr {
                Some(addr) => match state.services.host_pool.get_or_warm(host_id, addr).await {
                    Ok(c) => {
                        let client: Arc<dyn engram_core::traits::HostClient> = Arc::new(c);
                        Some(host_responds(&client).await)
                    }
                    Err(e) => {
                        tracing::debug!(host_id = %host_id, error = %e, "dead-host probe: could not warm a dial; treating as a failed probe");
                        Some(false)
                    }
                },
                None => None,
            },
        }
    };
    match probe_outcome {
        Some(true) => {
            // ADR 0068: this probe already existed (added in `7fcc4c3c`).
            // Graphing it alongside the reconcile probe's rescue counter
            // (`RECONCILE_PROBE_RESCUES_TOTAL`) makes both rescue paths
            // visible together. The rescue also arms the grace window: a
            // host proven alive NOW can't be killed by a single failed
            // probe on the next tick (prod 2026-07-09, rk28).
            ::metrics::counter!(crate::metrics::DEAD_HOST_PROBE_RESCUES_TOTAL).increment(1);
            probe_memory.insert(
                host_id,
                ProbeMemory {
                    consecutive_failures: 0,
                    last_rescue: Some(state.services.clock.now_mono()),
                },
            );
            tracing::warn!(
                host_id = %host_id,
                "stale row but live host — host answered Ping while last_heartbeat_at is stale; SKIPPING eviction. Check heartbeat persistence (coord PG pool saturation?) — see engram_heartbeat_persist_failures_total (issue #231)",
            );
            return Ok(());
        }
        Some(false) => {
            // Failed probe: one strike. Evict only with enough
            // consecutive strikes AND no recent rescue — a genuinely
            // dead host fails every tick and pays only
            // `(min_probe_failures - 1) × poll_interval`; a flapping
            // host rides it out.
            let mem = probe_memory.entry(host_id).or_default();
            mem.consecutive_failures = mem.consecutive_failures.saturating_add(1);
            let now = state.services.clock.now_mono();
            if !probe_failure_permits_eviction(
                mem,
                now,
                cfg.min_probe_failures,
                cfg.probe_rescue_grace,
            ) {
                tracing::warn!(
                    host_id = %host_id,
                    strikes = mem.consecutive_failures,
                    min_strikes = cfg.min_probe_failures,
                    recently_rescued = mem
                        .last_rescue
                        .is_some_and(|t| now.saturating_sub(t) < cfg.probe_rescue_grace),
                    "stale row + failed probe, but not enough evidence to orphan its sessions yet; deferring eviction to a later tick",
                );
                return Ok(());
            }
        }
        // Unprobeable (no host_addr): legacy immediate eviction.
        None => {}
    }
    probe_memory.remove(&host_id);

    let affected = meta.mark_host_dead_and_orphan_sessions(host_id).await?;

    // Notify other replicas so they drop their HostRegistry entry.
    meta.notify_host_dead(host_id).await?;

    // Stage 1: emit StatusChanged{prev -> HostLost} for every session
    // the bulk touched. The `prev` came back from the UPDATE so the
    // `from` is honest (not a hand-encoded `Active` that would lie if
    // the session had been Idle).
    for (session_id, prev) in &affected {
        emit_status_changed(
            meta,
            events,
            *session_id,
            *prev,
            SessionState::HostLost,
            state.services.clock.now_utc(),
        )
        .await;
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
    // Decision matrix (issue #777 honest-Dead: the snapshot column is the
    // `recoverable` FLAG, not mere row presence — an un-recoverable
    // snapshot is NOT resumable and must not route to a lying Idle):
    //
    // | recoverable snap | live_manifest | next state | who recovers it          |
    // |------------------|---------------|------------|--------------------------|
    // | true             | _             | Idle       | user/exec /resume        |
    // | false/none       | Some          | Idle       | /resume (disk-only cold) |
    // | false/none       | None          | Dead       | (no recoverable state)   |
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

        let has_recoverable_snapshot = snapshot.as_ref().is_some_and(|s| s.recoverable);
        let target = recovery_target(has_recoverable_snapshot, has_live_manifest);
        note_unrecoverable_if_dead(target, snapshot.as_ref(), *session_id);

        match meta.transition_session(*session_id, target).await {
            Ok(prev) => {
                emit_status_changed(
                    meta,
                    events,
                    *session_id,
                    prev,
                    target,
                    state.services.clock.now_utc(),
                )
                .await;
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The detector's polling loop and lease dance need a
    // MetadataStore with real lease semantics to test meaningfully
    // (live Postgres, or engram-sim's SimMetadataStore). The trait-layer logic
    // (`mark_host_dead_and_orphan_sessions` semantics) is covered
    // by Mock-based tests in `tests/dead_host_mock.rs`. End-to-end
    // multi-replica behaviour is the live-Postgres test
    // (`#[ignore]`'d, gated behind dev-VM Docker compose).

    // ADR 0045 Phase A + issue #777 honest-Dead: the stage-2 routing
    // decision. A RECOVERABLE dead-host session goes to Idle (lazy
    // /resume), never Evacuating (the reactive auto-evac is retired);
    // only the no-recoverable-state case is terminal. The first arg is
    // the snapshot's `recoverable` FLAG, not mere row presence — an
    // un-recoverable snapshot alone is NOT resumable.
    #[test]
    fn recovery_target_routes_recoverable_to_idle_never_evacuating() {
        // recoverable snapshot → Idle (memory + disk resume).
        assert_eq!(recovery_target(true, false), SessionState::Idle);
        // disk-only (live manifest, no recoverable snapshot) → Idle (cold-boot resume).
        assert_eq!(recovery_target(false, true), SessionState::Idle);
        // both present → Idle.
        assert_eq!(recovery_target(true, true), SessionState::Idle);
        // nothing recoverable (no recoverable snapshot, no manifest) → Dead.
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
        async fn destroy(
            &self,
            _id: SandboxId,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn probe_sandbox(
            &self,
            _id: SandboxId,
        ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
            unimplemented!()
        }
        async fn exec_stream(
            &self,
            _id: SandboxId,
            _cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            unimplemented!()
        }
        async fn snapshot(
            &self,
            _id: SandboxId,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<SnapshotMetadata, SandboxError> {
            unimplemented!()
        }
        async fn restore(
            &self,
            _metadata: SnapshotMetadata,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<SandboxId, SandboxError> {
            unimplemented!()
        }
        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: AgentSpec,
            _policy: SessionEgressPolicy,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn apply_egress_policy(
            &self,
            _policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
            unimplemented!()
        }
        async fn bind_session(
            &self,
            _session_id: SessionId,
            _sandbox_id: SandboxId,
            _binding_epoch: u64,
        ) {
            unimplemented!()
        }
        async fn unbind_session(&self, _session_id: SessionId) {
            unimplemented!()
        }
        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _prompt_id: String,
            _text: String,
        ) -> Result<(), SandboxError> {
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

    // Prod incident 2026-07-09 (rk28): during a ~2-minute post-roll
    // http2 flap the detector's probe rescued the host four times, then
    // a SINGLE failed Ping — twenty seconds after the last rescue —
    // marked it dead and orphaned two live sessions. The verdict below
    // is the gate that makes that impossible: a failed probe evicts
    // only with `min_probe_failures` consecutive strikes AND no rescue
    // within `probe_rescue_grace`.
    const MIN: u32 = 3;
    const GRACE: Duration = Duration::from_secs(120);

    fn mem(failures: u32, rescued_ago: Option<Duration>) -> (ProbeMemory, Duration) {
        // `now` is a monotonic mark (ADR 0098 D1: `Clock::now_mono()`
        // returns a `Duration`). Anchor it far enough from the
        // ProbeMemory's rescue mark that subtraction can't underflow.
        let now = GRACE * 10;
        let m = ProbeMemory {
            consecutive_failures: failures,
            last_rescue: rescued_ago.map(|ago| now - ago),
        };
        (m, now)
    }

    #[test]
    fn single_probe_failure_never_evicts() {
        // The incident shape: one failed probe, host rescued 20s ago.
        let (m, now) = mem(1, Some(Duration::from_secs(20)));
        assert!(!probe_failure_permits_eviction(&m, now, MIN, GRACE));
        // Even with no rescue on record, one strike isn't enough.
        let (m, now) = mem(1, None);
        assert!(!probe_failure_permits_eviction(&m, now, MIN, GRACE));
    }

    #[test]
    fn consecutive_failures_without_a_rescue_evict() {
        // The genuine dead host (kill -9): never answers, accumulates
        // strikes across ticks, evicts at the threshold.
        let (m, now) = mem(MIN - 1, None);
        assert!(!probe_failure_permits_eviction(&m, now, MIN, GRACE));
        let (m, now) = mem(MIN, None);
        assert!(probe_failure_permits_eviction(&m, now, MIN, GRACE));
    }

    #[test]
    fn recent_rescue_blocks_eviction_even_at_the_strike_threshold() {
        // Enough strikes, but the host answered a probe inside the
        // grace window — mid-flap until proven otherwise.
        let (m, now) = mem(MIN + 2, Some(GRACE - Duration::from_secs(1)));
        assert!(!probe_failure_permits_eviction(&m, now, MIN, GRACE));
    }

    #[test]
    fn expired_rescue_grace_allows_eviction_with_enough_strikes() {
        // The flap turned out to be a real death: the last rescue is
        // beyond the grace and the strikes kept mounting — evict.
        let (m, now) = mem(MIN, Some(GRACE));
        assert!(probe_failure_permits_eviction(&m, now, MIN, GRACE));
        let (m, now) = mem(MIN, Some(GRACE * 2));
        assert!(probe_failure_permits_eviction(&m, now, MIN, GRACE));
    }
}
