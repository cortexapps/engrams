//! Pinned regression seeds (ADR 0098 D7).
//!
//! Every simulator-found bug gets its seed pinned HERE with a comment
//! naming the finding and its fix — the sim-swarm's permanent memory.
//! Random exploration lives in the CI swarm (`just sim-swarm`); this
//! file replays known-bad interleavings forever.

use engram_dst::{Profile, Sim};

fn run(seed: u64, profile: Profile, steps: u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(seed, profile);
        if let Err(msg) = sim.run(steps).await {
            panic!(
                "pinned seed {seed} regressed: {msg}\ntrace tail:\n{}",
                sim.report()
                    .trace
                    .iter()
                    .rev()
                    .take(25)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    });
}

/// Issue #722: chaos seed 0 first exposed the placement over-reservation
/// hole (the 10-minute crash-orphan exclusion vs the ADR 0079 pending
/// revival backstop) at step 547 under the UNCONDITIONAL accounting
/// oracle. The shipped oracle is scoped to placement's self-consistent
/// arithmetic until #722's fix lands; this pin keeps the interleaving
/// alive so tightening the oracle back re-tests the exact scenario.
#[test]
fn issue_722_placement_over_reservation_interleaving() {
    run(0, Profile::Chaos, 600);
}

/// Nightly seed 33043259: idle-eviction nomination plus a host restart
/// loses the VM, the evict op exhausts its 20-attempt budget, and the
/// fallback parks the session at HostLost with bindings still set. Before
/// issue #762, no driver ever ran HostLost stage 2 again; dead_host's
/// host_lost_straggler_sweep is the fix.
#[test]
fn issue_762_host_lost_straggler_after_evict_budget_exhaustion() {
    run(33043259, Profile::Chaos, 5000);
}

/// R1.7a swarm find (chaos seed 96 at 1500 steps): first exposed once the
/// EnableScanner / CheckpointRetention / BaseSnapshotRetention DriverKinds
/// grew the driver menu and reshuffled exploration. A different route into
/// `HostLost` than seed 33043259 above (the grown menu manufactures its own
/// interleaving), it lands a session at `HostLost` that no inline stage-2
/// ever settles — the row sits stuck through full quiescence, tripping
/// `quiescence-no-stragglers`. Only `dead_host::host_lost_straggler_sweep`
/// (#770) moves it on: verified this pin FAILS ("session … stuck at
/// HostLost after convergence") with the sweep call commented out and
/// PASSES with it, so it is a live regression guard on the sweep, not a
/// tautology.
#[test]
fn seed_96_host_lost_straggler_settles_via_sweep() {
    run(96, Profile::Chaos, 1500);
}

/// ADR 0098 coverage-gap G1 (PR #743, session 03e6535e): a resume-class op
/// wedged forever inside its host RPC while the within-step heartbeat kept
/// the op row fresh — stale-op reclaim never fired and the op pinned for
/// 40 minutes until a pod roll. The fix (`op_deadline`, deliberately on the
/// tokio timer) bounds the wedge; the fault (`Step::RpcHang`, replacing the
/// never-read `rpc_partitioned` flag) is what lets the sim FIRE it: the
/// hung verb resolves only because the deadline drops it (pre-#743, the
/// `Driver(SessionOps)` step below would hang this test forever), the op
/// requeues, and the healed quiescence pass must converge it to a terminal
/// state — `no_op_dropped` + `quiescence-no-stragglers` are the catch.
#[test]
fn wedged_boot_op_reaches_terminal_via_op_deadline() {
    use engram_dst::{DriverKind, Step};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(43, Profile::Calm);

        // Hosts must be registered (heartbeats) for the create to PLACE —
        // only a Placed create enqueues the create_boot op.
        sim.execute(Step::HostHeartbeats).await;
        // Every RPC verb on both hosts hangs far past the 600s CreateBoot
        // deadline (the #743 wedge: heartbeats stay healthy, the verb
        // never returns). Armed BEFORE the create: since R1.7c the sim
        // drives a claimed op synchronously inside the enqueuing step
        // (no detached spawn), so the wedged dispatch happens inline in
        // CreateSession below.
        sim.execute(Step::RpcHang(0, true)).await;
        sim.execute(Step::RpcHang(1, true)).await;

        // The wedged attempt. This step RETURNING AT ALL is the fix
        // working — only the tokio-timer op_deadline breaks the hang
        // (auto-advance fires it deterministically on the paused clock);
        // the op requeues with backoff rather than completing.
        sim.execute(Step::CreateSession).await;

        // The executor pass finds nothing dispatchable (the op is backing
        // off) and must not wedge either.
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;

        // The wedge genuinely happened AND the deadline genuinely broke it:
        // the create_boot op was attempted (attempts advanced past the
        // initial claim), did NOT finish, and is back in a non-terminal
        // state awaiting retry — never a completed boot, never a dropped op.
        let (attempted, booted) = sim.world.meta.with_db(|db| {
            let op = db
                .session_ops
                .values()
                .find(|o| o.kind == engram_core::types::session_op::OpKind::CreateBoot)
                .expect("CreateSession enqueued a create_boot op");
            (
                op.attempts >= 1 && op.finished_at.is_none(),
                db.sessions
                    .values()
                    .any(|r| r.session.status == engram_core::types::session::SessionState::Active),
            )
        });
        assert!(
            attempted,
            "the create_boot op must have been attempted and requeued by the \
             deadline — an un-attempted op means the wedge was never exercised",
        );
        assert!(
            !booted,
            "the hung verb must never have completed the boot — the deadline \
             dropped the dispatch and requeued the op",
        );

        // Quiescence: run(0) heals every fault (incl. the hang) and drives
        // all rounds; check_quiescence (no_op_dropped + no-stragglers)
        // proves the wedged op converged to done/failed — the G1 property.
        if let Err(msg) = sim.run(0).await {
            panic!("wedged op failed to converge after heal: {msg}");
        }
    });
}

