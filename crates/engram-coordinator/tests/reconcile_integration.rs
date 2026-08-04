//! ADR 0009 Phase 3 integration test.
//!
//! Exercises the full reconcile pipeline at the trait level (no
//! WS round-trip, no real backend, no Postgres — those are covered
//! by the dev-vm end-to-end smoke). What this file pins down is the
//! load-bearing decision logic over multiple heartbeats:
//!
//! * Sandbox present → no flip.
//! * Sandbox missing for `< grace_ticks` heartbeats → no flip, strike
//!   accumulates.
//! * Sandbox missing for `≥ grace_ticks` heartbeats:
//!   - latest snapshot `recoverable=true` → session → `Idle`.
//!   - latest snapshot `recoverable=false` (or none) → session → `Dead`.
//!
//! Closes case **B** from the ADR 0009 failure-mode taxonomy: the
//! pattern observed today where coord restarts leave Active sessions
//! pointing at sandbox_ids that no longer exist anywhere.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_coordinator::reconcile::{Reconciler, DEFAULT_GRACE_TICKS};
use engram_coordinator::state::SessionEventBus;
use engram_coordinator::HostRegistry;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::{SessionState, SnapshotRecord};
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use engram_sim::{SimEntropy, SimMetadataStore};
use parking_lot::Mutex;

// ADR 0098 D4: the hand-rolled `ReconcileMeta` mock is retired onto the
// conformance-tested `SimMetadataStore`. Sessions/strikes/snapshots are
// staged through REAL store calls (below), so the fixtures can no longer
// stage a state the production write paths couldn't reach.

fn sim_meta() -> Arc<SimMetadataStore> {
    SimMetadataStore::new(
        Arc::new(engram_core::traits::SystemClock::new()),
        Arc::new(SimEntropy::seeded(0x9EC0)),
    )
}

/// Stage an Active session bound to `(host, sandbox)` through legal FSM
/// edges: create (Pending) → assign host → `transition_session_created`
/// (Created + sandbox) → Active.
async fn seed_active(meta: &Arc<SimMetadataStore>, host: HostId, sandbox: SandboxId) -> SessionId {
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    meta.assign_session_host(id, Some(host))
        .await
        .expect("assign host");
    meta.transition_session_created(id, sandbox)
        .await
        .expect("created");
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("active");
    id
}

/// Stage an in-flight missing-heartbeat strike streak through the REAL
/// strike path (`apply_missing_sandbox_strikes`), as if earlier missing
/// heartbeats had already accrued against this binding (Issue #215).
/// Resets to a known 0 (present-arm) first, then accrues `strikes` with
/// an unreachable grace so the staging itself never flips.
async fn seed_strikes(meta: &Arc<SimMetadataStore>, session: SessionId, strikes: i32) {
    meta.apply_missing_sandbox_strikes(&[session], &[], i32::MAX)
        .await
        .expect("reset strikes");
    for _ in 0..strikes {
        let flipped = meta
            .apply_missing_sandbox_strikes(&[], &[session], i32::MAX)
            .await
            .expect("accrue strike");
        assert!(flipped.is_empty(), "staging strikes must not flip");
    }
}

async fn seed_recoverable_snapshot(
    meta: &Arc<SimMetadataStore>,
    session: SessionId,
    recoverable: bool,
) {
    let snap = SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session),
        host_id: None,
        image_version: "test".into(),
        size_bytes: 1024,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: Some(ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: 1,
        }),
        memory_manifest: None,
        recoverable,
        aux_bundles: vec![],
        events_cursor: None,
        fc_snapshot_version: None,
    };
    meta.record_snapshot(snap).await.expect("record snapshot");
}

async fn status(meta: &Arc<SimMetadataStore>, id: SessionId) -> SessionState {
    meta.get_session(id).await.expect("session").status
}

async fn sandbox(meta: &Arc<SimMetadataStore>, id: SessionId) -> Option<SandboxId> {
    meta.get_session(id).await.expect("session").sandbox_id
}

