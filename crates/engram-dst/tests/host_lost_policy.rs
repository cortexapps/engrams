//! Issue #777 HostLost stage-2 policy proofs (ADR 0098 Phase 3),
//! driven against the REAL coordinator drivers over engram-sim.
//!
//! Two design calls, each pinned red-then-green:
//!   1. **honest-Dead** — a bound HostLost straggler whose only snapshot
//!      is un-recoverable settles to `Dead`, never a lying `Idle`.
//!   2. **ask-the-host** (added with the sweep's serving-defer in the
//!      same PR's second commit) — a bound HostLost straggler whose host
//!      still reports the sandbox serving is NOT destroyed on the first
//!      sweep; it is deferred for the reattach machinery until the
//!      serving-strike cap, then settles (convergence — oracle #8's shape).

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{Clock as _, MetadataStore as _};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use engram_dst::{DriverKind, Profile, Sim, Step};
use engram_sim::SimMetadataStore;

/// Build the paused-clock current-thread runtime the sim requires and run
/// `body` with a fresh `Sim`. Mirrors the harness in `regression_seeds.rs`.
fn on_sim<Fut>(seed: u64, body: impl FnOnce(Sim) -> Fut)
where
    Fut: std::future::Future<Output = ()>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        body(Sim::new(seed, Profile::Calm)).await;
    });
}

/// Park a session at HostLost with `host_id` + `sandbox_id` STILL bound —
/// the #762 straggler shape (an evict-budget-exhaustion / idle-evictor
/// fallback leaves the bindings set and no inline stage-2 ever runs). All
/// staged through legal FSM edges on the REAL store.
async fn seed_bound_host_lost(
    meta: &Arc<SimMetadataStore>,
    host: HostId,
    sandbox: SandboxId,
) -> SessionId {
    let sid = meta
        .create_session(SessionSpec {
            image: "sim:host-lost".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    meta.assign_session_host(sid, Some(host))
        .await
        .expect("assign host");
    meta.transition_session_created(sid, sandbox)
        .await
        .expect("created");
    meta.transition_session(sid, SessionState::Active)
        .await
        .expect("active");
    meta.transition_session(sid, SessionState::HostLost)
        .await
        .expect("host-lost (bindings intact)");
    sid
}

/// Record one snapshot row for `sid` with the given `recoverable` flag.
async fn record_snapshot(
    meta: &Arc<SimMetadataStore>,
    sid: SessionId,
    recoverable: bool,
    at: chrono::DateTime<chrono::Utc>,
) {
    let snap: SnapshotRecord = serde_json::from_value(serde_json::json!({
        "id": SnapshotId::new(),
        "session_id": sid,
        "host_id": null,
        "image_version": "sim",
        "size_bytes": 0,
        "created_at": at,
        "last_accessed_at": at,
        "recoverable": recoverable,
    }))
    .expect("snapshot record");
    meta.record_snapshot(snap).await.expect("record snapshot");
}

fn status(sim: &Sim, sid: SessionId) -> SessionState {
    sim.world
        .meta
        .with_db(|db| db.sessions.get(&sid).expect("session row").session.status)
}

/// Commit 1 (honest-Dead): a bound HostLost straggler whose ONLY snapshot
/// is un-recoverable must settle to `Dead`, never a lying `Idle`.
///
/// Red-then-green: with the pre-#777 `snapshot.is_some()` predicate the
/// sweep routed this to `Idle` (a snapshot row exists); the honest
/// `recoverable`-filtered predicate routes it to `Dead`. The sandbox is
/// deliberately NOT inserted into the host world, so the ask-the-host
/// probe (commit 2) finds no live VM and the sweep settles this same
/// cycle in BOTH commits — isolating the Idle-vs-Dead PREDICATE.
#[test]
fn unrecoverable_only_straggler_settles_dead_not_idle() {
    on_sim(0x0777_D, |mut sim| async move {
        let host = sim.world.host_ids[0];
        let sandbox = SandboxId::new();
        let meta = sim.world.meta.clone();
        let sid = seed_bound_host_lost(&meta, host, sandbox).await;

        // The only snapshot on record is un-recoverable (a torn / HEAD-
        // failed capture): the row exists, but `recoverable` is false.
        let now = sim.world.clock.now_utc();
        record_snapshot(&meta, sid, false, now).await;

        // Age past the sweep's 60s min-age, then drive the dead-host
        // sweep on replica 0.
        sim.execute(Step::AdvanceTime(Duration::from_secs(120)))
            .await;
        sim.execute(Step::Driver(0, DriverKind::DeadHost)).await;

        assert_eq!(
            status(&sim, sid),
            SessionState::Dead,
            "an un-recoverable-only straggler must settle Dead — never a lying Idle (#777 honest-Dead)",
        );
    });
}
