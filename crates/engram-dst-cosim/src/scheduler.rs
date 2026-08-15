//! The directed-scenario harness (ADR 0098 R-CoSim, rung 1).
//!
//! Rung 1 is DIRECTED, not a swarm: a scenario hand-drives an exact
//! interleaving of real coordinator drivers and real host steps over one
//! shared paused clock (the pinned-seed discipline). Every method here steps
//! a REAL driver / a REAL host flow — the harness only sequences them and
//! bridges the two facts the boundary needs (the eviction cursor a capture
//! is taken at, and the durable snapshot row a completed finalize lands, the
//! way prod's heartbeat reconcile does).
//!
//! The single standing oracle is [`Cosim::assert_idle_snapshot_durable`]:
//! *the coordinator reaching `Idle` implies the snapshot its resume path
//! will select is durable and at-or-above the eviction cursor* — the exact
//! property issue #570 violates.

use engram_core::types::BindingDisposition;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use engram_core::traits::metadata::{CreateDisposition, SessionCreateWriteSet};
use engram_core::traits::MetadataStore as _;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::session_op::OpKind;
use engram_core::{SandboxId, SessionId};

use crate::bridge::recoverable_snapshot_row;
use crate::world::{CosimWorld, COSIM_IMAGE};

/// The co-simulation harness.
pub struct Cosim {
    pub world: CosimWorld,
    idle_cfg: engram_coordinator::idle_detector::IdleDetectorConfig,
    queue_cfg: engram_coordinator::queue_scanner::QueueScannerConfig,
    /// The reconcile first-seen ledger (ADR 0116 A5: the age grace that
    /// replaced the strike debounce), owned across ticks like the real loop.
    first_seen: HashMap<SandboxId, std::time::Duration>,
    /// The sandbox each session was last bound to (captured before the D5
    /// unbind clears the coordinator's `sandbox_id`).
    session_sandbox: BTreeMap<SessionId, SandboxId>,
    /// The work cursor a session's eviction capture was taken at.
    evict_cursor: BTreeMap<SessionId, i64>,
    /// A human-readable step trace (for failure artifacts).
    pub trace: Vec<String>,
}

impl Cosim {
    pub async fn new(seed: u64) -> Self {
        Self::new_with_fault_plan(seed, None).await
    }

    /// A harness whose shared bucket injects faults per `plan` (see
    /// [`CosimWorld::new_with_fault_plan`]).
    pub async fn new_with_fault_plan(
        seed: u64,
        plan: Option<engram_testkit::storage::FaultPlan>,
    ) -> Self {
        Self {
            world: CosimWorld::new_with_fault_plan(seed, plan).await,
            idle_cfg: engram_coordinator::idle_detector::IdleDetectorConfig::default(),
            queue_cfg: engram_coordinator::queue_scanner::QueueScannerConfig::default(),
            first_seen: HashMap::new(),
            session_sandbox: BTreeMap::new(),
            evict_cursor: BTreeMap::new(),
            trace: Vec::new(),
        }
    }

    fn log(&mut self, s: impl Into<String>) {
        self.trace.push(s.into());
    }

    // ───────────────────────── time ─────────────────────────

    pub async fn advance(&mut self, secs: u64) {
        self.log(format!("advance {secs}s"));
        self.world.clock.advance(Duration::from_secs(secs)).await;
    }

    // ─────────────────────── coordinator drivers ───────────────────────

    /// Enqueue an op and drive it to a terminal (or requeued) row state
    /// INLINE on this task — NOT via the production `session_ops::enqueue`,
    /// which detaches the drive onto a `tokio::spawn`. The detached form is
    /// correct in prod (and fine in `engram-dst`, whose toy `SimHostClient`
    /// does zero I/O and settles in one poll), but here the REAL host flows
    /// perform real chunk-store / eviction-finalize I/O whose multi-poll
    /// futures starve inside a background task under a directed paused-clock
    /// harness. Driving `drive_claimed` inline runs the SAME verb body on
    /// this task, where the reactor drains it deterministically. The op row
    /// is the durable owner either way — this is purely where the compute
    /// runs.
    async fn enqueue_and_drive(
        &mut self,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
        key: &str,
    ) {
        match engram_coordinator::session_ops::enqueue_claim(
            &self.world.state,
            session_id,
            kind,
            payload,
            Some(key),
        )
        .await
        {
            Ok(engram_core::types::session_op::EnqueueOutcome::Claimed(op)) => {
                engram_coordinator::session_ops::drive_claimed(&self.world.state, op).await;
            }
            // Queued behind an in-flight op / duplicate: the settle loop
            // drives it.
            _ => self.drive_ops().await,
        }
    }

    /// The create handler's persistence shape (crib of `engram-dst`): reserve
    /// (Placed on the fitting host) + drive the create_boot op inline.
    pub async fn create_session(&mut self) -> SessionId {
        use engram_core::traits::Entropy as _;
        let session_id = SessionId::from(self.world.entropy.uuid());
        let ws = SessionCreateWriteSet {
            session_id,
            spec: SessionSpec {
                image: COSIM_IMAGE.into(),
                mode: SessionMode::DevVm,
            },
            mem_budget_mib: 2048,
            cpu_budget_vcpus: 2,
            sealed_secrets: None,
            capabilities: Vec::new(),
            integration_policy_json: None,
            runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(
                Vec::new(),
                None,
                None,
                Vec::new(),
            ),
            oauth_binding: None,
        };
        let candidates = vec![self.world.host_id];
        let disp = self
            .world
            .meta
            .reserve_and_persist_create(ws, &candidates, 0)
            .await;
        self.log(format!("create_session {session_id} -> {disp:?}"));
        if matches!(disp, Ok(CreateDisposition::Placed(_))) {
            let key = format!("create:{session_id}");
            self.enqueue_and_drive(session_id, OpKind::CreateBoot, serde_json::json!({}), &key)
                .await;
        }
        session_id
    }