/// Issue #787 (ADR 0098 Phase 3, R3): the ADR 0090 single-ownership
/// split-brain — a session ending up with TWO live sandboxes across the
/// fleet — reproduced from the op-path workload alone once the sim's hosts
/// are made FAITHFUL (schedulable through the digest-gated `candidates_for`
/// path). Hand-driven (the `wedged_boot` precedent) rather than a swarm
/// pick, because the faithful-host mode is opt-in (`with_faithful_hosts`)
/// until the sibling classes it also unmasks — `placement-accounting`
/// (#722) and evict→resume `snapshot-safety` — are fixed and it can become
/// the swarm default.
///
/// ROOT CAUSE: the dead-host detector's issue-#231 liveness probe was
/// structurally unmodeled in the DST harness — it dialed through
/// `services.host_pool` (a concrete `GrpcHostPool` the sim leaves empty)
/// with hosts carrying `host_addr = None`, so `evict_host_locked` hit the
/// "unprobeable → legacy immediate eviction" arm and marked a LIVE host
/// dead on mere heartbeat staleness. The session flipped
/// Active→HostLost→Idle (its VM never torn down — the host was up the whole
/// time), then a resume booted a SECOND sandbox while the first survived.
/// The fix routes the probe through `host_registry.backend_of` (the same
/// seam reconcile and the straggler sweep already use), so a live host
/// answers the Ping and is rescued — no false HostLost, no second boot.
///
/// FAIL-WITHOUT / PASS-WITH: with the `dead_host` probe change reverted
/// this test FAILS (the live host is falsely evicted; the resume double-
/// boots and the session owns 2 live sandboxes); with the fix it PASSES
/// (the host is rescued, the session stays Active with its one VM).
#[test]
fn issue_787_dead_host_false_evict_double_boot() {
    use engram_core::types::session::SessionState;
    use engram_dst::{DriverKind, Step};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        // Faithful hosts: schedulable, so the create boots a real bound VM
        // and a later resume can place a fresh one (the second boot).
        let mut sim = Sim::new(4, Profile::Calm).with_faithful_hosts();

        // Register the fleet (heartbeats stamp last_heartbeat_at = now) and
        // boot one session to Active with a bound sandbox.
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;

        // A recoverable checkpoint on every host so the (false) HostLost
        // routes stage-2 to Idle (resumable), not Dead — the resume is what
        // manufactures the second sandbox.
        sim.execute(Step::HostCheckpoint(0)).await;
        sim.execute(Step::HostCheckpoint(1)).await;
        sim.execute(Step::HostCheckpoint(2)).await;

        let session_id = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions
                    .values()
                    .find(|r| r.session.status == SessionState::Active)
                    .map(|r| r.session.id)
            })
            .expect("one Active session with a bound sandbox after create");

        // Advance the clock past the 30s dead-host staleness threshold
        // WITHOUT another heartbeat: every host is now a stale candidate,
        // though every host is still UP in the world.
        sim.execute(Step::AdvanceTime(std::time::Duration::from_secs(90)))
            .await;

        // The dead-host sweep. This is where the bug lived: pre-fix the
        // probe never ran (empty host_pool + host_addr=None) so a LIVE host
        // was marked dead and the session orphaned to HostLost→Idle. Post-
        // fix the probe dials the live registry client, the host answers,
        // and the eviction is skipped.
        for r in 0..2 {
            sim.execute(Step::Driver(r, DriverKind::DeadHost)).await;
        }

        // The session must NOT have been falsely evicted: still Active, and
        // its VM never went through HostLost.
        let (status, host_lost_seen) = sim.world.meta.with_db(|db| {
            let status = db.sessions.get(&session_id).map(|r| r.session.status);
            let host_lost = db
                .transition_log
                .iter()
                .any(|e| e.session == session_id && e.to == SessionState::HostLost);
            (status, host_lost)
        });
        assert!(
            !host_lost_seen,
            "the live host was falsely evicted: session transitioned to HostLost \
             while its host was UP (issue #787 dead-host probe regression)",
        );
        assert_eq!(
            status,
            Some(SessionState::Active),
            "the rescued session must stay Active (its one VM intact), not be \
             orphaned by a false dead-host eviction",
        );

        // Drive a resume attempt + the executor: with the pre-fix false
        // Idle this booted a second sandbox; post-fix there is no Idle
        // session to resume, so nothing new is booted.
        sim.execute(Step::ResumeSession).await;
        sim.execute(Step::Driver(0, DriverKind::SessionOps)).await;

        // THE #787 INVARIANT: exactly one live sandbox is owned by the
        // session across the whole fleet (ADR 0090 single-ownership).
        let owned = {
            let hosts = sim.world.host_world.hosts.lock();
            hosts
                .values()
                .flat_map(|h| h.sandboxes.values().flatten().copied())
                .filter(|owner| *owner == session_id)
                .count()
        };
        assert_eq!(
            owned, 1,
            "session must own exactly ONE live sandbox across the fleet; owning \
             {owned} is the ADR 0090 split-brain (issue #787)",
        );

        // And it converges cleanly once the fleet heals.
        if let Err(msg) = sim.run(0).await {
            panic!("post-repro convergence failed: {msg}");
        }
    });
}
