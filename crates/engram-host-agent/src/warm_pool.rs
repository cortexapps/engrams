//! ADR 0014 M1.6: per-host warm-pool driver.
//!
//! Holds a free-list of pre-restored Firecracker microVMs per
//! [`TemplateRef`]. Each entry is a sandbox that has been
//! `restore_from_snapshot`d from the template's bake-time portable
//! snapshot (state.bin + sidecar + memory chunks) and is currently
//! paused with bootstrap awaiting a `BootstrapLaunch` frame on
//! vsock.
//!
//! ## Lifecycle
//!
//! - **Observe**: heartbeat-response (M1.7) hands the pool the
//!   coord's current active-template set. Templates that drop off
//!   the set get a 60s grace before their free-list drains.
//! - **Refill**: when a template's free-list dips below `target`,
//!   a background task restores a fresh microVM and pushes the
//!   resulting `SandboxId` onto the list. Concurrency is bounded
//!   per template (one refill at a time) to avoid thundering
//!   herd against BlobStorage.
//! - **Lease**: atomic `Vec::pop` from the free-list. Returns
//!   `Granted(sandbox_id)` on success, `NoCapacity` if empty,
//!   or `Stale{current_ref}` when the requested ref isn't in the
//!   known-templates map (coord cache lag). Triggers a refill
//!   spawn.
//! - **Launch**: the activation step — coord calls
//!   [`WarmPool::launch`] with the per-session agent + egress
//!   policy; this delegates to `SandboxBackend::start_agent`,
//!   which applies the policy and writes the `BootstrapLaunch`
//!   frame to the in-guest bootstrap supervisor.
//!
//! For M1.6 the autoscaler target is fixed at N=1 per template;
//! M1.9 introduces lease-rate-driven scaling.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use engram_core::error::SandboxError;
use engram_core::traits::host_client::{WarmLeaseOutcome, WarmSlotCount};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::ids::TemplateRef;
use engram_core::types::sandbox::AgentSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::template::TemplateRecord;
use engram_core::SandboxId;
use parking_lot::Mutex;

/// Floor target for any known template — the warm pool keeps at
/// least this many slots ready when the autoscaler observes any
/// recent lease activity. v1 floors at 1.
const FLOOR_TARGET: u32 = 1;

/// Ceiling on per-template target. v1 caps at 1 because N>1
/// concurrent restores from one snapshot collide on the
/// source-sandbox-id-keyed vsock UDS path (FC's state.bin embeds
/// it; two FCs can't bind the same Unix socket). ADR 0014 calls
/// out per-FC mount-namespace + bind-mount as the unblocker;
/// when that lands, raise the ceiling to N=4 (or higher,
/// depending on the warm_pool_memory bench result).
///
/// This means M1's warm pool today is effectively N=1 per
/// template per host — a slot exists or it doesn't. The
/// autoscaler tracks lease rate so it stays at FLOOR_TARGET=1
/// when leases happen and drains to 0 in the cold tail. Raising
/// this constant is the v2 unlock.
const CEILING_TARGET: u32 = 1;