    pub async fn queue_scanner(&mut self) {
        let _ =
            engram_coordinator::queue_scanner::run_once(&self.queue_cfg, &self.world.state).await;
    }

    pub async fn idle_detector(&mut self) {
        let _ =
            engram_coordinator::idle_detector::run_once(&self.idle_cfg, &self.world.state).await;
        self.log("idle_detector");
    }

    /// Drive every session with a due op (the SessionOps loop body).
    pub async fn drive_ops(&mut self) {
        let due = self.world.meta.op_due_sessions().await.unwrap_or_default();
        for sid in due {
            engram_coordinator::session_ops::drive_session(&self.world.state, sid).await;
        }
    }

    /// Create → boot a session all the way to `Active`, capturing its
    /// sandbox binding. The boot verb runs inline (see [`enqueue_and_drive`]).
    pub async fn boot_session(&mut self) -> SessionId {
        let session_id = self.create_session().await;
        // Settle any backed-off retry (the boot is single-pass on the happy
        // path).
        for _ in 0..8 {
            if self.session_state(session_id).await == Some(SessionState::Active) {
                break;
            }
            self.advance(2).await;
            self.drive_ops().await;
        }
        if let Some(sandbox) = self.sandbox_of(session_id).await {
            self.session_sandbox.insert(session_id, sandbox);
        }
        session_id
    }

    /// Nominate + evict a session to `Idle` via the REAL D5 path, recording
    /// the eviction cursor (captured from the host before the D5 unbind).
    ///
    /// The real `idle_detector::run_once` nominates (Active → Evicting); the
    /// real Evict VERB (`session_verbs::dispatch`, holding the D5 fast path)
    /// runs inline. The production `idle_evictor` scanner's only job is to
    /// enqueue that Evict op — which it does via the detached-spawn
    /// `session_ops::enqueue`; rung 1 enqueues+drives it inline instead (same
    /// verb body, same idempotency key shape) so the real capture I/O drains
    /// on this task.
    pub async fn evict_to_idle(&mut self, session_id: SessionId) {
        // Capture the sandbox + its work cursor NOW — the D5 fast path is
        // about to clear the coordinator's `sandbox_id`.
        let sandbox = self
            .sandbox_of(session_id)
            .await
            .or_else(|| self.session_sandbox.get(&session_id).copied())
            .expect("evicting session has a bound sandbox");
        self.session_sandbox.insert(session_id, sandbox);
        let cursor = self
            .world
            .host
            .lock()
            .await
            .cursor(sandbox)
            .expect("sandbox present at eviction");

        // The idle_detector's `nominate` does two things: `Active → Evicting`
        // AND `session_ops::enqueue(Evict)` — but that enqueue is the same
        // detached spawn that starves the real capture I/O here, so rung 1
        // applies the nomination transition directly and drives the Evict
        // verb inline below (identical to what the detector enqueues).
        let _ = self
            .world
            .meta
            .transition_session(
                session_id,
                SessionState::Evicting,
                BindingDisposition::Retain,
            )
            .await;
        let key = format!("evict:{session_id}");
        self.enqueue_and_drive(
            session_id,
            OpKind::Evict,
            serde_json::json!({ "target": "idle", "allow_park": true, "nominated": true }),
            &key,
        )
        .await;
        // Settle a backed-off capture retry. ADR 0101 C: the op leaves
        // the session honestly `Evicting` (capture landed, row not yet
        // recorded) — Idle arrives only when `finalize_pending` completes
        // the host-owned finalize and the settle flips it. Tests that
        // want Idle drive `finalize_pending` explicitly, exactly like
        // prod's upload + heartbeat cadence.
        for _ in 0..6 {
            let op_live = self
                .world
                .meta
                .op_running_for(session_id)
                .await
                .ok()
                .flatten()
                .is_some();
            if !op_live {
                break;
            }
            self.advance(1).await;
            self.drive_ops().await;
        }
        self.evict_cursor.insert(session_id, cursor);
        self.log(format!(
            "evict_to_idle {session_id} sandbox={sandbox} cursor={cursor} state={:?}",
            self.session_state(session_id).await
        ));
    }

    // ─────────────────────── host steps ───────────────────────

    /// A unit of guest work on the session's sandbox (advances the cursor).
    pub async fn guest_work(&mut self, session_id: SessionId, units: u32) {
        let Some(sandbox) = self.sandbox_of(session_id).await else {
            return;
        };
        let mut host = self.world.host.lock().await;
        for _ in 0..units {
            let _ = host.guest_write(sandbox).await;
        }
    }

