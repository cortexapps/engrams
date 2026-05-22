//! ADR 0014 M1.6: per-host warm-pool driver.
//!
//! Holds a free-list of pre-restored Firecracker microVMs per
//! [`TemplateRef`]. Each entry is a sandbox that has been
//! `restore_from_snapshot`d from the template's bake-time portable
//! snapshot (state.bin + sidecar + memory chunks) and is currently
//! paused with agentd awaiting a `SpawnHarness` frame on vsock.
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
//!   which applies the policy and writes the `SpawnHarness`
//!   frame to the in-guest agentd.
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

/// Operator-facing kill switch. `ENGRAM_WARM_POOL_DISABLED=1` (or
/// `true` / `yes`, case-insensitive) forces the autoscaler to report
/// `target=0` for every template — `gc_tick` then drains existing
/// free-list slots and `maybe_refill` no-ops because `current >=
/// target` is already true at 0. Coord falls through to cold-create.
///
/// Why an env var and not a CLI flag: prod host-agent is configured
/// almost entirely through env (`ENGRAM_*`), and an env flip means
/// ops can disable warm-pool on a single host without a re-deploy
/// (set in helm values + roll the MIG, or apply directly to a
/// running supervisor for emergency mitigation).
///
/// Used today (2026-05-21) to stop bleeding from the warm-restore
/// CPU-mismatch bug — AMD bake runners produce snapshots whose
/// guest CPUID claims AMD, restored on Intel Cascade Lake prod
/// hosts; glibc's ifunc resolver picks AMD-only AVX-512 paths and
/// every shell child fork segfaults on exit cleanup. Disabling
/// warm-pool puts every session on the cold path where the guest's
/// CPUID matches the actual prod CPU.
fn warm_pool_disabled_from_env() -> bool {
    matches!(
        std::env::var("ENGRAM_WARM_POOL_DISABLED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True") | Ok("yes") | Ok("YES") | Ok("Yes")
    )
}

struct WarmPoolInner {
    /// ADR 0014 followup 2026-05-21: when `true`, every per-template
    /// target is pinned to 0 regardless of lease history. Set via
    /// `ENGRAM_WARM_POOL_DISABLED` at construction; immutable for
    /// the lifetime of the process.
    disabled: bool,
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
    /// ADR 0014 issue #5: per-template refill failure tracker. The
    /// refill spawn increments the counter (and remembers the most
    /// recent error class) on `backend.restore` failure;
    /// `list_slots` drains it on each heartbeat so coord receives
    /// the delta, not a cumulative count. Coord emits
    /// `engram_warm_pool_refill_failures_total{host_id,template_ref,
    /// error_class}` from the drained surface.
    refill_failures: DashMap<TemplateRef, parking_lot::Mutex<FailureRollup>>,
}

/// In-memory rollup of refill failures since the last heartbeat
/// drain. Captures the count and the most recent classifier.
#[derive(Clone, Debug, Default)]
struct FailureRollup {
    count: u32,
    last_class: String,
}