/// Window over which lease rate is computed for autoscaler input.
/// 5 minutes keeps the signal stable across bursty session-create
/// patterns while still reacting within one batch.
const AUTOSCALE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Time to spawn one refill end-to-end (download blob + restore
/// FC). Multiplied by lease-rate to compute the in-flight target
/// the pool needs to keep up. Conservative; M1.10's bench will
/// give us a measured value.
const REFILL_TIME_SECS: f64 = 5.0;

/// Headroom multiplier on the computed target (1.2 = 20% slack)
/// so bursts past the steady rate don't immediately starve the
/// pool.
const AUTOSCALE_HEADROOM: f64 = 1.2;

/// How long a template can go without a lease before the
/// autoscaler floors it back to 0 (drained). 30 min matches the
/// ADR 0014 plan; the pool's STALE_GRACE handles the eventual
/// teardown.
const COLD_TAIL: Duration = Duration::from_secs(30 * 60);

/// How long an inactive template's free-list keeps existing
/// sandboxes alive after the coord drops it from the active set.
/// During this window, in-flight leases against the old ref can
/// still succeed; after, the slots are destroyed and the entry
/// removed.
const STALE_GRACE: Duration = Duration::from_secs(60);

/// Per-host warm-pool state. Cheap to clone (one inner Arc).
#[derive(Clone)]
pub struct WarmPool {
    inner: Arc<WarmPoolInner>,
}

struct WarmPoolInner {
    /// Free-list per template_ref. `Vec::pop` is the lease primitive.
    free_lists: DashMap<TemplateRef, Mutex<Vec<SandboxId>>>,
    /// Most recently observed (TemplateRecord + SnapshotMetadata)
    /// per template_ref. The refill loop reads SnapshotMetadata
    /// from here to drive `backend.restore`.
    known: DashMap<TemplateRef, KnownTemplate>,
    /// Autoscaler target per template (M1.9). Bounded by
    /// `FLOOR_TARGET`..`CEILING_TARGET`; recomputed from lease
    /// history on every gc_tick.
    targets: DashMap<TemplateRef, u32>,
    /// Per-template lease history — `Instant` of each successful
    /// `lease()` over the `AUTOSCALE_WINDOW`. Older entries are
    /// trimmed on gc_tick.
    lease_history: DashMap<TemplateRef, Mutex<Vec<std::time::Instant>>>,
    /// Per-template race guard against concurrent refills. Claimed
    /// atomically in `maybe_refill` via the DashMap `Entry::Vacant`
    /// → `slot.insert(())` pattern (the shard's write lock spans
    /// the match arm, so the test-and-set is atomic). Without this,
    /// multiple `gc_tick` calls or a `lease` + `gc_tick` pair would
    /// each spawn their own `backend.restore` task, and both would
    /// try to bind the same vsock UDS path embedded in `state.bin`
    /// → EADDRINUSE on the second. Removed by the spawn callback
    /// before pushing to the free-list, so the next gc_tick can
    /// refill if the slot was consumed in the meantime.
    inflight_refills: DashMap<TemplateRef, ()>,
    /// Reference back to the host's SandboxBackend (typically a
    /// PooledBackend wrapping FirecrackerBackend). Used by the
    /// refill loop for `restore` and by `launch` for
    /// `start_agent` / `destroy`.
    backend: Arc<dyn SandboxBackend>,
}

struct KnownTemplate {
    record: TemplateRecord,
    metadata: SnapshotMetadata,
    /// When the coord most recently said this template was active.
    /// `None` means the template is currently active in the coord's
    /// set; `Some(t)` means we dropped it from active at time `t`
    /// and the grace window started.
    inactive_since: Option<std::time::Instant>,
}

impl WarmPool {
    pub fn new(backend: Arc<dyn SandboxBackend>) -> Self {
        Self {
            inner: Arc::new(WarmPoolInner {
                free_lists: DashMap::new(),
                known: DashMap::new(),
                targets: DashMap::new(),
                lease_history: DashMap::new(),
                inflight_refills: DashMap::new(),
                backend,
            }),
        }
    }

    /// Refresh the host's view of the active template set, called
    /// from the heartbeat-response handler. `templates` is the
    /// coord's authoritative list. Returns a (gained, dropped)
    /// tuple — handy for log lines but the side effects are what
    /// matters.
    ///
    /// Side effects:
    /// - Newly-active templates land in `known` and the refill
    ///   loop fires for each.
    /// - Templates absent from the new list flip to `inactive_since
    ///   = Some(now)`; their free-lists keep existing entries until
    ///   the grace window expires (handled by the gc tick).
    /// - Templates whose snapshot_id changed (rebake) flush their
    ///   free-list — the old slots reference a stale snapshot and
    ///   need to be destroyed.
    pub async fn observe_templates(&self, templates: Vec<(TemplateRecord, SnapshotMetadata)>) {
        let now = std::time::Instant::now();
        let mut seen = std::collections::HashSet::new();
        for (record, metadata) in templates {
            seen.insert(record.template_ref);
            self.upsert_known(record, metadata).await;
        }
        // Templates in `known` but not in `seen` — flip to inactive.
        for mut entry in self.inner.known.iter_mut() {
            if !seen.contains(entry.key()) && entry.inactive_since.is_none() {
                entry.inactive_since = Some(now);
            }
        }
        // Spawn refill for active templates that need it.
        for entry in self.inner.known.iter() {
            if entry.inactive_since.is_some() {
                continue;
            }
            self.maybe_refill(*entry.key()).await;
        }
    }

    async fn upsert_known(&self, record: TemplateRecord, metadata: SnapshotMetadata) {
        let template_ref = record.template_ref;
        let snapshot_id = record.snapshot_id;
        let mut entry = self
            .inner
            .known
            .entry(template_ref)
            .or_insert_with(|| KnownTemplate {
                record: record.clone(),
                metadata: metadata.clone(),
                inactive_since: None,
            });
        // If the snapshot_id changed (rebake), the existing free-list
        // entries are stale — drain them. Drop the lock before
        // calling destroy on each.
        let stale_drain = if entry.record.snapshot_id != snapshot_id {
            entry.record = record;
            entry.metadata = metadata;
            entry.inactive_since = None;
            self.inner
                .free_lists
                .get(&template_ref)
                .map(|m| std::mem::take(&mut *m.lock()))
                .unwrap_or_default()
        } else {
            // Re-arm the active flag — coord said this template is
            // current again. May be redundant on a hot loop; cheap.
            entry.inactive_since = None;
            Vec::new()
        };
        drop(entry);
        self.inner
            .targets
            .entry(template_ref)
            .or_insert(FLOOR_TARGET);
        for sandbox_id in stale_drain {
            let _ = self.inner.backend.destroy(sandbox_id).await;
        }
    }

    /// Atomic take from the free-list for `template_ref`. Three
    /// outcomes per the [`WarmLeaseOutcome`] discriminant. Schedules
    /// a refill in the background on success or empty so the pool
    /// returns to target.
    pub async fn lease(&self, template_ref: TemplateRef) -> WarmLeaseOutcome {
        let phase_start = std::time::Instant::now();
        // Stale template: coord asked about a ref we don't know
        // about. Return the most recent known ref so the coord can
        // refresh its cache. If no ref is known at all, NoCapacity.
        if !self.inner.known.contains_key(&template_ref) {
            // No exact match — see if we have any active known to
            // hint at. Picking the first one isn't great heuristic
            // but the coord will re-resolve anyway.
            let hint = self
                .inner
                .known
                .iter()
                .find(|e| e.inactive_since.is_none())
                .map(|e| *e.key());
            let outcome = match hint {
                Some(current_ref) => WarmLeaseOutcome::Stale { current_ref },
                None => WarmLeaseOutcome::NoCapacity,
            };
            metrics::histogram!(
                crate::metrics::SANDBOX_BOOT_SECONDS,
                "phase" => "warm_lease",
                "outcome" => "no_capacity",
                "kind" => "warm",
            )
            .record(phase_start.elapsed().as_secs_f64());
            return outcome;
        }
        let leased = self
            .inner
            .free_lists
            .get(&template_ref)
            .and_then(|m| m.lock().pop());
        // ADR 0014 M1.9: every lease attempt (success or empty)
        // counts as demand for the autoscaler. Recording on both
        // outcomes lets the pool scale UP under load even when
        // it's being out-paced — empty leases are exactly the
        // signal that target is too low.
        self.record_lease_demand(template_ref);
        // Whether we leased one or not, refill toward the target
        // so the next lease finds a slot. Spawned to avoid blocking
        // the caller (coord's parallel-ask path).
        self.maybe_refill(template_ref).await;
        let lease_outcome = match leased {
            Some(sandbox_id) => WarmLeaseOutcome::Granted(sandbox_id),
            None => WarmLeaseOutcome::NoCapacity,
        };
        let outcome_label = match &lease_outcome {
            WarmLeaseOutcome::Granted(_) => "success",
            WarmLeaseOutcome::NoCapacity => "no_capacity",
            WarmLeaseOutcome::Stale { .. } => "stale",
        };
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "warm_lease",
            "outcome" => outcome_label,
            "kind" => "warm",
        )
        .record(phase_start.elapsed().as_secs_f64());
        lease_outcome
    }

    /// Append a demand timestamp to the per-template lease history
    /// (M1.9 autoscaler input). Trimming happens on `gc_tick`.
    fn record_lease_demand(&self, template_ref: TemplateRef) {
        let now = std::time::Instant::now();
        self.inner
            .lease_history
            .entry(template_ref)
            .or_default()
            .lock()
            .push(now);
    }

    /// Activate a previously-leased warm sandbox: push
    /// BootstrapLaunch + apply egress policy. Pre-restored substrate
    /// is already running; this is the per-session activation step.
    pub async fn launch(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        // ADR 0014 ordering: policy onto the proxy registry BEFORE
        // bootstrap exec's the agent. Same invariant ADR 0013
        // codified for cold-create's StartAgent.
        let phase_start = std::time::Instant::now();
        let result = async {
            self.inner.backend.notify_session_policy(policy).await?;
            self.inner.backend.start_agent(sandbox_id, agent).await
        }
        .await;
        let outcome = match &result {
            Ok(_) => "success",
            Err(_) => "fc_error",
        };
        // `agent_handshake` on the warm path is sub-100ms in the
        // happy case — bootstrap is already accept()'ing on the
        // restored microVM, so the vsock CONNECT returns
        // immediately. Compare against the cold-path emission at
        // `grpc_server::start_agent` to see the snapshot-restore
        // win.
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "agent_handshake",
            "outcome" => outcome,
            "kind" => "warm",
        )
        .record(phase_start.elapsed().as_secs_f64());
        result
    }

    /// Snapshot of per-template inventory (free-list size + target).
    /// Heartbeat (M1.7) ships this to coord; ops queries it via
    /// `ListWarmSlots`.
    pub fn list_slots(&self) -> Vec<WarmSlotCount> {
        let mut out = Vec::with_capacity(self.inner.free_lists.len());
        for entry in self.inner.free_lists.iter() {
            let template_ref = *entry.key();
            let available = entry.value().lock().len() as u32;
            let target = self
                .inner
                .targets
                .get(&template_ref)
                .map(|t| *t)
                .unwrap_or(FLOOR_TARGET);
            out.push(WarmSlotCount {
                template_ref,
                available,
                target,
            });
        }
        out
    }

    /// Tick called by a periodic timer (every ~5s) to:
    /// - Recompute per-template autoscaler targets from the
    ///   recent lease history.
    /// - Garbage-collect inactive templates past their grace window.
    /// - Top up free-lists toward their target.
    pub async fn gc_tick(&self) {
        let now = std::time::Instant::now();
        // ADR 0014 M1.9: trim per-template lease history to the
        // autoscaler window and recompute each target.
        for entry in self.inner.lease_history.iter() {
            let cutoff = now - AUTOSCALE_WINDOW;
            entry.value().lock().retain(|t| *t >= cutoff);
        }
        for entry in self.inner.known.iter() {
            let template_ref = *entry.key();
            let new_target = self.compute_target(template_ref, now);
            self.inner.targets.insert(template_ref, new_target);
        }
        // GC inactive templates past their grace window.
        let mut to_remove = Vec::new();
        for entry in self.inner.known.iter() {
            if let Some(t) = entry.inactive_since {
                if now.duration_since(t) >= STALE_GRACE {
                    to_remove.push(*entry.key());
                }
            }
        }
        for template_ref in to_remove {
            let to_destroy = self
                .inner
                .free_lists
                .remove(&template_ref)
                .map(|(_, m)| m.into_inner())
                .unwrap_or_default();
            self.inner.known.remove(&template_ref);
            self.inner.targets.remove(&template_ref);
            self.inner.lease_history.remove(&template_ref);
            for sandbox_id in to_destroy {
                let _ = self.inner.backend.destroy(sandbox_id).await;
            }
        }
        // Drain free-lists for templates whose target went to 0.
        // (Cold tail: no leases observed in COLD_TAIL.)
        for entry in self.inner.targets.iter() {
            let template_ref = *entry.key();
            let target = *entry.value();
            if target > 0 {
                continue;
            }
            let to_destroy = self
                .inner
                .free_lists
                .get(&template_ref)
                .map(|m| std::mem::take(&mut *m.lock()))
                .unwrap_or_default();
            for sandbox_id in to_destroy {
                let _ = self.inner.backend.destroy(sandbox_id).await;
            }
        }
        // Top up active templates toward their (possibly new) target.
        for entry in self.inner.known.iter() {
            if entry.inactive_since.is_some() {
                continue;
            }
            self.maybe_refill(*entry.key()).await;
        }
    }

    /// ADR 0014 M1.9 autoscaler. Computes the target N(T) for
    /// `template_ref` at `now`:
    ///
    /// - If no leases in COLD_TAIL → 0 (drain back to cold tail).
    /// - Else compute lease_rate = leases_in_window / window_secs
    ///   (per second), and N = max(FLOOR_TARGET, ceil(rate ×
    ///   REFILL_TIME_SECS × AUTOSCALE_HEADROOM)). Clamp to
    ///   CEILING_TARGET.
    ///
    /// The formula is "keep enough slots that the refill loop can
    /// keep up with the observed lease rate × refill time, plus a
    /// headroom multiplier for burst tolerance." Driven by the
    /// observed lease rate, not heartbeat-reported slots, so it's
    /// resilient to template-ref skew.
    fn compute_target(&self, template_ref: TemplateRef, now: std::time::Instant) -> u32 {
        let history = match self.inner.lease_history.get(&template_ref) {
            Some(h) => h,
            None => return FLOOR_TARGET,
        };
        let leases = history.lock();
        if leases.is_empty() {
            return FLOOR_TARGET;
        }
        // Cold tail: if newest lease is older than COLD_TAIL, drain.
        if let Some(latest) = leases.iter().max() {
            if now.duration_since(*latest) >= COLD_TAIL {
                return 0;
            }
        }
        let count = leases.len() as f64;
        let window_secs = AUTOSCALE_WINDOW.as_secs_f64();
        let lease_rate = count / window_secs;
        let raw = lease_rate * REFILL_TIME_SECS * AUTOSCALE_HEADROOM;
        let target = (raw.ceil() as u32).max(FLOOR_TARGET);
        target.min(CEILING_TARGET)
    }

    /// If the free-list for `template_ref` is below target, spawn
    /// one refill task. The spawned task does the heavy lifting
    /// (BlobStorage download via `backend.restore`) off the caller's
    /// task. Concurrency is bounded by `RefillGuard` (one in-flight
    /// refill per template_ref); a M1.9 follow-up will make this
    /// rate-aware.
    async fn maybe_refill(&self, template_ref: TemplateRef) {
        let target = self
            .inner
            .targets
            .get(&template_ref)
            .map(|t| *t)
            .unwrap_or(FLOOR_TARGET);
        let current = self
            .inner
            .free_lists
            .get(&template_ref)
            .map(|m| m.lock().len() as u32)
            .unwrap_or(0);
        if current >= target {
            return;
        }
        let metadata = match self.inner.known.get(&template_ref) {
            Some(k) => k.metadata.clone(),
            None => return,
        };
        // Race guard: only one in-flight refill per template_ref
        // at a time. The DashMap `Entry` API gives us atomic
        // test-and-set under a per-shard write lock — the match
        // arm runs while the lock is held, so the Vacant→insert
        // transition cannot race a parallel Vacant→insert on
        // another task. The buggy alternative was `matches!` on
        // `entry()` followed by a separate `.insert()`, which
        // dropped the lock between the check and the write.
        match self.inner.inflight_refills.entry(template_ref) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                tracing::trace!(
                    %template_ref,
                    "warm_pool refill already in flight; skipping spawn",
                );
                return;
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(());
            }
        }
        let pool = self.clone();
        tokio::spawn(async move {
            // SnapshotMetadata is Clone — capture it into the
            // spawned task. The backend's restore() handles
            // BlobStorage materialisation (state.bin + sidecar +
            // memory chunks) under the hood, then drives FC.
            let outcome = pool.inner.backend.restore(metadata).await;
            // Drop the in-flight flag before pushing to the
            // free-list so the next gc_tick can spawn another
            // refill if target hasn't been met yet.
            pool.inner.inflight_refills.remove(&template_ref);
            let sandbox_id = match outcome {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        %template_ref,
                        error = %e,
                        "warm_pool refill failed",
                    );
                    return;
                }
            };
            pool.inner
                .free_lists
                .entry(template_ref)
                .or_default()
                .lock()
                .push(sandbox_id);
            tracing::debug!(
                %template_ref,
                %sandbox_id,
                "warm_pool refill complete",
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use engram_core::traits::sandbox::SandboxBackend;
    use engram_core::types::ids::SnapshotId;
    use engram_core::types::manifest::ManifestRef;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::SandboxId;
    use parking_lot::Mutex as PMutex;

    /// Test backend that hands out a fresh SandboxId on every
    /// restore() and records the snapshot_id it was asked to
    /// restore. Implements just enough of SandboxBackend for
    /// WarmPool's needs.
    struct FakeBackend {
        restore_count: PMutex<u32>,
        last_metadata: PMutex<Option<SnapshotMetadata>>,
        destroy_log: PMutex<Vec<SandboxId>>,
        agent_log: PMutex<Vec<(SandboxId, AgentSpec)>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                restore_count: PMutex::new(0),
                last_metadata: PMutex::new(None),
                destroy_log: PMutex::new(Vec::new()),
                agent_log: PMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SandboxBackend for FakeBackend {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn snapshot_path_for(&self, _: SnapshotId) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        async fn restore(&self, m: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            *self.restore_count.lock() += 1;
            *self.last_metadata.lock() = Some(m);
            Ok(SandboxId::new())
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroy_log.lock().push(id);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, id: SandboxId, spec: AgentSpec) -> Result<(), SandboxError> {
            self.agent_log.lock().push((id, spec));
            Ok(())
        }
    }

    fn template(snapshot_id: SnapshotId) -> (TemplateRecord, SnapshotMetadata) {
        let ref_id = TemplateRef::new();
        let rec = TemplateRecord {
            template_ref: ref_id,
            image_repo: "repo".into(),
            image_tag: "tag".into(),
            harness_pack_uri: "pack".into(),
            snapshot_id,
            vcpus: 1,
            memory_mib: 64,
            created_at: Utc::now(),
            active: true,
        };
        let meta = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: Some(ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
        };
        (rec, meta)
    }

    /// Wait until `cond` returns true OR the deadline expires.
    /// Refill happens off-task in a tokio::spawn so unit tests need
    /// to poll for completion.
    async fn wait_for(cond: impl Fn() -> bool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn observe_templates_refills_to_default_target() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;
        assert!(
            wait_for(
                || *backend.restore_count.lock() >= FLOOR_TARGET,
                Duration::from_secs(2)
            )
            .await,
            "refill should run at least FLOOR_TARGET times",
        );
        // The free-list should now have at least one entry.
        let slots = pool.list_slots();
        let entry = slots
            .iter()
            .find(|s| s.template_ref == rec.template_ref)
            .expect("free-list entry exists");
        assert!(entry.available >= 1);
        assert_eq!(entry.target, FLOOR_TARGET);
    }

    #[tokio::test]
    async fn lease_pops_from_free_list_and_triggers_refill() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;
        wait_for(
            || *backend.restore_count.lock() >= 1,
            Duration::from_secs(2),
        )
        .await;
        let outcome = pool.lease(rec.template_ref).await;
        assert!(matches!(outcome, WarmLeaseOutcome::Granted(_)));
        // Lease triggers another refill to maintain target.
        wait_for(
            || *backend.restore_count.lock() >= 2,
            Duration::from_secs(2),
        )
        .await;
        assert!(*backend.restore_count.lock() >= 2);
    }

    #[tokio::test]
    async fn lease_unknown_template_returns_stale_or_nocapacity() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        // Empty pool — no known templates.
        let unknown = TemplateRef::new();
        let outcome = pool.lease(unknown).await;
        assert!(matches!(outcome, WarmLeaseOutcome::NoCapacity));

        // Populate one known template, then ask for a different one.
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta)]).await;
        let outcome = pool.lease(TemplateRef::new()).await;
        assert!(
            matches!(outcome, WarmLeaseOutcome::Stale { current_ref } if current_ref == rec.template_ref),
            "stale outcome should hint at the host's known active ref",
        );
    }

    #[tokio::test]
    async fn launch_invokes_start_agent_on_backend() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let sandbox_id = SandboxId::new();
        let agent = AgentSpec {
            argv: vec!["/bin/echo".into(), "x".into()],
            env: Default::default(),
        };
        let policy = SessionEgressPolicy {
            session_id: engram_core::SessionId::new(),
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        };
        pool.launch(sandbox_id, agent.clone(), policy)
            .await
            .unwrap();
        let log = backend.agent_log.lock().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, sandbox_id);
        assert_eq!(log[0].1.argv, agent.argv);
    }

    #[tokio::test]
    async fn autoscaler_raises_target_under_lease_rate() {
        // Many recent leases ⇒ target rises above FLOOR_TARGET
        // (capped by CEILING_TARGET).
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta)]).await;
        // Inject a synthetic burst: 100 lease timestamps in the
        // last 60s. At 100 leases / 300s window × 5s refill × 1.2
        // headroom = ceil(2.0) = 2, capped to CEILING_TARGET=4.
        let now = std::time::Instant::now();
        {
            let entry = pool
                .inner
                .lease_history
                .entry(rec.template_ref)
                .or_default();
            entry
                .lock()
                .extend((0..100).map(|i| now - Duration::from_secs(i as u64)));
        }
        pool.gc_tick().await;
        let target = pool.compute_target(rec.template_ref, now);
        // v1 caps at CEILING_TARGET=1 because concurrent restores
        // from one snapshot collide on vsock UDS (ADR 0014 mount-
        // namespace work). Once that lands and CEILING_TARGET
        // rises, this assertion should change to
        // `(2..=CEILING_TARGET).contains(&target)`.
        assert_eq!(
            target, CEILING_TARGET,
            "autoscaler should clamp to CEILING_TARGET under load (got {target})",
        );
    }

    #[tokio::test]
    async fn autoscaler_holds_at_floor_with_no_leases() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta)]).await;
        let now = std::time::Instant::now();
        // No history → floor.
        assert_eq!(pool.compute_target(rec.template_ref, now), FLOOR_TARGET);
    }

    #[tokio::test]
    async fn autoscaler_drains_to_zero_after_cold_tail() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta)]).await;
        // Inject a lease that's older than COLD_TAIL — autoscaler
        // should target 0.
        let now = std::time::Instant::now();
        let stale = now - COLD_TAIL - Duration::from_secs(60);
        pool.inner
            .lease_history
            .entry(rec.template_ref)
            .or_default()
            .lock()
            .push(stale);
        assert_eq!(pool.compute_target(rec.template_ref, now), 0);
    }

    #[tokio::test]
    async fn rebake_drains_old_free_list() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec_v1, meta_v1) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec_v1.clone(), meta_v1)])
            .await;
        wait_for(
            || *backend.restore_count.lock() >= 1,
            Duration::from_secs(2),
        )
        .await;
        let v1_sandboxes: Vec<_> = pool
            .inner
            .free_lists
            .get(&rec_v1.template_ref)
            .map(|m| m.lock().clone())
            .unwrap_or_default();
        assert!(!v1_sandboxes.is_empty());

        // Rebake: same template_ref family but new snapshot_id.
        let new_snap = SnapshotId::new();
        let rec_v2 = TemplateRecord {
            snapshot_id: new_snap,
            ..rec_v1.clone()
        };
        let meta_v2 = SnapshotMetadata {
            id: new_snap,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: Some(ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
        };
        pool.observe_templates(vec![(rec_v2, meta_v2)]).await;
        // Old sandboxes were destroyed.
        wait_for(
            || backend.destroy_log.lock().len() >= v1_sandboxes.len(),
            Duration::from_secs(2),
        )
        .await;
        let destroyed = backend.destroy_log.lock().clone();
        for sid in v1_sandboxes {
            assert!(destroyed.contains(&sid), "old sandbox should be destroyed");
        }
    }
}