    /// Record a periodic checkpoint at the session's current cursor (the
    /// host-driven recoverable snapshot row). This is the "prior checkpoint"
    /// a lost eviction snapshot falls back to (issue #570 scenario 1).
    pub async fn periodic_checkpoint(&mut self, session_id: SessionId) {
        let Some(sandbox) = self.sandbox_of(session_id).await else {
            return;
        };
        let (cursor, now, aux) = {
            let host = self.world.host.lock().await;
            (
                host.cursor(sandbox).unwrap_or(0),
                self.world.clock_now(),
                host.attachment_staging()
                    .into_iter()
                    .filter(|(id, _, _)| *id == sandbox)
                    .map(|(_, r, _)| r)
                    .collect::<Vec<_>>(),
            )
        };
        // ADR 0035 amendment: publish-before-record — the REAL snapshot
        // pipeline makes the pinned generations durable before the row
        // lands (HEAD-hit when startup publish already ran). A missing
        // generation fails the checkpoint here, exactly like prod's
        // "bundle publish: open staged … No such file" → chain poison.
        if !aux.is_empty() {
            if let Err(e) = {
                let host = self.world.host.lock().await;
                let store = engram_host_agent::bundles::BundleStore::new(
                    self.world.blob.clone(),
                    host.bundle_dir(),
                    crate::host::BUNDLE_EXT,
                );
                drop(host);
                store.publish(&aux).await
            } {
                self.log(format!(
                    "periodic_checkpoint {session_id} FAILED bundle publish: {e}"
                ));
                return;
            }
        }
        let _ = {
            let mut host = self.world.host.lock().await;
            // Firecracker's `prepare_save` queues TRANSPORT_RESET for every
            // guest vsock connection on periodic captures. The guest forgets
            // them. The stream wrapper gives the host reader EOF so it can
            // re-attach (ADR 0103).
            host.sever_exec_transports(sandbox);
            host.flush(sandbox).await
        };
        use engram_core::traits::Entropy as _;
        let snap = recoverable_snapshot_row(
            engram_core::SnapshotId::from(self.world.entropy.uuid()),
            session_id,
            self.world.host_id,
            COSIM_IMAGE,
            cursor,
            now,
            &aux,
        );
        let _ = self.world.meta.record_snapshot(snap).await;
        self.log(format!("periodic_checkpoint {session_id} cursor={cursor}"));
    }

    /// One teardown-reconcile tick over the REAL `reconcile_once`.
    /// `honor_capture_signal` = false replays the PRE-#570-fix reconcile
    /// (never consulted the capture-in-flight signal).
    pub async fn reconcile_tick(&mut self, honor_capture_signal: bool) {
        let backend = self.world.reconcile_backend(honor_capture_signal);
        let now = engram_core::traits::Clock::now_mono(&*self.world.clock);
        let _ = engram_host_agent::teardown_reconcile::reconcile_once(
            &backend,
            &*self.world.coord_plane,
            self.world.host_id,
            &mut self.first_seen,
            now,
        )
        .await;
        self.log(format!("reconcile_tick(honor={honor_capture_signal})"));
    }

    // ─────────── Rung 2: the NBD device-plane family (#784) ───────────

    /// A host-agent process ROLL: fresh generation, RAM disk backends drop, FC
    /// survivors stay resident. Devices keep their (now-dead-generation) kernel
    /// owner until the register-rehydrate re-serves or the sweep disconnects.
    pub async fn roll_host(&mut self) {
        self.world.host.lock().await.roll();
        self.log("roll_host (new generation; survivors resident)");
    }

    /// The register-time rehydrate sequence driven against the REAL coordinator
    /// listing (`register_rehydrate_list_core`): coord-list pass → local
    /// ChainHeadRecord pass → stale-binding sweep. `local_pass` = the #739
    /// defense-in-depth pass (safe default `true`; `false` is the adversarial
    /// ungated variant proving the un-pause gate is the last line).
    pub async fn register_rehydrate(&mut self, local_pass: bool) {
        let list = self
            .world
            .coord_plane
            .rehydrate_list(self.world.host_id)
            .await;
        let n = list.len();
        let res = self
            .world
            .host
            .lock()
            .await
            .register_rehydrate(&list, local_pass)
            .await;
        self.log(format!(
            "register_rehydrate(local_pass={local_pass}) coord_listed={n} -> {res:?}"
        ));
    }

    /// Model the VM genuinely departing (the host tears it down out of band) —
    /// after this the host's `probe_sandbox` reports it not-alive.
    pub async fn destroy_sandbox(&self, sandbox: SandboxId) {
        self.world.host.lock().await.destroy(sandbox);
    }

    /// Run the stale-binding sweep independently (REAL `sweep_verdict`).
    pub async fn stale_sweep(&mut self) {
        self.world.host.lock().await.stale_sweep_tick();
        self.log("stale_sweep");
    }

    // ─────────── ADR 0035 amendment: the bundle lifecycle ───────────

    /// The full heartbeat → pin-set → supervisor cycle (ADR 0035 §5 + the
    /// amendment): retry a failed startup publish (the 60 s timer's analog),
    /// PERSIST the host's stamp + per-sandbox attachments, THEN compute the
    /// ack's `live_bundles` from the REAL `bundle_pin_set`, then run the
    /// REAL materialize + sweep against it. The persist-before-pin-set
    /// ordering is the load-bearing one the incident rode. A pin-set
    /// failure withholds the ack — no sweep runs (prod: 5xx, host retries).
    pub async fn host_heartbeat(&mut self) {
        {
            let mut host = self.world.host.lock().await;
            if !host.stamp_published() {
                let _ = host.startup_publish().await;
            }
        }
        self.world.heartbeat().await;
        let live = match self.world.meta.bundle_pin_set().await {
            Ok(live) => live,
            Err(e) => {
                self.log(format!(
                    "host_heartbeat: pin set failed ({e}); ack withheld"
                ));
                return;
            }
        };
        let sweep = self.world.host.lock().await.bundle_sweep(&live).await;
        self.log(format!(
            "host_heartbeat live_pins={} sweep={:?}",
            live.len(),
            sweep.err()
        ));
    }