/// Every `append_session_event` the reconciler emitted, flattened out of
/// the store's event log (the mock captured these in a side Vec; the
/// faithful store persists them like PG does).
fn emitted_events(meta: &SimMetadataStore) -> Vec<(SessionId, String, serde_json::Value)> {
    meta.with_db(|db| {
        db.session_events
            .iter()
            .flat_map(|(sid, evs)| {
                evs.iter()
                    .map(move |e| (*sid, e.kind.clone(), e.payload.clone()))
            })
            .collect()
    })
}

/// The headline scenario from the ADR 0009 rollout doc:
/// "Start coord + 1 host, create 3 active sessions (one with a
/// `recoverable=true` snapshot taken first), drop two from the
/// backend's in-memory map (simulating crash), wait 20 s, assert:
/// the one with a snapshot flipped to Idle, the other to Dead, the
/// third stayed Active."
#[tokio::test]
async fn three_active_sessions_drop_two_one_recoverable_one_not() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();

    // Three sessions, each tied to a distinct sandbox_id.
    let sb_recoverable = SandboxId::new();
    let sb_dead = SandboxId::new();
    let sb_present = SandboxId::new();
    let s_recoverable = seed_active(&meta, host, sb_recoverable).await;
    let s_dead = seed_active(&meta, host, sb_dead).await;
    let s_present = seed_active(&meta, host, sb_present).await;

    // Only one of the sessions has a recoverable snapshot.
    seed_recoverable_snapshot(&meta, s_recoverable, true).await;
    // Second session: a snapshot exists but isn't recoverable
    // (mimics chunks GC'd / never replicated). Reconcile should
    // still treat this as Dead.
    seed_recoverable_snapshot(&meta, s_dead, false).await;

    // Initial state: all three sandboxes "present" in the host's
    // heartbeat. No flips expected.
    let running = vec![sb_recoverable, sb_dead, sb_present];
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &running)
        .await;
    assert!(flipped.is_empty(), "no missing sandboxes → no flips");

    // Now simulate the crash: two sandboxes vanish from the host's
    // backend. The third stays alive. Tick the reconcile loop the
    // grace-window number of times.
    let after_crash = vec![sb_present];
    for _ in 0..(DEFAULT_GRACE_TICKS as usize - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &after_crash)
            .await;
        assert!(
            flipped.is_empty(),
            "grace window absorbs transient missing-sandbox; no flips before strike-out"
        );
    }
    // On the N-th consecutive missing heartbeat, both crashed
    // sessions cross the strike threshold and flip.
    let mut flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &after_crash)
        .await;
    flipped.sort();
    let mut expected = vec![s_recoverable, s_dead];
    expected.sort();
    assert_eq!(
        flipped, expected,
        "both crashed sessions flip on the same tick; the present one is untouched"
    );

    // Verify the per-session terminal status:
    assert_eq!(
        status(&meta, s_recoverable).await,
        SessionState::Idle,
        "session with recoverable=true → Idle (resumable via cold-tier)"
    );
    assert_eq!(
        status(&meta, s_dead).await,
        SessionState::Dead,
        "session with recoverable=false → Dead (terminal; no resume path)"
    );
    assert_eq!(
        status(&meta, s_present).await,
        SessionState::Active,
        "session whose sandbox is still in the heartbeat must NOT flip"
    );

    // Both flipped sessions get sandbox_id cleared so a future
    // coord restart's `repopulate_routing` doesn't try to talk to
    // the dead sandbox.
    assert_eq!(sandbox(&meta, s_recoverable).await, None);
    assert_eq!(sandbox(&meta, s_dead).await, None);
    // The third session keeps its sandbox_id.
    assert_eq!(sandbox(&meta, s_present).await, Some(sb_present));

    // ADR 0015 M2: each flipped session emits TWO StatusChanged
    // events — Active -> HostLost (host went away) followed by
    // HostLost -> {Idle,Dead} (resolved per snapshot recoverability).
    // Two flipped sessions × two events = four events.
    let emitted = emitted_events(&meta);
    let status_changed: Vec<_> = emitted
        .iter()
        .filter(|(_, kind, _)| kind == "status_changed")
        .collect();
    assert_eq!(
        status_changed.len(),
        4,
        "two flipped sessions × (Active->HostLost + HostLost->Idle/Dead) = 4 events"
    );
    let host_lost_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "host_lost")
        .count();
    assert_eq!(
        host_lost_events, 2,
        "one Active->HostLost event per flipped session"
    );
    let idle_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "idle")
        .count();
    let dead_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "dead")
        .count();
    assert_eq!(idle_events, 1, "the recoverable session ends in Idle");
    assert_eq!(dead_events, 1, "the non-recoverable session ends in Dead");
}

