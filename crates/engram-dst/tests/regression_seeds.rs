//! Pinned regression seeds (ADR 0098 D7).
//!
//! Every simulator-found bug gets its seed pinned HERE with a comment
//! naming the finding and its fix — the sim-swarm's permanent memory.
//! Random exploration lives in the CI swarm (`just sim-swarm`); this
//! file replays known-bad interleavings forever.
//!
//! ADR 0098 wave 4 reweighted BOTH pick tables (carving 6 points for the
//! Prompt/Rename/Destroy workload verbs from AdvanceTime/Driver/
//! HostHeartbeats). Per the D6 precedent, seeds pin to a commit: the shift
//! moves each seed's exploration. Every pin below was re-verified to still
//! CONVERGE and to REPLAY byte-identically after the reweight (kept as-is,
//! like #786's Chaos re-weight). The verbs draw only WORLD entropy, never
//! `self.rng` (the pick stream), so the reweight is the sole shift — the
//! seeds do not otherwise perturb.

use engram_dst::{Profile, Sim};

fn run(seed: u64, profile: Profile, steps: u64) {
    run_inner(seed, profile, steps, false);
}

/// Same, but over the opt-in faithful-host world (`with_faithful_hosts`) —
/// the digest-gated `candidates_for` scheduling path that becomes the swarm
/// default once #789's chain clears.
fn run_faithful(seed: u64, profile: Profile, steps: u64) {
    run_inner(seed, profile, steps, true);
}

fn run_inner(seed: u64, profile: Profile, steps: u64, faithful: bool) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(seed, profile);
        if faithful {
            sim = sim.with_faithful_hosts();
        }
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
/// hole at step 547 under the UNCONDITIONAL accounting oracle, back when
/// that oracle was scoped to self-consistency because the product was
/// broken. R3 landed the fix (ONE reservation authority: a `pending`
/// reserves unconditionally + resume honors the hard reserved-budget bound)
/// and RESTORED the unconditional oracle (engram-dst invariants.rs). This
/// pin keeps the non-faithful interleaving green against that armed oracle.
#[test]
fn issue_722_placement_over_reservation_interleaving() {
    run(0, Profile::Chaos, 600);
}