    /// A bundle-bake roll: the node-assets init stages a NEW stamp
    /// generation (the old one rotates out of `current.json`). `publish`
    /// runs the D1 startup publish; `false` replays the pre-amendment
    /// staged-but-never-durable world (the red knob — with it, a stamp
    /// rotation + sweep can destroy a running sandbox's only copy).
    pub async fn roll_bundle_stamp(&mut self, publish: bool) {
        let mut host = self.world.host.lock().await;
        host.stage_new_stamp_generation().await;
        let res = if publish {
            host.startup_publish().await
        } else {
            Ok(())
        };
        drop(host);
        self.log(format!("roll_bundle_stamp publish={publish} -> {res:?}"));
    }

    /// One REAL `run_one_bundle_sweep` (mark + promote) with ZERO grace —
    /// maximally adversarial: anything the pin set does not cover is
    /// reclaimable immediately, so a pin-set hole surfaces within two
    /// sweeps instead of hiding behind the 24 h production grace.
    pub async fn bundle_gc_sweep(&mut self) {
        let cfg = engram_coordinator::chunk_gc::ChunkGcConfig {
            grace_period: Duration::ZERO,
            ..Default::default()
        };
        let clock: std::sync::Arc<dyn engram_core::traits::Clock> = self.world.clock.clone();
        let report = engram_coordinator::bundle_gc::run_one_bundle_sweep(
            self.world.meta.clone() as std::sync::Arc<dyn engram_core::traits::MetadataStore>,
            self.world.blob.clone(),
            &cfg,
            engram_coordinator::chunk_gc::SweepMode::Full,
            &clock,
        )
        .await;
        self.log(format!("bundle_gc_sweep -> {report:?}"));
    }

    /// **Oracle — bundle reachability (ADR 0035 amendment; the 2026-08-10
    /// class):** every generation attached to a sandbox the host still
    /// holds is reachable from local staging ∪ blob storage (its next
    /// capture must be able to publish it; a restore must be able to
    /// materialize it), AND every recoverable snapshot row's pinned
    /// generations are durable in blob storage (publish-before-record —
    /// a pin nothing can satisfy must never be recorded). Checked after
    /// every swarm step.
    pub async fn assert_bundle_reachability(&self) -> Result<(), String> {
        use engram_core::types::sandbox::AuxRoDrive;
        let attachments = self.world.host.lock().await.attachment_staging();
        for (sandbox, r, staged) in attachments {
            if staged {
                continue;
            }
            let durable = self
                .world
                .blob
                .exists(&AuxRoDrive::blob_key(&r.sha256))
                .await
                .map_err(|e| format!("bundle-reachability: blob HEAD {}: {e}", r.sha256))?;
            if !durable {
                return Err(format!(
                    "bundle-reachability: sandbox {sandbox}'s attached generation \
                     {}/{} is staged NOWHERE and absent from blob storage — the \
                     2026-08-10 chain_poisoned class (its next capture cannot \
                     publish; a restore cannot materialize)",
                    r.drive_id, r.sha256
                ));
            }
        }
        let pinned: Vec<(engram_core::types::SnapshotId, String)> = self.world.meta.with_db(|db| {
            db.snapshots
                .values()
                .filter(|s| s.recoverable)
                .flat_map(|s| s.aux_bundles.iter().map(move |b| (s.id, b.sha256.clone())))
                .collect()
        });
        for (snapshot, sha) in pinned {
            let durable = self
                .world
                .blob
                .exists(&AuxRoDrive::blob_key(&sha))
                .await
                .map_err(|e| format!("bundle-reachability: blob HEAD {sha}: {e}"))?;
            if !durable {
                return Err(format!(
                    "bundle-reachability: recoverable snapshot {snapshot} pins \
                     generation {sha} which is absent from blob storage — a \
                     recorded pin nothing can satisfy (the restore on another \
                     host fails); publish-before-record was violated"
                ));
            }
        }
        Ok(())
    }

    /// Rung-2 PARK a session's sandbox (FC paused, VM resident, device served)
    /// — the 731df805 pre-condition.
    pub async fn park(&mut self, session_id: SessionId) {
        if let Some(sandbox) = self.resolve_sandbox(session_id).await {
            self.world.host.lock().await.park(sandbox);
            self.log(format!("park {session_id} sandbox={sandbox}"));
        }
    }

    /// Un-pause a session's sandbox over the REAL un-pause data-plane gate.
    /// Returns whether the guest un-paused (`false` = the gate fired onto an
    /// unserved plane — correct behavior, routed to recovery).
    pub async fn unpause(&mut self, session_id: SessionId) -> bool {
        let Some(sandbox) = self.resolve_sandbox(session_id).await else {
            return false;
        };
        let ok = self.world.host.lock().await.unpause(sandbox);
        self.log(format!("unpause {session_id} sandbox={sandbox} -> {ok}"));
        ok
    }

    /// Model the FC guest for a session genuinely dying (#806): it no longer
    /// holds its device node open, so a dead-owner sweep may legally DISCONNECT.
    pub async fn kill_guest(&mut self, session_id: SessionId) {
        if let Some(sandbox) = self.resolve_sandbox(session_id).await {
            self.world.host.lock().await.kill_guest(sandbox);
            self.log(format!("kill_guest {session_id} sandbox={sandbox}"));
        }
    }

    /// Wave 7b (#784 layer 2): lose a session's tracked records (#769 gap A) — a
    /// resident guest ends up holding a device no record accounts for, which the
    /// classification barrier must QUARANTINE (not skip, not sever).
    pub async fn lose_record(&mut self, session_id: SessionId) {
        if let Some(sandbox) = self.resolve_sandbox(session_id).await {
            self.world.host.lock().await.lose_record(sandbox);
            self.log(format!("lose_record {session_id} sandbox={sandbox}"));
        }
    }