/// Defends the load-bearing anti-flap property: a sandbox that's
/// missing for `grace_ticks - 1` ticks and then re-appears must NOT
/// flip. This is the "backend.list() blipped for a few ticks then
/// recovered" scenario.
#[tokio::test]
async fn re_appearing_sandbox_within_grace_does_not_flip() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb = SandboxId::new();
    let session = seed_active(&meta, host, sb).await;

    // Two consecutive missing ticks…
    for _ in 0..(DEFAULT_GRACE_TICKS - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        assert!(flipped.is_empty());
    }
    // …then the sandbox re-appears. Strikes counter must reset to 0.
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[sb])
        .await;
    assert!(
        flipped.is_empty(),
        "re-appearance within grace does not flip"
    );
    // Even if the sandbox subsequently vanishes again, it should
    // take a fresh `grace_ticks` consecutive misses to flip — not
    // just one (since the counter is back at 0).
    for _ in 0..(DEFAULT_GRACE_TICKS - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        assert!(flipped.is_empty(), "post-recovery, strikes started over");
    }
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
        .await;
    assert_eq!(flipped, vec![session], "flip on the (n+1)th total miss");
}

/// Issue #215 regression: stale strikes accumulated against a session's
/// OLD sandbox must NOT count against a brand-new sandbox after a re-key
/// (evac/resume/migration). The strike counter encodes "N CONSECUTIVE
/// heartbeats THIS binding's sandbox was missing"; rebinding to a fresh
/// sandbox breaks consecutiveness, so the new binding is owed a full
/// `grace_ticks` window. Before the fix, `assign_session_sandbox` left
/// the counter untouched, so a freshly-resumed session carrying 2 stale
/// strikes was dismantled on its first transient under-report (strike 3
/// of a contract-promised 3-tick grace collapsed to 1).
#[tokio::test]
async fn stale_strikes_do_not_carry_across_sandbox_rekey() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);

    // Session S was Active on host A, sandbox SB1, and accrued
    // `grace_ticks - 1` strikes there (a flaky agent / a couple of
    // `list()` blips) — one short of a flip.
    let host_a = HostId::new();
    let sb1 = SandboxId::new();
    let session = seed_active(&meta, host_a, sb1).await;
    seed_strikes(&meta, session, (DEFAULT_GRACE_TICKS - 1) as i32).await;
    // It has a recoverable snapshot, so a wrongful flip would land it
    // in Idle (in-RAM work lost) rather than Dead — but the point is
    // it must NOT flip at all here.
    seed_recoverable_snapshot(&meta, session, true).await;

    // S is drained off A and resumed on host B against a fresh sandbox
    // SB2 (evac/migration). This is the re-key: the binding writer
    // must reset the strike streak.
    let host_b = HostId::new();
    let sb2 = SandboxId::new();
    meta.assign_session_host(session, Some(host_b))
        .await
        .unwrap();
    meta.assign_session_sandbox(session, Some(sb2))
        .await
        .unwrap();

    // First transient under-report on B (e.g. a heartbeat sampled
    // during B's rehydration window). PRE-FIX this is strike 3 → flip.
    // POST-FIX it is strike 1 of a fresh window → no flip.
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host_b, &[])
        .await;
    assert!(
        flipped.is_empty(),
        "a single missing tick after a sandbox re-key must NOT flip a freshly-resumed session; \
         stale strikes from the old sandbox leaked into the new binding's grace window"
    );
    assert_eq!(
        status(&meta, session).await,
        SessionState::Active,
        "the just-resumed session must stay Active through its first post-rebind blip"
    );

    // It still takes a FULL fresh grace window against SB2 to flip —
    // proving the reset, not merely a one-off skip.
    for _ in 0..(DEFAULT_GRACE_TICKS - 2) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host_b, &[])
            .await;
        assert!(
            flipped.is_empty(),
            "fresh grace window not yet exhausted against the new sandbox"
        );
    }
    // The `grace_ticks`-th consecutive miss against SB2 finally flips.
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host_b, &[])
        .await;
    assert_eq!(
        flipped,
        vec![session],
        "flip only after a full fresh grace window of misses against the new sandbox"
    );
}