/// Issue #722 (R3): the SMALLEST-step faithful-host firing (chaos seed 5,
/// step 208) of the actual bug the re-open surfaced. RCA: NOT the
/// pending crash-orphan exclusion (D6-era hypothesis) — every one of the
/// 12/200 faithful firings was the UNRESERVED RESUME path. `pick_from`'s
/// capacity-soft fallback (the ADR 0046 "resume-isn't-reserved posture")
/// bound a resuming session onto a `measured_full` host (free=0), and under
/// crash/partition churn a wave of forced resumes onto full survivors drove
/// Σ reserved > allocatable (13×2048 > 24576). The fix makes resume queue
/// (Idle→queued, resume origin) when no host FITS — `placement_preview`,
/// the SAME hard 2D check the queue-scanner precheck uses (so no
/// Idle↔Queued churn). This pin runs the full 1500-step window so the
/// resume also has to queue, re-place via the RESERVED `place_queued_session`
/// once capacity frees, and converge through the quiescence drain.
#[test]
fn issue_722_r3_resume_over_commit_faithful_smallest_seed() {
    run_faithful(5, Profile::Chaos, 1500);
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
/// pick, because it needs a specific create→false-evict→resume interleaving;
/// the swarm now runs faithful BY DEFAULT (R3 #722: the sibling classes it
/// unmasked — `placement-accounting` and evict→resume `snapshot-safety` —
/// are fixed).
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

/// Issue #790 (ADR 0098 Phase 3, R3): the evict→resume snapshot-safety
/// hole — a session reaching `Idle` with NO recoverable durable copy.
///
/// ROOT CAUSE — a sim-world FIDELITY GAP, not a coordinator/product hole.
/// `SimHostClient::snapshot` returned manifest-LESS metadata
/// (`disk_manifest = memory_manifest = None`). The coordinator's honest
/// capture-time recoverability check `verify_snapshot_recoverable(blob,
/// None, None)` is `false` (nothing to HEAD), so EVERY idle-evict capture
/// recorded a `recoverable = false` snapshot row while the pipeline still
/// flipped the session to `Idle` — leaving it with no recoverable durable
/// copy. In prod a real evict capture uploads its chunked disk+memory
/// manifests (+ the portable state.bin/sidecar) to blob storage BEFORE the
/// row is recorded, so the flag is `true`; only the sim was unfaithful.
/// The fix makes the sim world faithful (world.rs): the snapshot verb now
/// writes those blobs to the SAME shared store the coordinator reads and
/// returns the refs, mirroring engram-dst-host's ledger discipline (model
/// the artifact, never fake the flag). NOTE the idle_evictor has no
/// explicit `recoverable`-before-`Idle` guard — it relies on captures always
/// producing manifests, which prod does; a transiently-unrecoverable
/// capture would land Idle-but-unrecoverable and only fail at resume
/// (→ Dead). That hardening is a separate follow-up, not this bug.
///
/// This is the default (NON-faithful) chaos-lane repro that reds `test-sim`
/// on seed 38 (surfaced once #789's dead-host timing shift reshuffled
/// exploration). FAIL-WITHOUT / PASS-WITH: reverting the snapshot-verb
/// change FAILS this at step 1436 ("session … at Idle has no recoverable
/// durable copy"); with the fix it PASSES.
#[test]
fn issue_790_evict_resume_snapshot_safety_default_lane() {
    run(38, Profile::Chaos, 1500);
}

/// Issue #790 under FAITHFUL hosts (the digest-gated `candidates_for`
/// scheduling path, now the swarm default — R3 #722): the SAME seed/session
/// reproduces the snapshot-safety hole. Seed 38 was the one isolable
/// snapshot-safety repro in the 0..200 faithful chaos swarm — the other
/// faithful failures were #722 `placement-accounting` (the resume
/// over-commit) firing at an earlier step and masking it (now fixed), and
/// calm-faithful never reaches the capture→Idle path. Pinned separately so
/// the faithful world permanently re-tests the fix. FAIL-WITHOUT /
/// PASS-WITH: pre-fix FAILS with `snapshot-safety` (NOT pre-empted by #722
/// on this seed); post-fix PASSES fully green.
#[test]
fn issue_790_evict_resume_snapshot_safety_faithful() {
    run_faithful(38, Profile::Chaos, 1500);
}

/// Issue #800: the RESERVED evac-placement over-reservation, exposed by
/// folding the operator-drain verb into the swarm (ADR 0098 Phase 3 wave
/// 5). Before the fix, `evac_resumer → evacuate_dead_source →
/// pick_for_session` was capacity-SOFT: a drain-driven wave of evacuations
/// bound measured-FULL survivors, driving Σ reserved > allocatable — the
/// #722/#795 over-reservation class on the EVAC leg, which #795's
/// `status == Idle` gate never covered (evac sessions are `Evacuating`).
/// With the drain step live, calm seeds 0, 4, 5, 7, 16, 21, 22 (and more
/// past 30) fire `placement-accounting`. Seed 0 is the SMALLEST firing seed
/// — the drain wave over-reserves host `…0d570000` (32768 MiB > allocatable
/// 24576) at step 240 under the faithful default.
///
/// The fix is RESERVED evac placement: the evac ctx now carries the
/// session's reserved 2D budget, `pick_for_session_reserved` drops the soft
/// fallback (honoring the hard bound), and an evac that fits no survivor
/// QUEUES (`Evacuating → Queued`, resume-origin) rather than binding a full
/// host — the queue scanner re-homes it once capacity returns (the #795
/// resume precedent, on the evac leg). FAIL-WITHOUT / PASS-WITH: forcing the
/// evac budget to `None` (reverting to the soft pick) FAILS this at step 240
/// with `placement-accounting`; with the reserved pick it PASSES the full
/// 1500-step window, drain-evacuated sessions queuing + re-homing through
/// quiescence.
#[test]
fn issue_800_reserved_evac_over_reservation_smallest_calm_seed() {
    run_faithful(0, Profile::Calm, 1500);
}

/// Quiescence oracle finding `quiescence-no-op-mint`: the drain's forced
/// host restart erased world sandboxes, but status-only stability accepted
/// Active/Parked rows before their third missing-sandbox strike settled.
/// Quiet rounds then released capacity and legitimately minted two CreateBoot
/// ops. Requiring the bound sandbox to exist on its up host keeps draining
/// through reconciliation before recording the op high-water mark.
#[test]
fn seed_33049837_quiescence_waits_for_missing_resident_reconcile() {
    run(33049837, Profile::Chaos, 5000);
}

/// The same `quiescence-no-op-mint` oracle bug with seven queued sessions
/// becoming placeable after the third missing-sandbox strike.
#[test]
fn seed_33058255_quiescence_waits_for_missing_resident_reconcile() {
    run(33058255, Profile::Chaos, 5000);
}

/// Nightly seed 33058131: a drain evict acknowledged its source-host
/// destroy, atomically detached the coordinator binding, and finished its
/// op while the host-side teardown effect was still deferred. The next
/// EvacResumer claim saw an unowned `Evacuating` row and restored a second
/// sandbox on a peer, violating ADR 0090 single ownership at step 79.
///
/// The fix keeps the outgoing sandbox bound through `Evacuating`. The
/// resumer re-issues the idempotent destroy and independently probes the
/// source; only confirmed absence permits the fenced binding clear and peer
/// restore. This full nightly-shape pin covers the 80-step firing and its
/// eventual convergence after the deferred host effect is delivered.
#[test]
fn seed_33058131_evacuation_waits_for_confirmed_source_teardown() {
    run(33058131, Profile::Chaos, 5000);
}

/// Issue #1271: nightly `attach-disagreement`, firing on ~10 chaos seeds
/// per nightly since the ADR 0108 E oracle landed (#952 collected the
/// first two weeks). `SimHostClient::restore` scheduled the captured-warm
/// harness dial at verb-ACK time while its create effect rode the
/// deferred queue; a dial due before delivery resolved against world
/// truth with no sandbox and died permanently, so the VM later
/// materialized running under an Active session with no harness and
/// nothing left to re-dial — a plain resume queues no outbox row, so
/// neither the A4 delivery remedy nor the A8 heartbeat recall ever
/// fires, and the zombie stands until the oracle's 30 s bound. In
/// production the state is unreachable: the harness lives INSIDE the
/// restored VM (its dial cannot precede the VM) and a failed dial
/// re-dials from the guest (ADR 0108 A1). Fix: the warm dial rides the
/// create effect's APPLICATION (`Effect::Create { warm_harness }`); the
/// hand-driven interleaving is pinned in ttft_attach.rs. This seed fired
/// at step 2190 (45 s unattached) and was the replay diagnosed
/// end-to-end.
#[test]
fn issue_1271_restore_dial_must_ride_the_deferred_create_effect() {
    run(33091230, Profile::Chaos, 5000);
}

/// #896 review (HIGH): a budget-exhausted evacuation deliberately leaves
/// the row Idle WITH its unconfirmed source binding — ownership is never
/// released on a guess. That residue must stay RECOVERABLE: the resume
/// verb's stale-binding gate re-issues the idempotent destroy, probes,
/// and only a confirmed-gone source authorizes the fenced clear + restore.
/// While the source may still be live the gate refuses (503, op retries)
/// rather than double-booting; pre-gate, the guarded bind 409'd forever
/// and the row was permanently un-resumable.
#[test]
fn issue_896_resume_gate_recovers_idle_row_with_retained_source_binding() {
    use engram_core::types::session::SessionState;
    use engram_dst::{DriverKind, Step};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(896, Profile::Calm).with_faithful_hosts();

        // Fleet up; one session booted to Active with a real bound VM.
        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        for h in 0..3 {
            sim.execute(Step::HostCheckpoint(h)).await;
        }
        let (sid, source_host, stale_sandbox) = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions.values().find_map(|r| {
                    (r.session.status == SessionState::Active).then_some((
                        r.session.id,
                        r.session.host_id?,
                        r.session.sandbox_id?,
                    ))
                })
            })
            .expect("one Active session with a bound sandbox after create");
        let source_idx = sim
            .world
            .host_ids
            .iter()
            .position(|h| *h == source_host)
            .expect("source host index");

        // The exhaustion residue, surgically: the row lands Idle with the
        // source binding retained while the source VM is still resident.
        sim.world.meta.with_db_mut(|db| {
            db.sessions
                .get_mut(&sid)
                .expect("session row")
                .session
                .status = SessionState::Idle;
        });

        let owned = |sim: &Sim| {
            let hosts = sim.world.host_world.hosts.lock();
            hosts
                .values()
                .flat_map(|h| h.sandboxes.values())
                .filter(|o| **o == Some(sid))
                .count()
        };
        assert_eq!(owned(&sim), 1, "the stale source VM is resident pre-resume");

        // Phase 1 — the source's teardown effect is DEFERRED (the exact
        // #883 shape): the gate's re-issued destroy queues, the probe still
        // answers alive, and the resume must REFUSE — binding intact, no
        // second VM, single ownership holds.
        sim.execute(Step::DeferHost(source_idx, true)).await;
        sim.execute(Step::ResumeSession).await;
        for _ in 0..3 {
            for r in 0..2 {
                sim.execute(Step::Driver(r, DriverKind::SessionOps)).await;
            }
            sim.execute(Step::AdvanceTime(std::time::Duration::from_secs(5)))
                .await;
        }
        let (status, binding) = sim.world.meta.with_db(|db| {
            let r = db.sessions.get(&sid).expect("session row");
            (r.session.status, r.session.sandbox_id)
        });
        assert_eq!(
            binding,
            Some(stale_sandbox),
            "an unconfirmable teardown must NOT release the source binding",
        );
        assert_ne!(
            status,
            SessionState::Active,
            "the resume must refuse while the source may still be live",
        );
        assert_eq!(
            owned(&sim),
            1,
            "single ownership must hold while the gate refuses (no double boot)",
        );

        // Phase 2 — the deferred teardown lands. The gate now confirms the
        // source gone, performs the fenced clear, and the restore proceeds.
        sim.execute(Step::DeferHost(source_idx, false)).await;
        sim.execute(Step::DeliverEffects).await;
        let mut resumed = false;
        for _ in 0..40 {
            for r in 0..2 {
                sim.execute(Step::Driver(r, DriverKind::SessionOps)).await;
            }
            sim.execute(Step::AdvanceTime(std::time::Duration::from_secs(5)))
                .await;
            assert!(
                owned(&sim) <= 1,
                "single ownership must hold at every step of the recovery",
            );
            let (status, binding) = sim.world.meta.with_db(|db| {
                let r = db.sessions.get(&sid).expect("session row");
                (r.session.status, r.session.sandbox_id)
            });
            if status == SessionState::Active {
                assert_ne!(
                    binding,
                    Some(stale_sandbox),
                    "the resumed session must ride a fresh sandbox, not the stale one",
                );
                resumed = true;
                break;
            }
        }
        assert!(
            resumed,
            "the Idle row with a retained (confirmed-dead) source binding must \
             be recoverable via /resume — the pre-gate 409-forever wedge",
        );
        let stale_present = sim
            .world
            .host_world
            .hosts
            .lock()
            .values()
            .any(|h| h.sandboxes.contains_key(&stale_sandbox));
        assert!(
            !stale_present,
            "the confirmed teardown removed the stale source VM",
        );
    });
}