    /// The ADR 0090 heartbeat advertise, against the REAL coordinator arm
    /// (`quarantined_survivor_advertise_core`): the host reports every
    /// quarantined survivor it still holds, exactly as prod's 5s heartbeat
    /// does, and the coordinator reacts by enqueueing the keyed quarantine
    /// `evict_local`. This is the seam the 2026-07-21 8174b7aa livelock
    /// lived in — pre-extraction the cosim wrote host liveness straight to
    /// the store and the advertise → enqueue → skip loop was invisible to
    /// it. Returns how many survivors were advertised (0 = the advertise
    /// source is gone; the loop is dead).
    pub async fn advertise_quarantined(&mut self) -> usize {
        let survivors: Vec<engram_protocol::heartbeat::QuarantinedSurvivor> = {
            let host = self.world.host.lock().await;
            host.quarantined_unknown()
                .into_iter()
                .filter_map(|sandbox_id| {
                    host.session_of(sandbox_id).map(|session_id| {
                        engram_protocol::heartbeat::QuarantinedSurvivor {
                            sandbox_id,
                            session_id,
                            // The cosim's survivors model the ADR 0090
                            // rehydrate flavor.
                            reason: Default::default(),
                        }
                    })
                })
                .collect()
        };
        engram_coordinator::api::host_http::quarantined_survivor_advertise_core(
            &self.world.state,
            self.world.host_id,
            &survivors,
        )
        .await;
        self.log(format!("advertise_quarantined n={}", survivors.len()));
        survivors.len()
    }

    /// Oracle read for op-quiescence (the 8174b7aa livelock class): the
    /// TOTAL `session_ops` rows ever created for a session. Under a fixed
    /// world state, repeated advertise/driver ticks must stop growing this —
    /// unbounded growth is the enqueue → fast-skip → re-enqueue signature
    /// (prod: ~43k rows in 2.5 days; the ADR 0093 423-row pileup is the
    /// same class).
    pub fn session_op_count(&self, session_id: SessionId) -> usize {
        self.world.meta.with_db(|db| {
            db.session_ops
                .values()
                .filter(|op| op.session_id == session_id)
                .count()
        })
    }

    /// The operator/runbook reconcile the `rehydrate-unknown-device` alert
    /// drives: restore the record of EVERY quarantined device so the drain's
    /// register re-serves it. Ensures bounded convergence — a quarantined slot
    /// reaches a terminal disposition rather than wedging forever.
    pub async fn reconcile_quarantined(&mut self) {
        let host = self.world.host.lock().await;
        let quarantined = host.quarantined_unknown();
        drop(host);
        if quarantined.is_empty() {
            return;
        }
        let mut host = self.world.host.lock().await;
        for id in &quarantined {
            host.regain_record(*id);
        }
        drop(host);
        self.log(format!("reconcile_quarantined n={}", quarantined.len()));
    }

    /// Quiescence heal (Wave 7b): restore EVERY device's record — the record-loss
    /// fault is healed globally at quiescence, exactly like the re-served
    /// survivors and drained finalizes, so convergence is required against a
    /// fully-healed world.
    pub async fn heal_all_records(&mut self) {
        self.world.host.lock().await.regain_all_records();
    }

    /// One tick of the coordinator's REAL `host_lost_straggler_sweep`
    /// (#782/#777, tombstone model since ADR 0116 A4): a bound HostLost
    /// row past the 60s min-age is entombed and settled; a SERVING VM
    /// is never destroyed by the coordinator (its tombstone owns it),
    /// a gone/not-alive one gets the inline belt destroy.
    pub async fn straggler_sweep_tick(&mut self) {
        let _ = engram_coordinator::dead_host::host_lost_straggler_sweep(&self.world.state).await;
        self.log("straggler_sweep_tick");
    }

    /// `try_claim` a spare NBD device (slot-accounting exercise).
    pub async fn slot_claim(&mut self) {
        self.world.host.lock().await.slot_claim().await;
    }

    /// Release the oldest held spare lease (slot-accounting exercise).
    pub async fn slot_populate_tick(&mut self) {
        self.world.host.lock().await.slot_populate_tick();
    }

    /// The sandbox a session is bound to — the coordinator row if present, else
    /// the harness's last-known binding (survives a D5 unbind / a HostLost
    /// clear).
    async fn resolve_sandbox(&self, session_id: SessionId) -> Option<SandboxId> {
        if let Some(sb) = self.sandbox_of(session_id).await {
            return Some(sb);
        }
        self.session_sandbox.get(&session_id).copied()
    }