/// Sessions that are already in a terminal state (Dead) must not be
/// re-flipped. The reconciler's idempotency is what lets operators
/// safely manually-kill a session without racing the strike-out.
#[tokio::test]
async fn does_not_re_flip_already_terminal_sessions() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb = SandboxId::new();
    let session = seed_active(&meta, host, sb).await;

    // First strike-out cycle → flip to Dead (no snapshot).
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
    }
    assert_eq!(status(&meta, session).await, SessionState::Dead);
    let after_first_flip = emitted_events(&meta).len();

    // Even though the session row still exists, `list_active...`
    // filters status='active' so it won't be returned. Quick
    // sanity check: another N reconcile passes don't move it
    // further or emit additional events.
    for _ in 0..(DEFAULT_GRACE_TICKS * 2) {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
    }
    assert_eq!(status(&meta, session).await, SessionState::Dead);
    assert_eq!(
        emitted_events(&meta).len(),
        after_first_flip,
        "no further events after a session reaches a terminal state"
    );
}

/// A session on a *different* host must not be affected by a
/// reconcile pass for this host. This is the basic isolation
/// property: per-host reconciliation must not cross-contaminate.
#[tokio::test]
async fn reconcile_does_not_flip_sessions_on_other_hosts() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host_a = HostId::new();
    let host_b = HostId::new();

    let sb_a = SandboxId::new();
    let sb_b = SandboxId::new();
    let session_a = seed_active(&meta, host_a, sb_a).await;
    let session_b = seed_active(&meta, host_b, sb_b).await;

    // Drive host_a's reconcile pass repeatedly with NO running
    // sandboxes. session_a should flip to Dead; session_b stays
    // Active because its host (B) never reported anything.
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host_a, &[])
            .await;
    }
    assert_eq!(status(&meta, session_a).await, SessionState::Dead);
    assert_eq!(
        status(&meta, session_b).await,
        SessionState::Active,
        "host_a's reconcile must not affect host_b's sessions"
    );
}

/// ADR 0015 M3: when the reconcile pass flips Active → HostLost,
/// the corresponding HostRegistry `sandbox_owner` entry must be
/// gone. Without this, a subsequent `exec_stream(sb)` would take
/// the cache fast path against a host whose session PG already
/// knows is HostLost, returning a tcp-connect-error or a stale
/// NotFound instead of a clean 410.
#[tokio::test]
async fn reconcile_invalidates_host_registry_cache_on_host_lost() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb_lost = SandboxId::new();
    let sb_alive = SandboxId::new();
    seed_active(&meta, host, sb_lost).await;
    seed_active(&meta, host, sb_alive).await;

    // Mirror what create_for_session would have done at session
    // create time: seed the in-memory ownership cache.
    host_registry.record_sandbox_owner(sb_lost, host);
    host_registry.record_sandbox_owner(sb_alive, host);

    // sb_alive keeps showing up in the heartbeat; sb_lost vanishes.
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[sb_alive])
            .await;
    }

    assert_eq!(
        host_registry.host_of(sb_lost),
        None,
        "the flipped sandbox's cache row must be purged so a stale exec gets a 410, not a 404"
    );
    assert_eq!(
        host_registry.host_of(sb_alive),
        Some(host),
        "the alive sandbox's cache row must survive — selective per-sandbox invalidation"
    );
}

/// ADR 0068 probe-before-host_lost: a mock `HostClient` whose
/// `probe_sandbox` answers on demand — every other method is
/// `unreachable!()`, since reconcile only ever calls `probe_sandbox`
/// on a registered backend.
struct ProbeBackend {
    process_alive: Mutex<bool>,
}