/// Map a `SandboxError` from `backend.restore` into a short
/// classifier the coord-side metric labels with. Stable strings —
/// dashboards key on these.
///
/// String-match on the message rather than a typed downcast because
/// the error chains from `BlobStorage`/`ChunkStore` get flattened
/// through `SandboxError::Vm/Snapshot(_)` and we'd lose the typed
/// surface anyway. The labels match the strings the templates
/// sweeper writes for the same root causes.
fn classify_refill_error(err: &engram_core::SandboxError) -> &'static str {
    let msg = err.to_string();
    let m = msg.to_lowercase();
    if m.contains("blob not found") || m.contains("no such file or directory") {
        "blob_not_found"
    } else if m.contains("manifest") {
        "manifest_load"
    } else if m.contains("firecracker") || m.contains("fc_spawn") || m.contains("spawn") {
        "fc_spawn"
    } else {
        "other"
    }
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
        let disabled = warm_pool_disabled_from_env();
        if disabled {
            tracing::info!(
                "warm pool disabled via ENGRAM_WARM_POOL_DISABLED — all sessions go cold",
            );
        }
        Self {
            inner: Arc::new(WarmPoolInner {
                disabled,
                free_lists: DashMap::new(),
                known: DashMap::new(),
                targets: DashMap::new(),
                lease_history: DashMap::new(),
                inflight_refills: DashMap::new(),
                backend,
                refill_failures: DashMap::new(),
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
            .or_insert_with(|| self.initial_target());
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

    /// Activate a previously-leased warm sandbox: optionally swap
    /// the harness drive (ADR 0014 M1.12 option D), apply egress
    /// policy, then push SpawnHarness. Pre-restored substrate is
    /// already running; this is the per-session activation step.
    ///
    /// `session_harness_path` is `Some` for sessions whose chosen
    /// harness differs from the template's bake-time stub. The
    /// host-agent's gRPC handler resolves the session's
    /// harness_pack_uri via `image_cache.ensure_harness_ext4`
    /// before calling here. `None` skips the swap (sessions with
    /// no harness, or templates whose stub already matches what
    /// the session wants).
    pub async fn launch(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        mut policy: SessionEgressPolicy,
        session_harness_path: Option<std::path::PathBuf>,
    ) -> Result<(), SandboxError> {
        // ADR 0014: coord builds the egress policy with a placeholder
        // `sandbox_id` because it doesn't know which warm slot will
        // be leased until LeaseWarmSandbox returns; the comment in
        // sessions.rs:574 reads "overwritten by host on launch."
        // That overwrite has to actually happen — `notify_session_policy`
        // takes `policy.sandbox_id` at face value and inserts the
        // placeholder into the egress-proxy registry. Without this
        // line, the registry has no entry for the actual warm
        // sandbox's source IP, so every outbound from the in-VM
        // harness (e.g. claude → api.anthropic.com) misses the
        // policy lookup and gets dropped. Observed on session
        // 9d9fef3e (2026-05-20): warm-lease worked, vsock handshake
        // worked, but the harness produced no response because its
        // first egress call failed.
        policy.sandbox_id = sandbox_id;
        // Same overwrite story for guest_ip: coord builds the policy
        // with `Ipv4Addr::UNSPECIFIED` (sessions.rs:575 leaves the
        // M1.x "policy refresh keyed on the actual guest_ip" TODO
        // unresolved on the warm path). Until the host fills in the
        // real value, the egress-proxy registry is indexed against
        // 0.0.0.0; when a VM packet arrives with peer-IP = the
        // netns SNAT slot (e.g. 10.200.0.6), `Registry::lookup`
        // misses entirely, the proxy drops the connection, and the
        // in-VM harness's first outbound (claude → api.anthropic.com)
        // hangs without logs. Observed in prod 2026-05-20 on session
        // accb3924: the iptables REDIRECT fix put packets at the
        // proxy's door but registry lookup found nothing, so
        // egress was still effectively blackholed — just one layer
        // deeper than the FORWARD DROP that preceded the REDIRECT
        // fix.
        if let Some(ip_str) = self.inner.backend.guest_ip(sandbox_id).await {
            if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
                policy.guest_ip = ip;
            } else {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    ip_str = %ip_str,
                    "guest_ip parse failed; policy will register against UNSPECIFIED \
                     and egress lookup will miss",
                );
            }
        } else {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                "backend returned no guest_ip; policy will register against UNSPECIFIED \
                 and egress lookup will miss",
            );
        }
        // ADR 0014 ordering: policy onto the proxy registry BEFORE
        // bootstrap exec's the agent. Same invariant ADR 0013
        // codified for cold-create's StartAgent. Harness swap goes
        // BEFORE start_agent so bootstrap mounts the session's
        // chosen ext4, not the bake-time stub.
        let phase_start = std::time::Instant::now();
        let result = async {
            self.inner.backend.notify_session_policy(policy).await?;
            if let Some(path) = session_harness_path {
                self.inner
                    .backend
                    .swap_harness_drive(sandbox_id, path)
                    .await?;
            }
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
    ///
    /// ADR 0014 issue #5: also drains the per-template
    /// `refill_failures` rollup so each heartbeat reports the delta
    /// since the last call. Drain-on-read is the cleanest "rate over
    /// a window" surface without per-tick math on the host — the
    /// host accumulates between heartbeats, coord receives + zeroes
    /// each tick.
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
                .unwrap_or_else(|| self.initial_target());
            let (refill_failures_since_last, last_error_class) =
                self.drain_refill_failures(template_ref);
            out.push(WarmSlotCount {
                template_ref,
                available,
                target,
                refill_failures_since_last,
                last_error_class,
            });
        }
        // Templates that have ONLY produced failures and zero
        // free-list entries deserve a heartbeat row too — otherwise
        // a permanently-broken template's signal vanishes the moment
        // its free-list empties.
        for entry in self.inner.refill_failures.iter() {
            let template_ref = *entry.key();
            if out.iter().any(|s| s.template_ref == template_ref) {
                continue;
            }
            let (refill_failures_since_last, last_error_class) =
                self.drain_refill_failures(template_ref);
            if refill_failures_since_last == 0 {
                continue;
            }
            let target = self
                .inner
                .targets
                .get(&template_ref)
                .map(|t| *t)
                .unwrap_or_else(|| self.initial_target());
            out.push(WarmSlotCount {
                template_ref,
                available: 0,
                target,
                refill_failures_since_last,
                last_error_class,
            });
        }
        out
    }

    /// Drain the per-template failure rollup and return the snapshot
    /// fields the heartbeat ships. Resets the rollup to zero.
    fn drain_refill_failures(&self, template_ref: TemplateRef) -> (u32, String) {
        match self.inner.refill_failures.get(&template_ref) {
            None => (0, String::new()),
            Some(mu) => {
                let mut g = mu.lock();
                let count = g.count;
                let class = std::mem::take(&mut g.last_class);
                g.count = 0;
                (count, class)
            }
        }
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
    /// The target a brand-new template should be assigned before the
    /// autoscaler has had any lease history to act on. `FLOOR_TARGET`
    /// in normal mode; `0` when the operator kill switch is on so
    /// nothing ever spins up.
    fn initial_target(&self) -> u32 {
        if self.inner.disabled {
            0
        } else {
            FLOOR_TARGET
        }
    }

    fn compute_target(&self, template_ref: TemplateRef, now: std::time::Instant) -> u32 {
        // Operator kill switch: force every template to target=0 so
        // gc_tick drains existing slots and maybe_refill no-ops.
        if self.inner.disabled {
            return 0;
        }
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
            .unwrap_or_else(|| self.initial_target());
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
                    // ADR 0014 issue #5: classify and rollup so the
                    // next heartbeat ships the failure delta.
                    let class = classify_refill_error(&e);
                    let mut entry = pool.inner.refill_failures.entry(template_ref).or_default();
                    let mut g = entry.value_mut().lock();
                    g.count = g.count.saturating_add(1);
                    g.last_class = class.to_string();
                    drop(g);
                    tracing::warn!(
                        %template_ref,
                        error = %e,
                        error_class = class,
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
        // ADR 0014: capture the policy that `launch` forwards to
        // `notify_session_policy` so tests can assert the
        // sandbox_id overwrite actually happens.
        policy_log: PMutex<Vec<SessionEgressPolicy>>,
        // ADR 0014: configurable guest_ip return value so tests can
        // verify `launch`'s overwrite of `policy.guest_ip` with the
        // backend's real SNAT'd IP. `None` (default) makes
        // `guest_ip()` return None and exercises the leave-as-
        // UNSPECIFIED branch.
        guest_ip_value: PMutex<Option<String>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                restore_count: PMutex::new(0),
                last_metadata: PMutex::new(None),
                destroy_log: PMutex::new(Vec::new()),
                agent_log: PMutex::new(Vec::new()),
                policy_log: PMutex::new(Vec::new()),
                guest_ip_value: PMutex::new(None),
            }
        }

        fn with_guest_ip(self, ip: &str) -> Self {
            *self.guest_ip_value.lock() = Some(ip.to_string());
            self
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
        async fn notify_session_policy(
            &self,
            policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            self.policy_log.lock().push(policy);
            Ok(())
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
            self.guest_ip_value.lock().clone()
        }
    }

    fn template(snapshot_id: SnapshotId) -> (TemplateRecord, SnapshotMetadata) {
        let ref_id = TemplateRef::new();
        let rec = TemplateRecord {
            template_ref: ref_id,
            image_repo: "repo".into(),
            image_tag: "tag".into(),
            harness_pack_uri: None,
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
            working_set_blob_key: None,
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
        // ADR 0014 M1.12: None for harness path — the FakeBackend
        // doesn't implement swap_harness_drive (default trait impl
        // returns InvalidSpec), so we skip the swap step here.
        // Real warm-lease callers pass the session's harness ext4
        // path resolved via image_cache.
        pool.launch(sandbox_id, agent.clone(), policy, None)
            .await
            .unwrap();
        let log = backend.agent_log.lock().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, sandbox_id);
        assert_eq!(log[0].1.argv, agent.argv);
    }

    /// ADR 0014 regression guard. The coord builds `SessionEgressPolicy`
    /// with a placeholder `sandbox_id` (it doesn't know which warm
    /// slot will be leased until LeaseWarmSandbox returns). The
    /// host-side `launch` MUST overwrite `policy.sandbox_id` with the
    /// real warm-slot id before forwarding to `notify_session_policy`
    /// — without it the egress proxy registry keys on the placeholder
    /// id, the actual warm slot's source IP misses the lookup, and
    /// the in-VM harness's first egress call (e.g. claude →
    /// api.anthropic.com) gets dropped. Observed on session 9d9fef3e
    /// (2026-05-20): warm-lease worked, vsock handshake worked, but
    /// the harness produced no response.
    #[tokio::test]
    async fn launch_overwrites_policy_sandbox_id_with_real_warm_id() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let real_sandbox = SandboxId::new();
        let placeholder = SandboxId::new();
        assert_ne!(real_sandbox, placeholder);
        let agent = AgentSpec {
            argv: vec!["/bin/true".into()],
            env: Default::default(),
        };
        let policy = SessionEgressPolicy {
            session_id: engram_core::SessionId::new(),
            sandbox_id: placeholder, // what coord ships
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        };
        pool.launch(real_sandbox, agent, policy, None)
            .await
            .unwrap();
        let log = backend.policy_log.lock().clone();
        assert_eq!(log.len(), 1, "notify_session_policy must fire exactly once");
        assert_eq!(
            log[0].sandbox_id, real_sandbox,
            "launch must overwrite policy.sandbox_id with the warm-slot id, \
             not pass the coord-side placeholder through"
        );
        assert_ne!(
            log[0].sandbox_id, placeholder,
            "the placeholder id must not survive into the egress registry",
        );
    }

    /// ADR 0014 regression guard, second axis. The coord ALSO ships
    /// the policy with `guest_ip = Ipv4Addr::UNSPECIFIED` because at
    /// session-create time it doesn't know which netns slot the warm
    /// lease will land on. The host MUST overwrite this with the
    /// backend's actual `guest_ip` (the netns's SNAT'd source IP)
    /// before forwarding to `notify_session_policy` — without it
    /// the egress proxy registry is indexed against 0.0.0.0, every
    /// real VM packet's peer-IP (e.g. 10.200.0.6) misses lookup,
    /// and the proxy drops the connection. Observed on session
    /// accb3924 (2026-05-20): the iptables REDIRECT fix had landed,
    /// packets reached the proxy, but registry.lookup(10.200.0.6)
    /// found nothing and egress was still blackholed — just one
    /// layer deeper than before.
    #[tokio::test]
    async fn launch_overwrites_policy_guest_ip_with_real_snat_ip() {
        let backend = Arc::new(FakeBackend::new().with_guest_ip("10.200.0.6"));
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let sandbox_id = SandboxId::new();
        let agent = AgentSpec {
            argv: vec!["/bin/true".into()],
            env: Default::default(),
        };
        let policy = SessionEgressPolicy {
            session_id: engram_core::SessionId::new(),
            sandbox_id,
            // The coord ships UNSPECIFIED — see sessions.rs:575.
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        };
        pool.launch(sandbox_id, agent, policy, None).await.unwrap();
        let log = backend.policy_log.lock().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].guest_ip,
            "10.200.0.6".parse::<std::net::Ipv4Addr>().unwrap(),
            "launch must overwrite policy.guest_ip with the backend's SNAT'd IP, \
             not pass the UNSPECIFIED placeholder through to the registry",
        );
        assert_ne!(
            log[0].guest_ip,
            std::net::Ipv4Addr::UNSPECIFIED,
            "the UNSPECIFIED placeholder must not survive into the egress registry",
        );
    }

    /// Belt-and-suspenders for the UNSPECIFIED path: when the
    /// backend can't determine a guest_ip (no netns yet, agentd not
    /// up, etc.) the policy.guest_ip stays UNSPECIFIED and `launch`
    /// emits a warn — but does NOT silently substitute a different
    /// value. The proxy will drop traffic in that case, which is
    /// the correct fail-closed behaviour.
    #[tokio::test]
    async fn launch_leaves_guest_ip_unspecified_when_backend_has_none() {
        let backend = Arc::new(FakeBackend::new()); // no .with_guest_ip
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let sandbox_id = SandboxId::new();
        let policy = SessionEgressPolicy {
            session_id: engram_core::SessionId::new(),
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        };
        pool.launch(
            sandbox_id,
            AgentSpec {
                argv: vec!["/bin/true".into()],
                env: Default::default(),
            },
            policy,
            None,
        )
        .await
        .unwrap();
        let log = backend.policy_log.lock().clone();
        assert_eq!(log[0].guest_ip, std::net::Ipv4Addr::UNSPECIFIED);
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
            working_set_blob_key: None,
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

    // ──────────────────────────────────────────────────────────────
    // ADR 0014 issue #5: refill-failure heartbeat tests.
    // ──────────────────────────────────────────────────────────────

    /// FakeBackend variant whose `restore()` always fails. The error
    /// message lets us assert the classifier produces the expected
    /// `error_class` label.
    struct FailingBackend {
        error: String,
        restore_attempts: PMutex<u32>,
    }

    impl FailingBackend {
        fn new(error: &str) -> Self {
            Self {
                error: error.into(),
                restore_attempts: PMutex::new(0),
            }
        }
    }

    #[async_trait]
    impl SandboxBackend for FailingBackend {
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
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            *self.restore_attempts.lock() += 1;
            Err(SandboxError::Vm(self.error.clone().into()))
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// classify_refill_error pulls the right label out of common
    /// error messages. Dashboards key on these strings.
    #[test]
    fn classify_refill_error_labels() {
        let blob = SandboxError::Vm("blob storage: blob not found at snapshots/x".into());
        let manifest = SandboxError::Vm("read manifest sha256:xxx version 1: ...".into());
        let fc = SandboxError::Vm("firecracker spawn failed: ENOENT".into());
        let other = SandboxError::Vm("something exotic".into());
        assert_eq!(classify_refill_error(&blob), "blob_not_found");
        assert_eq!(classify_refill_error(&manifest), "manifest_load");
        assert_eq!(classify_refill_error(&fc), "fc_spawn");
        assert_eq!(classify_refill_error(&other), "other");
    }

    /// Refill failures accumulate per template and surface on the
    /// next `list_slots`. Drain-on-read: a second list_slots without
    /// new failures returns count=0.
    #[tokio::test]
    async fn refill_failures_accumulate_and_drain_on_list_slots() {
        let backend = Arc::new(FailingBackend::new(
            "blob storage: blob not found at snapshots/abc/state.bin",
        ));
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;

        // Wait for at least one refill attempt (the FLOOR_TARGET=1
        // path fires immediately on observe).
        assert!(
            wait_for(
                || *backend.restore_attempts.lock() >= 1,
                Duration::from_secs(2)
            )
            .await,
            "at least one refill attempt must occur",
        );

        // First list_slots: count >= 1, class = blob_not_found.
        let slots = pool.list_slots();
        let entry = slots
            .iter()
            .find(|s| s.template_ref == rec.template_ref)
            .expect("refill_failures must surface even with empty free-list");
        assert!(
            entry.refill_failures_since_last >= 1,
            "expected >= 1 failure, got {}",
            entry.refill_failures_since_last
        );
        assert_eq!(entry.last_error_class, "blob_not_found");

        // Second list_slots without any new failure happening: the
        // counter must have been drained.
        let slots = pool.list_slots();
        let entry = slots
            .iter()
            .find(|s| s.template_ref == rec.template_ref)
            // The template may or may not still surface depending on
            // gc_tick + refill timing; only assert if it does that
            // the count is 0.
            .map(|s| (s.refill_failures_since_last, s.last_error_class.clone()));
        if let Some((count, class)) = entry {
            assert_eq!(count, 0, "second list_slots must drain to zero");
            assert!(class.is_empty(), "class drained to empty");
        }
    }

    /// ADR 0014 followup 2026-05-21: the operator kill switch
    /// (`ENGRAM_WARM_POOL_DISABLED=1`) must pin every per-template
    /// target to 0 — `observe_templates` sets it; `compute_target`
    /// keeps it; `maybe_refill` no-ops because `current >= target`
    /// is already true at 0. End-to-end: `backend.restore` is
    /// never invoked, the free-list stays empty, and `list_slots`
    /// reports either nothing (no entries) or an entry with
    /// `target == 0 && available == 0`.
    #[tokio::test]
    async fn kill_switch_pins_target_to_zero_and_skips_refill() {
        // Env var is process-global, so isolate. `WarmPool::new`
        // captures the flag at construction; later mutations of the
        // env are ignored by this instance.
        let prev = std::env::var("ENGRAM_WARM_POOL_DISABLED").ok();
        // SAFETY: serialized by this single-threaded test; no other
        // test should be reading the var during this window. The
        // restore at the end (or panic path) puts it back.
        unsafe { std::env::set_var("ENGRAM_WARM_POOL_DISABLED", "1") };

        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;

        // Give the would-be refill spawn time to either run or not.
        // 500ms is enough — observe_templates -> maybe_refill -> spawn
        // would have begun by now.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            *backend.restore_count.lock(),
            0,
            "kill switch must prevent backend.restore from being invoked",
        );

        let slots = pool.list_slots();
        // Either the template hasn't entered list_slots yet (no
        // refill_failures + no free-list entry) OR it appears with
        // target=0. Both shapes are acceptable; assert the
        // negative: no slot reports a non-zero target.
        for slot in &slots {
            if slot.template_ref == rec.template_ref {
                assert_eq!(
                    slot.target, 0,
                    "kill switch must pin reported target to 0 (got {})",
                    slot.target,
                );
                assert_eq!(slot.available, 0);
            }
        }

        // Restore env so peer tests aren't confused by leakage.
        unsafe {
            match prev.as_deref() {
                Some(v) => std::env::set_var("ENGRAM_WARM_POOL_DISABLED", v),
                None => std::env::remove_var("ENGRAM_WARM_POOL_DISABLED"),
            }
        }
    }

    /// Successful refill does NOT increment the failure counter
    /// (regression guard against accidentally counting every spawn).
    #[tokio::test]
    async fn successful_refill_does_not_increment_failure_counter() {
        let backend = Arc::new(FakeBackend::new());
        let pool = WarmPool::new(backend.clone() as Arc<dyn SandboxBackend>);
        let (rec, meta) = template(SnapshotId::new());
        pool.observe_templates(vec![(rec.clone(), meta.clone())])
            .await;
        assert!(
            wait_for(
                || *backend.restore_count.lock() >= FLOOR_TARGET,
                Duration::from_secs(2),
            )
            .await,
            "refill must run",
        );
        let slots = pool.list_slots();
        let entry = slots
            .iter()
            .find(|s| s.template_ref == rec.template_ref)
            .expect("free-list entry exists");
        assert_eq!(entry.refill_failures_since_last, 0);
        assert!(entry.last_error_class.is_empty());
    }
}