    /// Drive one finalize attempt for every in-flight capture; on completion
    /// land the durable recoverable snapshot row (prod: the heartbeat
    /// reconcile).
    pub async fn finalize_pending(&mut self) {
        let sandboxes = self.world.host.lock().await.pending_finalize_sandboxes();
        for sandbox in sandboxes {
            let outcome = self.world.host.lock().await.finalize_tick(sandbox).await;
            if let crate::host::FinalizeTickOutcome::Completed {
                snapshot_id,
                session_id,
                cursor,
                aux_bundles,
                ..
            } = outcome
            {
                let snap = recoverable_snapshot_row(
                    snapshot_id,
                    session_id,
                    self.world.host_id,
                    COSIM_IMAGE,
                    cursor,
                    self.world.clock_now(),
                    &aux_bundles,
                );
                let _ = self.world.meta.record_snapshot(snap).await;
                // ADR 0101 C: the reconcile settle — the recoverable row
                // just landed, so the guarded `Evicting → Idle` + detach
                // flips now (the REAL store semantics; prod's heartbeat
                // HTTP glue is e2e territory). Before the floor flip the
                // D5 verb lied the session Idle at capture time.
                // The settle's lifecycle facts ride its transaction (the
                // crash-window fix) — the cosim drives the real store
                // semantics, so it passes them the same way the
                // reconcile does.
                let settle_events = [
                    ("evicted".to_string(), serde_json::json!({})),
                    (
                        "status_changed".to_string(),
                        serde_json::json!({"to": "idle"}),
                    ),
                ];
                let settled = self
                    .world
                    .meta
                    .settle_evicted_session_idle(session_id, sandbox, snapshot_id, &settle_events)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                self.log(format!(
                    "finalize completed {session_id} snapshot={snapshot_id} cursor={cursor} \
                     settled_idle={settled}"
                ));
            }
        }
    }

    /// Begin an eviction finalize on a bound sandbox directly (the REAL
    /// `snapshot_begin` capture leg) — for the capture-lock release pin.
    pub async fn begin_finalize(
        &mut self,
        sandbox: SandboxId,
    ) -> Result<engram_core::types::SnapshotId, String> {
        self.world.host.lock().await.snapshot_begin(sandbox).await
    }

    /// Delete the in-flight finalize's staging dir so every redrive fails and
    /// the finalize QUARANTINES (the capture-lock release pin, #784 rung 2).
    pub async fn sabotage_finalize_staging(&self, sandbox: SandboxId) {
        self.world
            .host
            .lock()
            .await
            .sabotage_finalize_staging(sandbox)
            .await;
    }

    /// Drive ONE finalize redrive attempt for a sandbox and return its outcome
    /// (the REAL `run_eviction_finalize_attempt`).
    pub async fn finalize_tick(&mut self, sandbox: SandboxId) -> crate::host::FinalizeTickOutcome {
        self.world.host.lock().await.finalize_tick(sandbox).await
    }

    /// Resume an `Idle` session back to `Active` (the real Resume op).
    pub async fn resume_session(&mut self, session_id: SessionId) {
        let key = format!("resume:{session_id}");
        self.enqueue_and_drive(session_id, OpKind::Resume, serde_json::json!({}), &key)
            .await;
        // A resume from Idle may route through Queued (placement); the real
        // queue_scanner places it, then its boot op drives to Active.
        for _ in 0..12 {
            if self.session_state(session_id).await == Some(SessionState::Active) {
                break;
            }
            self.queue_scanner().await;
            self.drive_ops().await;
            self.advance(2).await;
        }
        if let Some(sandbox) = self.sandbox_of(session_id).await {
            self.session_sandbox.insert(session_id, sandbox);
        }
        self.log(format!(
            "resume_session {session_id} -> {:?}",
            self.session_state(session_id).await
        ));
    }

    // ─────────────────────── the oracle ───────────────────────

    /// The boundary oracle (issue #570): every session the coordinator drove
    /// to `Idle` via eviction must have a recoverable snapshot at-or-above
    /// the cursor its capture was taken at. A stale-or-missing snapshot means
    /// the resume path will rewind past completed work.
    pub fn assert_idle_snapshot_durable(&self, session_id: SessionId) -> Result<(), String> {
        let Some(&evicted_at) = self.evict_cursor.get(&session_id) else {
            return Ok(()); // never evicted — nothing to assert
        };
        let newest = self.world.newest_recoverable_cursor(session_id);
        match newest {
            Some(c) if c >= evicted_at => Ok(()),
            other => Err(format!(
                "issue #570: session {session_id} reached Idle with its eviction capture at \
                 cursor {evicted_at}, but the newest recoverable snapshot the resume path \
                 will select is {other:?} — the eviction snapshot was lost; resume rewinds \
                 to a stale checkpoint"
            )),
        }
    }

    /// Publish a live disk manifest from the host to the coordinator, via
    /// the REAL `CoordControlPlane` bridge (→ `live_manifest_publish_core`).
    /// This is the host→coordinator survivor-publish the flush/finalize legs
    /// make; the returned outcome is what the host reads back over the wire.
    pub async fn publish_manifest(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        manifest_id: uuid::Uuid,
        version: u64,
    ) -> engram_host_core::LiveManifestPublishOutcome {
        use engram_host_core::CoordControlPlane as _;
        let req = engram_host_core::LiveManifestPublishRequest {
            session_id,
            sandbox_id,
            manifest_id,
            manifest_version: version,
        };
        self.world
            .coord_plane
            .publish_live_manifest(self.world.host_id, &req)
            .await
            .expect("publish reaches the coordinator")
            .outcome
    }

    /// The session's persisted live disk manifest ref (the durable survivor
    /// pointer the publish updates).
    pub fn live_disk_manifest(
        &self,
        session_id: SessionId,
    ) -> Option<engram_core::types::manifest::ManifestRef> {
        self.world.meta.with_db(|db| {
            db.sessions
                .get(&session_id)
                .and_then(|r| r.session.live_disk_manifest)
        })
    }

    /// Is an eviction finalize (capture lock) in flight for a sandbox?
    pub async fn capture_in_flight(&self, sandbox: SandboxId) -> bool {
        self.world.host.lock().await.capture_in_flight(sandbox)
    }

    /// The sandboxes teardown-reconcile locally destroyed (oracle memory).
    pub fn reconcile_destroys(&self) -> Vec<SandboxId> {
        self.world.view.destroyed()
    }