#[async_trait]
impl engram_core::traits::HostClient for ProbeBackend {
    async fn create(
        &self,
        _: engram_core::types::sandbox::SandboxSpec,
    ) -> Result<SandboxId, engram_core::SandboxError> {
        unreachable!()
    }
    async fn destroy(
        &self,
        _: SandboxId,
        _: engram_core::traits::SessionFence,
    ) -> Result<(), engram_core::SandboxError> {
        unreachable!()
    }
    async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
        unreachable!()
    }
    async fn probe_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError> {
        Ok(engram_core::types::sandbox::SandboxProbe {
            known_to_backend: false,
            process_alive: *self.process_alive.lock(),
            control_alive: None,
        })
    }
    async fn exec_stream(
        &self,
        _: SandboxId,
        _: engram_core::types::sandbox::ExecRequest,
    ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError> {
        unreachable!()
    }
    async fn snapshot(
        &self,
        _: SandboxId,
        _: engram_core::traits::SessionFence,
    ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError> {
        unreachable!()
    }
    async fn restore(
        &self,
        _: engram_core::types::snapshot::SnapshotMetadata,
        _: engram_core::traits::SessionFence,
    ) -> Result<SandboxId, engram_core::SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        _: SandboxId,
        _: engram_core::types::sandbox::AgentSpec,
        _: engram_core::types::egress::SessionEgressPolicy,
        _: engram_core::traits::SessionFence,
    ) -> Result<(), engram_core::SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<std::net::Ipv4Addr> {
        unreachable!()
    }
    async fn bind_session(&self, _: SessionId, _: SandboxId, _binding_epoch: u64) {}
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(
        &self,
        _: SandboxId,
        _: String,
        _: String,
        _mode: Option<String>,
    ) -> Result<(), engram_core::SandboxError> {
        unreachable!()
    }
}

/// ADR 0068 headline scenario — the fbd3794c incident shape: a
/// sandbox that's genuinely alive (the probe says `process_alive =
/// true`) but absent from the host's self-reported `running_sandboxes`
/// for the full grace window must NOT flip to `host_lost`, and its
/// strike counter must reset (not just hold at the threshold, ready to
/// flip the instant one more heartbeat is missed).
#[tokio::test]
async fn probe_rescues_a_session_whose_process_is_alive_despite_missing_from_running_sandboxes() {
    let meta = sim_meta();
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb = SandboxId::new();
    let session = seed_active(&meta, host, sb).await;

    let backend = Arc::new(ProbeBackend {
        process_alive: Mutex::new(true),
    });
    host_registry.register(
        host,
        backend.clone() as Arc<dyn engram_core::traits::HostClient>,
    );

    // Missing from every heartbeat's running_sandboxes for MORE than
    // the grace window — without the probe rescue this would flip.
    for _ in 0..(DEFAULT_GRACE_TICKS as usize + 2) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        assert!(
            flipped.is_empty(),
            "a probe confirming process_alive=true must rescue every tick, not just once"
        );
    }
    assert_eq!(
        status(&meta, session).await,
        SessionState::Active,
        "the session must never leave Active — no host_lost, no transcript rewind"
    );

    // Now the process actually dies (the probe answers honest) — the
    // flip must proceed within the SAME grace window as today (the
    // probe rescue must not delay real failure detection). Reset the
    // strike counter to a known baseline first: phase 1's tail ticks
    // (after the last rescue mid-cycle) left a nonzero-but-unspecified
    // strike count, which would make "exactly DEFAULT_GRACE_TICKS more
    // ticks" a flaky claim about this test rather than about the code.
    seed_strikes(&meta, session, 0).await;
    *backend.process_alive.lock() = false;
    let mut all_flipped = Vec::new();
    for i in 0..DEFAULT_GRACE_TICKS {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        if i + 1 < DEFAULT_GRACE_TICKS {
            assert!(
                flipped.is_empty(),
                "must not flip before the grace window elapses, tick {}",
                i + 1
            );
        }
        all_flipped.extend(flipped);
    }
    assert_eq!(
        all_flipped,
        vec![session],
        "once the probe agrees the process is gone, the flip proceeds exactly at the grace window \
         — probe-before-host_lost must not delay real failure detection"
    );
    assert_eq!(status(&meta, session).await, SessionState::Dead);
}
