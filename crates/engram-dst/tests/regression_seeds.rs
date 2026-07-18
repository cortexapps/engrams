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
        // A session whose create_boot op is about to dispatch...
        sim.execute(Step::CreateSession).await;
        // ...onto hosts whose every RPC verb hangs far past the 600s
        // CreateBoot deadline (the #743 wedge: heartbeats stay healthy,
        // the verb never returns).
        sim.execute(Step::RpcHang(0, true)).await;
        sim.execute(Step::RpcHang(1, true)).await;

        // The wedged attempt. This step RETURNING AT ALL is the fix
        // working — only the tokio-timer op_deadline breaks the hang
        // (auto-advance fires it deterministically on the paused clock);
        // the op requeues with backoff rather than completing.
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