    /// The local bindings teardown-reconcile repaired (oracle memory).
    pub fn reconcile_binding_repairs(&self) -> Vec<(SandboxId, SessionId)> {
        self.world.view.binding_repairs()
    }

    /// Drop the host's in-RAM local binding for a sandbox (the post-roll
    /// survivor: its `PooledBackend` binding table died with the process).
    pub fn drop_local_binding(&self, sandbox: SandboxId) {
        self.world.view.drop_binding(sandbox);
    }

    // ─────────── Rung 2 device-plane reads + oracles (#784) ───────────

    /// Is a session's sandbox parked (rung-2 evicting-shaped)?
    pub async fn is_parked(&self, session_id: SessionId) -> bool {
        match self.resolve_sandbox(session_id).await {
            Some(sb) => self.world.host.lock().await.is_parked(sb),
            None => false,
        }
    }

    /// Is a session's sandbox device served by the current host generation?
    pub async fn served_by_current(&self, session_id: SessionId) -> bool {
        match self.resolve_sandbox(session_id).await {
            Some(sb) => self.world.host.lock().await.served_by_current(sb),
            None => false,
        }
    }

    /// Sandboxes the sweep PARKed because a live guest held the device (#806).
    pub async fn sweep_parked_live(&self) -> Vec<SandboxId> {
        self.world.host.lock().await.sweep_parked_live()
    }

    /// Sandboxes the classification barrier put in `QuarantinedUnknown` — a
    /// record-invisible live survivor (#769 gap A). CLASSIFIED, never skipped.
    pub async fn quarantined_unknown(&self) -> Vec<SandboxId> {
        self.world.host.lock().await.quarantined_unknown()
    }

    /// **Oracle — severed-live-holder (#806):** no device is left both
    /// UNSERVED and UNBOUND (`kernel_owner == None`) while a live guest still
    /// holds it open. A device in that state was severed out from under a
    /// reading guest — the 731df805 EIO-on-live-guest corruption. Checked over
    /// every device slot each quiescence.
    pub async fn assert_no_severed_live_holder(&self) -> Result<(), String> {
        let host = self.world.host.lock().await;
        for id in host.device_slot_ids() {
            let live = host.guest_holds_device(id);
            let served = host.served_by_current(id);
            let bound = host.kernel_owner(id).is_some();
            if live && !served && !bound {
                return Err(format!(
                    "severed-live-holder (#806): sandbox {id}'s device is unserved AND unbound \
                     (kernel_owner=None) while its guest still holds it open — a live guest's \
                     data plane was severed (the 731df805 EIO class)"
                ));
            }
        }
        Ok(())
    }

    /// **Oracle — record-invisible-survivor-classified (Wave 7b, #784 layers
    /// 2–3):** every current record-invisible resident survivor (live, unserved,
    /// dead-owner, no record — the #769 gap-A precondition) that has been through
    /// a register/sweep MUST have been CLASSIFIED `QuarantinedUnknown`, not
    /// silently skipped. We assert the barrier's promise directly: any such
    /// survivor whose device is UNBOUND (kernel_owner cleared) is a severed
    /// classification failure, and any quarantined device must stay
    /// RECONNECTABLE. Checked every step. (The strong revert-detecting proof is
    /// the directed `gap_a_*` pin; this guards the swarm-scale invariant.)
    pub async fn assert_quarantine_reconnectable(&self) -> Result<(), String> {
        let host = self.world.host.lock().await;
        for id in host.quarantined_unknown() {
            // A quarantined device is served (recovered) OR reconnectable
            // (kernel-bound). Never both unserved AND unbound while guest-held.
            let served = host.served_by_current(id);
            let bound = host.kernel_owner(id).is_some();
            let live = host.guest_holds_device(id);
            if !served && live && !bound {
                return Err(format!(
                    "quarantine-reconnectable (#784): sandbox {id} was classified \
                     QuarantinedUnknown but is now unserved, kernel-unbound, and still \
                     guest-held — a quarantined survivor was severed, not left reconnectable"
                ));
            }
        }
        Ok(())
    }

    /// **Oracle — slot-accounting (ADR 0098 P7 oracle #3):** the real
    /// allocator's `free + warm + held == capacity` identity holds (no leaked
    /// or double-counted `/dev/nbdN` slot).
    pub async fn assert_slot_accounting(&self) -> Result<(), String> {
        // Let any pending async lease-release settle before reading counters.
        tokio::task::yield_now().await;
        let (free, warm, held, capacity) = self.world.host.lock().await.slot_accounting().await;
        if free + warm + held != capacity {
            return Err(format!(
                "slot-accounting: free({free}) + warm({warm}) + held({held}) != capacity({capacity})"
            ));
        }
        Ok(())
    }

    /// **Oracle — cross-boundary ownership agreement (the split-brain guard):**
    /// for every device this host generation SERVES that is bound to a
    /// NON-TERMINAL session, the coordinator must agree it still owns the
    /// session→sandbox (`sandbox_ownership_core`). A served plane whose live
    /// session the coordinator has RE-HOMED to a different binding is the split
    /// — the host would keep flushing a plane another binding now owns.
    ///
    /// A TERMINAL session's still-served device is deliberately NOT flagged
    /// here: the coordinator ended the session (it was not re-homed), and the
    /// device is a teardown-in-progress leftover the reconcile reaps. That
    /// window is legitimate and transient; its COMPLETION is guaranteed
    /// separately by [`assert_teardown_complete`](Self::assert_teardown_complete)
    /// at quiescence. Scoping the split-brain check to non-terminal sessions
    /// matches the invariant's intent (re-home, not teardown) — it does not
    /// weaken it (RCA'd from the swarm's first firing, seed 7: a `ForceTerminal`
    /// step left a served device before the reconcile reap ran).
    pub async fn assert_ownership_agreement(&self) -> Result<(), String> {
        // Only ACTIVELY-serving planes (a live RAM backend) can double-serve /
        // race a re-home. A paused (capture-in-flight) or post-roll device with
        // no live backend is mid-lifecycle-transition — its teardown/rehydrate
        // is checked at quiescence, not flagged here.
        let active: Vec<SandboxId> = {
            let host = self.world.host.lock().await;
            host.device_slot_ids()
                .into_iter()
                .filter(|id| host.served_by_current(*id) && host.has_live_backend(*id))
                .collect()
        };
        for sandbox in active {
            let Some(session) = self.session_binding(sandbox) else {
                continue; // a served device with no known session — freshly created, unbound
            };
            // A terminal session's device is teardown-in-progress (checked at
            // quiescence), not a re-home split-brain.
            if self
                .session_state(session)
                .await
                .is_some_and(|s| s.is_terminal())
            {
                continue;
            }
            let owned = engram_coordinator::api::host_http::sandbox_ownership_core(
                &self.world.state,
                session,
                sandbox,
            )
            .await
            .unwrap_or(false);
            if !owned {
                // ADR 0116 A-D5: a disowned-but-served sandbox with an
                // OUTSTANDING TOMBSTONE is a legal converging transient,
                // not split-brain — the coordinator has explicitly
                // obligated the host to destroy it, and the next
                // heartbeat consumes + acks. Only an un-entombed
                // disagreement is the re-home split-brain this flags.
                let entombed = self
                    .world
                    .meta
                    .sandbox_tombstones_for_host(self.world.host_id)
                    .await
                    .unwrap_or_default()
                    .contains(&sandbox);
                if entombed {
                    continue;
                }
                return Err(format!(
                    "ownership-agreement: host ACTIVELY serves sandbox {sandbox} (live backend) \
                     bound to NON-TERMINAL session {session}, but the coordinator disowns it \
                     with NO tombstone recorded — a served plane the coordinator has re-homed \
                     (split-brain)"
                ));
            }
        }
        Ok(())
    }

    /// **Oracle — teardown / ownership completeness (quiescence only, the STRONG
    /// form):** after the reconcile + finalize + straggler drain, EVERY device
    /// the host still serves for a known session must be OWNED by the
    /// coordinator. Transiently a leftover lingers — a terminal session's device
    /// awaiting its reconcile reap, an evicted sandbox mid-finalize (coord
    /// binding cleared by D5), a re-home window — and the every-step guard
    /// exempts those non-actively-serving cases. But by quiescence every such
    /// leftover MUST be resolved (the finalize destroyed it, the reconcile
    /// reaped it, or the coord re-bound it); a served-yet-disowned device at
    /// quiescence is a teardown-liveness hole (a plane that never converges).
    /// This is the strong closure the every-step guard's exemptions rely on.
    pub async fn assert_teardown_complete(&self) -> Result<(), String> {
        let served: Vec<SandboxId> = {
            let host = self.world.host.lock().await;
            host.device_slot_ids()
                .into_iter()
                .filter(|id| host.served_by_current(*id))
                .collect()
        };
        for sandbox in served {
            let Some(session) = self.session_binding(sandbox) else {
                continue;
            };
            let owned = engram_coordinator::api::host_http::sandbox_ownership_core(
                &self.world.state,
                session,
                sandbox,
            )
            .await
            .unwrap_or(false);
            if !owned {
                return Err(format!(
                    "teardown-complete: at quiescence the host still SERVES sandbox {sandbox} \
                     bound to session {session}, but the coordinator disowns it — a leftover plane \
                     the reconcile/finalize drain never converged (a teardown-liveness hole)"
                ));
            }
        }
        Ok(())
    }

    /// The harness's last-known session for a sandbox (survives a D5 unbind).
    fn session_binding(&self, sandbox: SandboxId) -> Option<SessionId> {
        self.session_sandbox
            .iter()
            .find(|(_, sb)| **sb == sandbox)
            .map(|(s, _)| *s)
    }

    /// Force a session state transition (models the dead-host detector
    /// flipping a survivor to `HostLost` without going through the op path).
    pub async fn force_session_state(
        &self,
        session_id: SessionId,
        to: SessionState,
        disposition: BindingDisposition,
    ) {
        let _ = self
            .world
            .meta
            .transition_session(session_id, to, disposition)
            .await;
    }

    /// The eviction cursor recorded for a session (the capture point).
    pub fn evict_cursor(&self, session_id: SessionId) -> Option<i64> {
        self.evict_cursor.get(&session_id).copied()
    }

    /// The newest recoverable snapshot cursor the resume path would select.
    pub fn newest_recoverable_cursor(&self, session_id: SessionId) -> Option<i64> {
        self.world.newest_recoverable_cursor(session_id)
    }

    // ─────────────────────── coordinator reads ───────────────────────

    pub async fn session_state(&self, session_id: SessionId) -> Option<SessionState> {
        self.world
            .meta
            .with_db(|db| db.sessions.get(&session_id).map(|r| r.session.status))
    }

    /// The coordinator's currently-bound sandbox for a session.
    pub async fn sandbox_of(&self, session_id: SessionId) -> Option<SandboxId> {
        self.world.meta.with_db(|db| {
            db.sessions
                .get(&session_id)
                .and_then(|r| r.session.sandbox_id)
        })
    }
}
