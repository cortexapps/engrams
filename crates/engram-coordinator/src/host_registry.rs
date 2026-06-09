//! Coordinator-side registry of connected hosts.
//!
//! Each host registers a [`HostClient`] (typically a [`RemoteHostClient`]
//! wrapping a WS connection, or in `--mode=all` a [`LocalHostClient`]).
//! `HostRegistry` itself implements [`HostClient`] by:
//!
//! 1. Routing `create` / `restore` to a scheduler-picked host and
//!    recording the resulting `SandboxId → HostId` so subsequent calls
//!    against that id reach the right host.
//! 2. Routing `exec_stream` / `snapshot` / `destroy` / `start_agent` /
//!    `bind_session` / `unbind_session` / `send_prompt` by looking up
//!    the `HostId` for the supplied `SandboxId`.
//! 3. Aggregating `list` across all connected hosts.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::{HarnessDial, HostClient, MetadataStore};
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::session::SessionState;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SessionId, SnapshotId};
use engram_protocol::heartbeat::{HostCapacityReport, LocalSnapshotReport};
use parking_lot::RwLock;

/// Per-host record kept in-memory. Backend dispatches RPCs over the
/// wire (or directly in `--mode=all`); the heartbeat-derived state
/// fields drive the scheduler in Phase 3b. The optional
/// `admin_client` is the underlying `ConnectedHost` used by
/// out-of-band admin RPCs (ADR 0007 materialize-dir reap fanout);
/// `None` for `--mode=all` test-registered backends that aren't
/// routed through the wire layer.
struct HostEntry {
    backend: Arc<dyn HostClient>,
    state: RwLock<HostState>,
    /// ADR 0015 M3: millis-since-epoch of the most recent
    /// observation (registration or heartbeat). `resolve_owner` uses
    /// this to soft-invalidate the cache after [`HostRegistry::ttl`]
    /// for hosts that have stopped heartbeating but haven't yet been
    /// declared dead by the detector (operator-paused, network
    /// partition that hasn't crossed the 30s threshold). A lazy
    /// invalidator beats a background sweeper here — we only care
    /// about freshness on the read path.
    last_observed_heartbeat: AtomicI64,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Heartbeat-derived view of a host. Updated by the WS supervisor
/// every ~5s; read by [`HostRegistry::pick_for_session`] when scheduling.
#[derive(Clone, Debug, Default)]
pub struct HostState {
    pub capacity: HostCapacityReport,
    pub local_snapshots: Vec<LocalSnapshotReport>,
    pub draining: bool,
    /// ADR 0015 M5: manifest digests of images this host has fully
    /// prefetched to local NVMe. `pick_for_session` gates host
    /// selection on `ready_images.contains(&digest)`. Empty on a
    /// freshly-registered host until its prefetch supervisor finishes
    /// pulling the first enabled image.
    pub ready_images: std::collections::HashSet<engram_protocol::heartbeat::ManifestDigest>,
    /// ADR 0035: the bake stamp this host reported — which bundle
    /// generation it carries as *current*. Operator visibility into
    /// fleet skew mid-roll; capture resolution itself happens
    /// host-side against the same stamp.
    pub current_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// Observed disk/mem/cpu utilization from the latest heartbeat.
    /// Read-only fleet-view signal — the scheduler reasons about
    /// `capacity` (reservation), not this. Mirrors the persisted
    /// `hosts` row so the view is correct even on a coord pod that
    /// didn't field this host's heartbeat.
    pub utilization: engram_core::types::host::HostUtilization,
}

/// Inputs the scheduler considers when picking a host. [`SandboxBackend`]
/// trait calls don't carry these, so handlers that want session-aware
/// scheduling call [`HostRegistry::create_for_session`] explicitly. The
/// trait impl uses [`HostRegistry::pick_any`] (no affinity — used for
/// anonymous flows like list / restore where the request doesn't carry
/// session context).
#[derive(Clone, Debug)]
pub struct ScheduleContext<'a> {
    pub repo: &'a str,
    pub image_version: &'a str,
    /// Snapshot id to prefer (zero-cost hot tier hit). `None` for fresh
    /// sessions; `Some` when resuming or migrating.
    pub prefer_snapshot_id: Option<SnapshotId>,
    /// Memory hint for capacity ranking. `None` falls through to
    /// "any host with > 0 free capacity" rather than a strict fit.
    pub memory_mib: Option<u32>,
    /// ADR 0015 M5: when set, restricts the candidate pool to hosts
    /// whose latest heartbeat reported this digest in `ready_images`.
    /// `None` skips the readiness filter — used by restore paths and
    /// anonymous flows that don't carry an `enabled_images` row.
    pub required_image_digest: Option<engram_protocol::heartbeat::ManifestDigest>,
    /// ADR 0018 Phase C: when set, the picker excludes `exclude_host`
    /// from the candidate pool. Used by the evacuation paths to
    /// guarantee a relocate never targets the source host —
    /// degenerate for dead-source (source is already unregistered)
    /// but load-bearing for the NBD-loss path and operator-initiated
    /// drain, where the source is still registered but should not be
    /// reselected.
    pub exclude_host: Option<HostId>,
}

/// Why `pick_for_session` couldn't place a session. Distinguishes
/// "image isn't prefetched anywhere yet" from "no host has
/// capacity" so the API layer can return distinct 503 reasons.
#[derive(Clone, Debug)]
pub enum PickError {
    /// No host's heartbeat has reported this digest in
    /// `ready_images`. Operator action: wait for the host
    /// prefetch loop to finish (typically seconds-to-minutes
    /// depending on chunk-store warmth + bandwidth), or
    /// investigate if BlobStorage egress is the bottleneck.
    ImageNotReady(engram_protocol::heartbeat::ManifestDigest),
    /// At least one host is ready for the image (or no digest
    /// was required) but none has free capacity matching the
    /// memory hint or is non-draining.
    NoCapacity,
}

/// Fleet-wide autoscaling signals (ADR 0044 K4). Read off the in-memory
/// registry on a tick; emitted as the `engram_fleet_*` gauges the node-pool
/// autoscaler scales on.
#[derive(Clone, Copy, Debug, Default)]
pub struct FleetMetrics {
    /// Hosts with a live heartbeat in the registry.
    pub ready_hosts: u32,
    /// Non-draining hosts the scheduler can place on.
    pub schedulable_hosts: u32,
    /// Σ (total_mib − used_mib) over schedulable hosts.
    pub free_mib: u64,
    /// Σ total_mib over schedulable hosts — lets a consumer derive the
    /// average per-host capacity (how much one node adds).
    pub total_mib: u64,
}

impl From<PickError> for SandboxError {
    fn from(e: PickError) -> Self {
        match e {
            PickError::ImageNotReady(d) => SandboxError::ImageNotReady(d.as_str().to_string()),
            PickError::NoCapacity => {
                SandboxError::Vm("no host has free capacity for this session".into())
            }
        }
    }
}

/// ADR 0015 M3: the in-memory routing cache, made coherent with the
/// authoritative `sessions` row in Postgres.
///
/// The previous shape kept `sandbox_id → host_id` purely in memory;
/// on host loss the entry persisted until the heartbeat-timeout path
/// dropped the whole host, leaving a window where RPCs against a
/// stale `SandboxId` got `tcp connect error`s from the gRPC pool or
/// a generic `sandbox not found` from the registry timeout. M2 made
/// the *PG* side of the mapping clean (sessions march through
/// `HostLost` and `sessions.sandbox_id` is nulled), but didn't touch
/// the cache.
///
/// M3 closes the loop:
///
/// 1. **Strict invalidation** — the per-session HostLost transition
///    sites (`reconcile`, `preemption_drain`) call
///    [`HostRegistry::invalidate_sandbox`] before the DB-side state
///    flip, and [`HostRegistry::unregister`] now purges every
///    `sandbox_owner` row pointing at the dying host, not just the
///    `hosts` entry itself.
/// 2. **Read-through on miss** — [`HostRegistry::resolve_owner`]
///    falls back to `MetadataStore::host_for_sandbox` on a cache
///    miss, repairs the entry if PG knows a current owner, and
///    returns [`SandboxError::HostLost`] (→ 410 Gone) when the
///    session is in a HostLost-class state.
/// 3. **Fallback TTL** — each host entry tracks
///    `last_observed_heartbeat`; entries past
///    [`HostRegistry::ttl`] are soft-invalidated on read, so an
///    operator-paused host (no heartbeats, dead-host detector also
///    paused) doesn't keep serving stale routes forever.
pub struct HostRegistry {
    hosts: DashMap<HostId, HostEntry>,
    /// Sandbox-to-host ownership map. Populated on `create` / `restore`,
    /// consulted by every method that takes an existing `SandboxId`.
    /// Without this the registry can't route `exec_stream(id)` because
    /// `id` is local to the host that produced it. M3 makes this a
    /// strict cache: invalidation on HostLost + read-through to PG
    /// on miss.
    sandbox_owner: DashMap<SandboxId, HostId>,
    /// PG-authoritative store consulted on cache miss. Driven through
    /// `MetadataStore::host_for_sandbox` (ADR 0015 M3).
    meta: Arc<dyn MetadataStore>,
    /// Soft-invalidation horizon for `last_observed_heartbeat`. Entries
    /// older than this fall through to a PG read on the next
    /// `resolve_owner`. Configurable via `ENGRAM_HOST_REGISTRY_TTL_SECS`;
    /// default 60s — well over the 5s heartbeat cadence but under
    /// the dead-host detector's 30s threshold, so the TTL only fires
    /// when *detection itself* is broken or paused.
    ttl: Duration,
}

impl HostRegistry {
    /// Construct a registry backed by `meta` for read-through cache
    /// misses. TTL is read from `ENGRAM_HOST_REGISTRY_TTL_SECS`
    /// (default 60s).
    pub fn new(meta: Arc<dyn MetadataStore>) -> Self {
        let ttl_secs = std::env::var("ENGRAM_HOST_REGISTRY_TTL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60);
        Self {
            hosts: DashMap::new(),
            sandbox_owner: DashMap::new(),
            meta,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Test-only: construct a registry whose read-through path uses
    /// `meta` and whose TTL is `ttl`. Lets the M3 TTL-fallback tests
    /// stage "host went silent" without sleeping for a real minute.
    #[cfg(test)]
    pub fn new_with_ttl(meta: Arc<dyn MetadataStore>, ttl: Duration) -> Self {
        Self {
            hosts: DashMap::new(),
            sandbox_owner: DashMap::new(),
            meta,
            ttl,
        }
    }

    /// Register a host. Replaces any prior registration for the same id
    /// (host reconnect after a network blip uses the same `HostId`).
    /// In production, `backend` is a `GrpcHostClient` from
    /// `state.services.host_pool` (ADR 0013); in `--mode=all` it's
    /// the in-proc `LocalHostClient`.
    pub fn register(&self, host_id: HostId, backend: Arc<dyn HostClient>) {
        self.hosts.insert(
            host_id,
            HostEntry {
                backend,
                state: RwLock::new(HostState::default()),
                last_observed_heartbeat: AtomicI64::new(now_millis()),
            },
        );
    }

    /// Whether this host id has an entry. Used by the heartbeat
    /// handler to decide whether to do an idempotent re-register
    /// (coord-restart self-heal, ADR 0013).
    pub fn contains(&self, host_id: HostId) -> bool {
        self.hosts.contains_key(&host_id)
    }

    /// Update a host's heartbeat-derived state. Called by the WS
    /// supervisor on each inbound `Heartbeat`; the scheduler reads
    /// the most recent value when picking a host. Also bumps
    /// `last_observed_heartbeat` so the M3 TTL path sees the host as
    /// fresh on the next `resolve_owner`.
    pub fn update_state(&self, host_id: HostId, state: HostState) {
        if let Some(entry) = self.hosts.get(&host_id) {
            *entry.value().state.write() = state;
            entry
                .value()
                .last_observed_heartbeat
                .store(now_millis(), Ordering::Relaxed);
        }
    }

    /// Snapshot the current scheduler view. Used by `GET /api/hosts`
    /// and tests; the live state is mutated by the heartbeat supervisor
    /// so callers should treat the returned `HostState` as a snapshot.
    pub fn snapshot_state(&self, host_id: HostId) -> Option<HostState> {
        self.hosts
            .get(&host_id)
            .map(|e| e.value().state.read().clone())
    }

    /// ADR 0014 M1.11: snapshot every host's state at once. Used by
    /// the `POST /api/enabled-images` synchronous-fill path to poll
    /// for warm-slot availability without per-host individual reads.
    /// Returned vec ordered by `HostId` for determinism in tests.
    pub fn snapshot_all_states(&self) -> Vec<(HostId, HostState)> {
        let mut out: Vec<(HostId, HostState)> = self
            .hosts
            .iter()
            .map(|e| (*e.key(), e.value().state.read().clone()))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Drop a host and every sandbox-ownership row that pointed at
    /// it. Atomic for the caller's intent: post-call the registry
    /// has zero stale references to `host_id`, so a subsequent
    /// `resolve_owner(sb)` for any of those sandboxes either repairs
    /// via PG (if the row got re-bound to a new host — M4 migration
    /// path) or returns [`SandboxError::HostLost`].
    ///
    /// ADR 0015 M3: the previous shape only dropped `hosts.host_id`
    /// and left `sandbox_owner` rows behind on the theory they were
    /// "meaningless but cheap" — that theory was wrong once
    /// `resolve_owner` started returning typed 410s on the basis of
    /// "cache says known-owner but the owner is gone". The sweep
    /// here is O(sandbox_owner.len) — acceptable given the map is
    /// bounded by concurrent active sandboxes (low thousands in
    /// prod) and `unregister` only fires on host death.
    pub fn unregister(&self, host_id: HostId) {
        self.hosts.remove(&host_id);
        self.sandbox_owner.retain(|_sb, owner| *owner != host_id);
    }

    /// ADR 0015 M3: drop the cached owner for a single sandbox.
    /// Called from the per-session HostLost transition sites
    /// (`reconcile::flip_missing`, `preemption_drain`) once the DB
    /// has cleared `sessions.sandbox_id` — the order is
    /// "drop-cache-then-DB" on the in-memory side and
    /// "clear-DB-then-flip-status" on the persistence side, so any
    /// reader that races us either gets the stale cache entry (and
    /// reaches a backend that's about to error), the read-through
    /// PG path (and learns the session is HostLost → 410), or a
    /// clean miss (→ 410 via the no-owner branch). Returns the
    /// prior owner for logging.
    pub fn invalidate_sandbox(&self, sandbox_id: SandboxId) -> Option<HostId> {
        self.sandbox_owner.remove(&sandbox_id).map(|(_, h)| h)
    }

    /// Currently-connected host count. Used by `/healthz` and by tests.
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// ADR 0018 commit 12d: mark a host non-schedulable. Flips the
    /// in-memory `HostState.draining` flag, which
    /// [`Self::pick_for_session`]'s `host_is_ready` closure already
    /// uses to filter the host out of scheduling. PG-side
    /// `hosts.status` is updated separately by the caller via
    /// `MetadataStore::set_host_status(Draining)` — the in-memory and
    /// PG sides are deliberately decoupled here so callers that only
    /// hold a `HostRegistry` (tests, mock harnesses) don't pay for a
    /// DB round-trip. The admin endpoint (`api::admin::cordon_host`)
    /// pairs the two writes.
    ///
    /// Returns `true` if the host was registered and was flipped,
    /// `false` if no entry exists (operator targeted an unregistered
    /// host — caller surfaces 404).
    pub fn cordon(&self, host_id: HostId) -> bool {
        match self.hosts.get(&host_id) {
            Some(entry) => {
                entry.value().state.write().draining = true;
                true
            }
            None => false,
        }
    }

    /// Inverse of [`Self::cordon`]. Returns the host to schedulable.
    /// Heartbeat updates would overwrite `draining` on the next tick
    /// anyway, but the explicit uncordon lets operators flip it
    /// immediately without waiting for the host to re-report.
    pub fn uncordon(&self, host_id: HostId) -> bool {
        match self.hosts.get(&host_id) {
            Some(entry) => {
                entry.value().state.write().draining = false;
                true
            }
            None => false,
        }
    }

    /// ADR 0018 commit 12d: enumerate every sandbox owned by `host_id`
    /// in the in-memory routing cache. Paired with PG-side
    /// `list_active_sandbox_assignments_on_host` for the admin /drain
    /// endpoint — PG is authoritative for "which sessions live here,"
    /// but the in-memory map carries the SandboxId we feed to the
    /// evict pipeline.
    pub fn sandboxes_on_host(&self, host_id: HostId) -> Vec<SandboxId> {
        self.sandbox_owner
            .iter()
            .filter_map(|e| {
                if *e.value() == host_id {
                    Some(*e.key())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Backend `Arc` for `host_id`, or `None` if not registered. Used
    /// by the ADR 0009 in-process reconcile driver (`reconcile::spawn_in_proc`)
    /// to call `backend.list()` on each tick. Cloning the trait
    /// object is cheap (Arc bump) so callers don't have to hold the
    /// registry's internal entry across `await` points.
    pub fn backend_of(&self, host_id: HostId) -> Option<Arc<dyn HostClient>> {
        self.hosts.get(&host_id).map(|e| e.value().backend.clone())
    }

    pub fn host_ids(&self) -> Vec<HostId> {
        self.hosts.iter().map(|e| *e.key()).collect()
    }

    /// Trivial Phase-3a scheduler: pick any non-draining registered
    /// host. Used by the `SandboxBackend` trait impl for anonymous
    /// flows (list, restore from a global metadata) where the caller
    /// hasn't supplied session-aware [`ScheduleContext`].
    fn pick_any(&self) -> Option<(HostId, Arc<dyn HostClient>)> {
        self.hosts
            .iter()
            .find(|e| !e.value().state.read().draining)
            .or_else(|| self.hosts.iter().next())
            .map(|e| (*e.key(), e.value().backend.clone()))
    }

    /// ADR 0020 P1: pick any non-draining host to run a base-snapshot
    /// capture on. Unlike session placement this is NOT gated on image
    /// readiness — on first enable no host has prefetched the image yet,
    /// so the capture host lazy-materializes the rootfs from BlobStorage
    /// during its boot. Public wrapper over `pick_any` for the
    /// enable-image handler.
    pub fn pick_capture_host(&self) -> Option<(HostId, Arc<dyn HostClient>)> {
        self.pick_any()
    }

    /// Session scheduler. Inputs the per-host heartbeat state (capacity,
    /// local snapshots, draining, ready_images) and ranks:
    ///
    /// 0. ADR 0015 M5: when `ctx.required_image_digest` is set,
    ///    restrict candidates to hosts that have prefetched that
    ///    digest. No ready host → `Err(ImageNotReady)` (surfaces
    ///    as HTTP 503 distinct from "no capacity").
    /// 1. Host with `prefer_snapshot_id` in `local_snapshots` —
    ///    zero-cost hot-tier hit.
    /// 2. Host with the largest free capacity (`total - used`),
    ///    filtered against `memory_mib` if provided.
    /// 3. Else any non-draining host.
    /// 4. Else `Err(NoCapacity)` — coordinator surfaces a 503.
    pub fn pick_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
    ) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
        // ADR 0044 K4: emit the demand-pressure signal at the one place every
        // scheduling decision funnels through.
        let result = self.pick_for_session_inner(ctx);
        let outcome = match &result {
            Ok(_) => "placed",
            Err(PickError::NoCapacity) => "no_capacity",
            Err(PickError::ImageNotReady(_)) => "image_not_ready",
        };
        ::metrics::counter!(crate::metrics::SESSION_PLACEMENT_TOTAL, "outcome" => outcome)
            .increment(1);
        result
    }

    fn pick_for_session_inner(
        &self,
        ctx: &ScheduleContext<'_>,
    ) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
        // Closure encoding "this host is a viable candidate".
        // Non-draining + (no digest required OR ready for the digest)
        // + not the excluded host (ADR 0018 Phase C evac guard).
        let host_is_ready = |host_id: HostId, st: &HostState| {
            if Some(host_id) == ctx.exclude_host {
                return false;
            }
            if st.draining {
                return false;
            }
            match ctx.required_image_digest.as_ref() {
                Some(d) => st.ready_images.contains(d),
                None => true,
            }
        };
        let any_ready = self
            .hosts
            .iter()
            .any(|e| host_is_ready(*e.key(), &e.value().state.read()));
        if !any_ready {
            return match ctx.required_image_digest.as_ref() {
                Some(d) => Err(PickError::ImageNotReady(d.clone())),
                None => Err(PickError::NoCapacity),
            };
        }

        // Snapshot-affinity: any READY host carrying the requested snapshot.
        if let Some(target) = ctx.prefer_snapshot_id {
            for entry in self.hosts.iter() {
                let st = entry.value().state.read();
                if !host_is_ready(*entry.key(), &st) {
                    continue;
                }
                if st.local_snapshots.iter().any(|s| s.snapshot_id == target) {
                    return Ok((*entry.key(), entry.value().backend.clone()));
                }
            }
        }

        // Capacity-fit: largest free RAM among ready hosts that
        // meets `memory_mib`.
        let need = ctx.memory_mib.unwrap_or(0) as u64;
        let mut best_cap: Option<(u64, HostId, Arc<dyn HostClient>)> = None;
        for entry in self.hosts.iter() {
            let st = entry.value().state.read();
            if !host_is_ready(*entry.key(), &st) {
                continue;
            }
            let free = st.capacity.total_mib.saturating_sub(st.capacity.used_mib);
            if free < need {
                continue;
            }
            let candidate = (free, *entry.key(), entry.value().backend.clone());
            best_cap = match best_cap {
                Some((cur, _, _)) if cur >= free => best_cap,
                _ => Some(candidate),
            };
        }
        if let Some((_, host_id, backend)) = best_cap {
            return Ok((host_id, backend));
        }

        // Fallback: ready host with no capacity report yet (first
        // heartbeat hasn't landed). Pick any ready host so brand-
        // new hosts can take work without waiting a tick.
        self.hosts
            .iter()
            .find(|e| host_is_ready(*e.key(), &e.value().state.read()))
            .map(|e| (*e.key(), e.value().backend.clone()))
            .ok_or(PickError::NoCapacity)
    }

    /// Fleet-wide autoscaling signals (ADR 0044 K4), sampled on a tick and
    /// emitted as gauges. `schedulable_hosts` excludes draining hosts;
    /// `free_mib` is the spare guest-RAM reservation across them.
    pub fn fleet_metrics(&self) -> FleetMetrics {
        let mut m = FleetMetrics::default();
        for entry in self.hosts.iter() {
            m.ready_hosts += 1;
            let st = entry.value().state.read();
            if !st.draining {
                m.schedulable_hosts += 1;
                m.free_mib += st.capacity.total_mib.saturating_sub(st.capacity.used_mib);
                m.total_mib += st.capacity.total_mib;
            }
        }
        m
    }

    /// Pick a host for `ctx`, then `restore` from `metadata` on that
    /// host's backend. Routes to the host carrying the snapshot if any;
    /// else falls through to capacity-based pick. ADR 0007 Phase 6: takes
    /// a `SnapshotMetadata` directly (backends look up their own
    /// per-snapshot staging dir from the chunked manifest refs). Caller
    /// is responsible for `assign_session_host`.
    #[tracing::instrument(name = "coord.restore_for_session", skip_all)]
    pub async fn restore_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        metadata: SnapshotMetadata,
    ) -> Result<(HostId, SandboxId), SandboxError> {
        let (host_id, backend) = self.pick_for_session(ctx)?;
        let sandbox_id = backend.restore(metadata).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok((host_id, sandbox_id))
    }

    /// ADR 0020 P1: restore a per-image base snapshot for a fresh
    /// session, late-binding the session harness (option-D swap). Like
    /// `restore_for_session` but the picked host runs the combined
    /// restore + harness-swap op so `create_session` can route a cold
    /// create through restore instead of a fresh kernel boot.
    #[tracing::instrument(name = "coord.restore_base_for_session", skip_all)]
    pub async fn restore_base_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
    ) -> Result<(HostId, SandboxId), SandboxError> {
        let (host_id, backend) = self.pick_for_session(ctx)?;
        let sandbox_id = backend
            .restore_base_for_session(metadata, session_env)
            .await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok((host_id, sandbox_id))
    }

    /// Look up the host that owns `sandbox_id`. Used by 3d migration
    /// and by anyone who wants to know which host an existing session
    /// landed on.
    pub fn host_of(&self, sandbox_id: SandboxId) -> Option<HostId> {
        self.sandbox_owner.get(&sandbox_id).map(|r| *r.value())
    }

    /// Pre-populate the `sandbox_id → host_id` map without going
    /// through `create()`. Used by the coordinator's startup
    /// `repopulate_routing` step to rebuild routing from the
    /// persisted `sessions.sandbox_id`/`sessions.host_id` columns
    /// after a restart. Once the host dials back in and registers
    /// its `RemoteSandboxBackend`, routing for these pre-seeded
    /// sandboxes resumes without further coordination.
    pub fn record_sandbox_owner(&self, sandbox_id: SandboxId, host_id: HostId) {
        self.sandbox_owner.insert(sandbox_id, host_id);
    }

    /// ADR 0015 M3 read-through cache lookup.
    ///
    /// Fast path: cache hit + host registered + host's
    /// `last_observed_heartbeat` is within `self.ttl`. No DB
    /// round-trip. This is the steady-state shape — every
    /// well-behaved RPC against an `Active` session.
    ///
    /// Slow path (cache miss, or TTL-expired host entry, or stale
    /// cache row pointing at a host the registry no longer knows
    /// about): ask `MetadataStore::host_for_sandbox` who owns the
    /// sandbox now. Outcomes:
    ///
    /// - PG returns `(host_id, session_status)` where the status is
    ///   a HostLost-class state (`HostLost`, `Dead`, `Completed`,
    ///   `Failed`) → return [`SandboxError::HostLost`]. The cache
    ///   row, if any, gets pruned first so future reads don't keep
    ///   hitting the fast path.
    /// - PG returns a current owner whose host *is* registered and
    ///   fresh → repair the cache row and return the backend.
    /// - PG returns a current owner whose host *isn't* registered
    ///   (or is past TTL) → [`SandboxError::HostLost`]: the row
    ///   says the host should be reachable, but the registry can't
    ///   reach it.
    /// - PG returns `None` → [`SandboxError::NotFound`]: this
    ///   sandbox id was never bound to a session, or its session
    ///   row has been hard-deleted.
    ///
    /// If the PG call itself errors, the registry falls back to
    /// `NotFound` — surfacing an internal error as a 404 here is
    /// the lesser evil than mapping to 410 (which would lie about
    /// the host being gone) or 500 (which doesn't compose with the
    /// API layer's `From<SandboxError>` mapping). The error is
    /// logged at WARN.
    pub async fn resolve_owner(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<(HostId, Arc<dyn HostClient>), SandboxError> {
        // Fast path.
        if let Some(host_id) = self.sandbox_owner.get(&sandbox_id).map(|r| *r.value()) {
            if let Some(entry) = self.hosts.get(&host_id) {
                if self.entry_is_fresh(&entry) {
                    return Ok((host_id, entry.value().backend.clone()));
                }
            }
            // Cache row exists but its host is missing or past TTL.
            // Drop the stale row before consulting PG so a concurrent
            // reader doesn't keep taking the in-memory path.
            self.sandbox_owner.remove(&sandbox_id);
        }

        // Read-through to PG.
        let pg = match self.meta.host_for_sandbox(sandbox_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "host_for_sandbox lookup failed; surfacing as NotFound"
                );
                return Err(SandboxError::NotFound);
            }
        };
        let Some((host_id, status)) = pg else {
            return Err(SandboxError::NotFound);
        };
        if Self::is_host_lost_status(status) {
            return Err(SandboxError::HostLost);
        }
        // Active-class session in PG. Repair the cache iff we have a
        // fresh host entry to back it.
        let Some(entry) = self.hosts.get(&host_id) else {
            return Err(SandboxError::HostLost);
        };
        if !self.entry_is_fresh(&entry) {
            return Err(SandboxError::HostLost);
        }
        let backend = entry.value().backend.clone();
        drop(entry);
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok((host_id, backend))
    }

    fn entry_is_fresh(&self, entry: &dashmap::mapref::one::Ref<'_, HostId, HostEntry>) -> bool {
        let last = entry
            .value()
            .last_observed_heartbeat
            .load(Ordering::Relaxed);
        let now = now_millis();
        let ttl_millis = self.ttl.as_millis() as i64;
        now.saturating_sub(last) <= ttl_millis
    }

    fn is_host_lost_status(status: SessionState) -> bool {
        matches!(
            status,
            SessionState::HostLost
                | SessionState::Dead
                | SessionState::Completed
                | SessionState::Failed
        )
    }

    fn no_host_error() -> SandboxError {
        SandboxError::Vm(Box::new(StringError(
            "no hosts connected to the coordinator".into(),
        )))
    }
}

#[derive(Debug)]
struct StringError(String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StringError {}

#[async_trait]
impl HostClient for HostRegistry {
    // Capability methods report a deployment-wide answer — they're
    // queried before a per-session host pick happens (e.g. by
    // `resolve_harness`), so we assume all registered hosts agree on
    // the answer. In --mode=all there's exactly one host; in
    // --mode=coordinator with mixed-backend hosts the first host wins.
    fn harness_dial(&self) -> HarnessDial {
        self.hosts
            .iter()
            .next()
            .map(|entry| entry.value().backend.harness_dial())
            .unwrap_or(HarnessDial::Vsock)
    }

    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let (host_id, backend) = self.pick_any().ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.create(spec).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok(sandbox_id)
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.exec_stream(id, cmd).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.snapshot(id).await
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.commit_snapshot(id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.abort_snapshot(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        let (host_id, backend) = self.pick_any().ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(metadata).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok(sandbox_id)
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        let result = backend.destroy(id).await;
        // Drop the ownership row regardless — even if destroy errors,
        // the id is no longer routable to anything sensible. A future
        // call against the same id will fail with NotFound, which is
        // the correct semantics.
        self.sandbox_owner.remove(&id);
        result
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.start_agent(id, agent, policy).await
    }

    async fn apply_egress_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(policy.sandbox_id).await?;
        backend.apply_egress_policy(policy).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        // Aggregate across all connected hosts. Errors from any one
        // host are surfaced; partial results aren't reported in 3a.
        let mut all = Vec::new();
        for entry in self.hosts.iter() {
            let ids = entry.value().backend.list().await?;
            all.extend(ids);
        }
        Ok(all)
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        let (_, backend) = self.resolve_owner(id).await.ok()?;
        backend.guest_ip(id).await
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        if let Ok((_, backend)) = self.resolve_owner(sandbox_id).await {
            backend.bind_session(session_id, sandbox_id).await;
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        // No sandbox routing for the unbind — fan out to every
        // connected host so whichever one had the binding clears it.
        // Each host's `unbind_session` is a no-op for unknown session
        // ids, so the broadcast is cheap.
        for entry in self.hosts.iter() {
            entry.value().backend.unbind_session(session_id).await;
        }
    }

    async fn send_prompt(&self, sandbox_id: SandboxId, text: String) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.send_prompt(sandbox_id, text).await
    }

    async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.interrupt(sandbox_id).await
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.acquire_shell(sandbox_id).await
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.release_shell(sandbox_id).await
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.proxy_shell(sandbox_id).await
    }

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        // Fan out to every registered host. `LocalHostClient` wires
        // it onto its inner VMM backend; `RemoteHostClient`'s default
        // no-op silently drops the closure (the remote host owns its
        // own local sink wiring).
        for entry in self.hosts.iter() {
            entry.value().backend.set_harness_sink(sink.clone());
        }
    }

    fn set_forge_sink(&self, sink: engram_core::traits::ForgeSink) {
        // Fan out to every registered host, same as `set_harness_sink`.
        for entry in self.hosts.iter() {
            entry.value().backend.set_forge_sink(sink.clone());
        }
    }

    fn set_upload_sink(&self, sink: engram_core::traits::UploadSink) {
        // Fan out to every registered host, same as `set_forge_sink`.
        for entry in self.hosts.iter() {
            entry.value().backend.set_upload_sink(sink.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use parking_lot::Mutex as PlMutex;

    /// Minimal `MetadataStore` for HostRegistry unit tests. The
    /// existing scheduler / routing tests never trigger the M3
    /// read-through path (the cache fast path always hits because
    /// the host is freshly registered with a fresh
    /// `last_observed_heartbeat`), so we only need a real impl of
    /// `host_for_sandbox`. The M3 cache-miss tests below override
    /// the slot with a richer entry.
    #[derive(Default)]
    struct StubMeta {
        sandbox_to_session: PlMutex<std::collections::HashMap<SandboxId, (HostId, SessionState)>>,
    }
    #[allow(dead_code)]
    impl StubMeta {
        fn bind(&self, sb: SandboxId, host: HostId, status: SessionState) {
            self.sandbox_to_session.lock().insert(sb, (host, status));
        }
    }
    #[async_trait]
    impl engram_core::traits::MetadataStore for StubMeta {
        async fn create_session(
            &self,
            _: engram_core::types::session::SessionSpec,
        ) -> Result<engram_core::SessionId, engram_core::MetaError> {
            unreachable!("host_registry tests don't create sessions")
        }
        async fn create_session_created(
            &self,
            _: engram_core::SessionId,
            _: engram_core::types::session::SessionSpec,
            _: HostId,
            _: SandboxId,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn get_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<engram_core::types::Session, engram_core::MetaError> {
            Err(engram_core::MetaError::NotFound)
        }
        async fn list_active_sessions(
            &self,
        ) -> Result<Vec<engram_core::types::Session>, engram_core::MetaError> {
            Ok(Vec::new())
        }
        async fn transition_session(
            &self,
            _: engram_core::SessionId,
            _: SessionState,
        ) -> Result<SessionState, engram_core::MetaError> {
            unreachable!()
        }
        async fn assign_session_host(
            &self,
            _: engram_core::SessionId,
            _: Option<HostId>,
        ) -> Result<(), engram_core::MetaError> {
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            _: engram_core::SessionId,
            _: Option<SandboxId>,
        ) -> Result<(), engram_core::MetaError> {
            Ok(())
        }
        async fn host_for_sandbox(
            &self,
            sandbox_id: SandboxId,
        ) -> Result<Option<(HostId, SessionState)>, engram_core::MetaError> {
            Ok(self.sandbox_to_session.lock().get(&sandbox_id).copied())
        }
        async fn upsert_host(
            &self,
            _: engram_core::types::host::HostRecord,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn list_active_hosts(
            &self,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, engram_core::MetaError> {
            Ok(Vec::new())
        }
        async fn set_host_status(
            &self,
            _: HostId,
            _: engram_core::types::host::HostStatus,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::host::HostStatus,
            _: engram_core::types::host::HostCapacity,
            _: engram_core::types::host::HostUtilization,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn list_stale_hosts(
            &self,
            _: u64,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, engram_core::MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<(engram_core::SessionId, SessionState)>, engram_core::MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(
            &self,
            _: engram_core::types::snapshot::SnapshotRecord,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn list_snapshots_for_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Vec<engram_core::types::snapshot::SnapshotRecord>, engram_core::MetaError>
        {
            Ok(Vec::new())
        }
        async fn latest_snapshot_for_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Option<engram_core::types::snapshot::SnapshotRecord>, engram_core::MetaError>
        {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _: engram_core::SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, engram_core::MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _: engram_core::SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<engram_core::types::event::PersistedEvent>, engram_core::MetaError>
        {
            Ok(Vec::new())
        }
        async fn insert_artifact(
            &self,
            _: uuid::Uuid,
            _: engram_core::SessionId,
            _: &str,
            _: &str,
            _: i64,
            _: Option<&str>,
        ) -> Result<(), engram_core::MetaError> {
            Ok(())
        }
        async fn get_artifact(
            &self,
            _: engram_core::SessionId,
            _: uuid::Uuid,
        ) -> Result<Option<engram_core::types::ArtifactRow>, engram_core::MetaError> {
            Ok(None)
        }
        async fn artifact_usage(
            &self,
            _: engram_core::SessionId,
        ) -> Result<(i64, i64), engram_core::MetaError> {
            Ok((0, 0))
        }
        async fn upsert_registry_credential(
            &self,
            _: engram_core::types::registry::RegistryCredential,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn list_registry_credentials(
            &self,
        ) -> Result<Vec<engram_core::types::registry::RegistryCredential>, engram_core::MetaError>
        {
            Ok(Vec::new())
        }
        async fn registry_credential_for_host(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::RegistryCredential>, engram_core::MetaError>
        {
            Ok(None)
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
        async fn upsert_enabled_image(
            &self,
            _: engram_core::types::registry::EnabledImage,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn list_enabled_images(
            &self,
        ) -> Result<Vec<engram_core::types::registry::EnabledImage>, engram_core::MetaError>
        {
            Ok(Vec::new())
        }
        async fn get_enabled_image(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, engram_core::MetaError>
        {
            Ok(None)
        }
        async fn get_enabled_image_any(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, engram_core::MetaError>
        {
            Ok(None)
        }
        async fn soft_delete_enabled_image(
            &self,
            _: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, engram_core::MetaError>
        {
            unreachable!()
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn upsert_session_secrets(
            &self,
            _: engram_core::types::registry::SessionSecrets,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn get_session_secrets(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Option<engram_core::types::registry::SessionSecrets>, engram_core::MetaError>
        {
            Ok(None)
        }
        async fn delete_session_secrets(
            &self,
            _: engram_core::SessionId,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
    }

    fn stub_registry() -> HostRegistry {
        HostRegistry::new(Arc::new(StubMeta::default()))
    }

    fn live_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    #[tokio::test]
    async fn create_then_destroy_routes_through_recorded_host() {
        let dir = tempfile::tempdir().unwrap();
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
        let backend: Arc<dyn HostClient> =
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(raw));
        let reg = stub_registry();
        let host = HostId::new();
        reg.register(host, backend);

        let sandbox_id = reg.create(live_spec()).await.unwrap();
        assert_eq!(reg.host_count(), 1);
        // Ownership row was inserted.
        assert_eq!(
            reg.sandbox_owner.get(&sandbox_id).map(|r| *r.value()),
            Some(host)
        );

        reg.destroy(sandbox_id).await.unwrap();
        assert!(
            reg.sandbox_owner.get(&sandbox_id).is_none(),
            "destroy must drop the ownership row",
        );
    }

    #[tokio::test]
    async fn create_with_no_hosts_errors_clearly() {
        let reg = stub_registry();
        let err = reg.create(live_spec()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no hosts connected"),
            "error must explain why: {msg}"
        );
    }

    #[tokio::test]
    async fn exec_against_unknown_sandbox_id_returns_not_found() {
        let reg = stub_registry();
        let err = reg
            .exec_stream(
                SandboxId::new(),
                ExecRequest {
                    command: vec!["echo".into()],
                    stdin: None,
                    env: Default::default(),
                    workdir: None,
                    timeout: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::NotFound));
    }

    fn dummy_backend() -> (Arc<dyn HostClient>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
        (
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(raw)),
            dir,
        )
    }

    #[test]
    fn pick_for_session_prefers_host_with_target_snapshot() {
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_no_snap = HostId::new();
        let h_with_snap = HostId::new();
        reg.register(h_no_snap, b1);
        reg.register(h_with_snap, b2);

        let snap = engram_core::SnapshotId::new();
        // Both hosts have free capacity; only one carries the snapshot.
        reg.update_state(
            h_no_snap,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 4096,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        reg.update_state(
            h_with_snap,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 1,
                },
                local_snapshots: vec![LocalSnapshotReport {
                    snapshot_id: snap,
                    session_id: engram_core::SessionId::new(),
                    size_bytes: 1,
                    replicated: false,
                    last_accessed_at: chrono::Utc::now(),
                }],
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: Some(snap),
            memory_mib: None,
            required_image_digest: None,
            exclude_host: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(
            picked, h_with_snap,
            "snapshot affinity must override raw capacity"
        );
    }

    #[test]
    fn pick_for_session_chooses_largest_free_capacity() {
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_small = HostId::new();
        let h_big = HostId::new();
        reg.register(h_small, b1);
        reg.register(h_big, b2);

        reg.update_state(
            h_small,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        reg.update_state(
            h_big,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_big, "larger free capacity wins");
    }

    #[test]
    fn pick_for_session_skips_draining_hosts() {
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_drain = HostId::new();
        let h_ready = HostId::new();
        reg.register(h_drain, b1);
        reg.register(h_ready, b2);

        // Draining host has more free capacity, but shouldn't be picked.
        reg.update_state(
            h_drain,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: true,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        reg.update_state(
            h_ready,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_ready);
    }

    /// ADR 0018 Phase C: `exclude_host` filters the candidate set.
    /// Load-bearing for the NBD-loss evac trigger — the source host
    /// is still registered + non-draining but should never be picked
    /// as the relocate target.
    #[test]
    fn pick_for_session_excludes_named_host() {
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let source = HostId::new();
        let peer = HostId::new();
        reg.register(source, b1);
        reg.register(peer, b2);

        // Source has FAR more free capacity than peer; without the
        // exclude_host filter, capacity-ranking would pick it.
        reg.update_state(
            source,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 64_000,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        reg.update_state(
            peer,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: Some(source),
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(
            picked, peer,
            "exclude_host must drop the source from candidates"
        );
    }

    /// `exclude_host` with no other candidates → NoCapacity. The evac
    /// caller (the `evac_resumer` scanner, draining off the excluded
    /// host) interprets this as `NoTargetAvailable` and leaves the
    /// session at `Evacuating` for a later retry.
    #[test]
    fn pick_for_session_with_only_excluded_host_errors() {
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let lone = HostId::new();
        reg.register(lone, b1);
        reg.update_state(
            lone,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 64_000,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: Some(lone),
        };
        let res = reg.pick_for_session(&ctx);
        match res {
            Err(PickError::NoCapacity) => {}
            Err(other) => panic!("expected NoCapacity, got {other:?}"),
            Ok(_) => panic!("expected an error when the only host is excluded"),
        }
    }

    #[tokio::test]
    async fn record_sandbox_owner_lets_existing_id_route_post_restart() {
        // Simulates the post-restart flow: the coordinator runs
        // `repopulate_routing` from sessions.sandbox_id/host_id and
        // calls `record_sandbox_owner` to seed the in-memory map.
        // The host then dials back in and `register`s. From that
        // point, exec/snapshot/destroy on the pre-existing sandbox_id
        // route to the right backend without going through `create`.
        //
        // ADR 0015 M3: routing is PG-authoritative now. The pre-
        // restart binding lives on the `sessions` row in Postgres
        // (that's what `repopulate_routing` reads from); the cache
        // is a fast path, not the source of truth. We seed the stub
        // accordingly so a host-still-missing lookup falls through
        // to PG and surfaces HostLost, and a host-back lookup hits
        // PG and repairs the cache.
        let stub = Arc::new(StubMeta::default());
        let host = HostId::new();
        let sandbox = SandboxId::new();
        stub.bind(sandbox, host, SessionState::Active);
        let reg = HostRegistry::new(stub.clone() as Arc<dyn MetadataStore>);

        // Pre-restart state: rebuild the in-memory cache from the
        // PG row, mirroring what repopulate_routing does.
        reg.record_sandbox_owner(sandbox, host);

        // Host hasn't dialed back yet — the cache fast path sees
        // "owner not registered", falls through to PG, sees PG also
        // says the host is the binding owner, and returns HostLost
        // because we can't reach it.
        let err = reg
            .destroy(sandbox)
            .await
            .expect_err("no backend registered yet");
        assert!(
            matches!(err, SandboxError::HostLost),
            "expected HostLost from PG read-through, got {err:?}",
        );

        // Host re-registers (same host_id). Now routing works: the
        // cache fast path missed last time (we cleared the stale
        // row), but PG repairs it on the next read.
        let dir = tempfile::tempdir().unwrap();
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
        let backend: Arc<dyn HostClient> =
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(raw));
        reg.register(host, backend);

        // The (idempotent) destroy on a ProcessBackend with an unknown
        // sandbox_id returns Ok — the wire round-trip we care about.
        reg.destroy(sandbox).await.expect("routing reaches backend");
    }

    #[test]
    fn pick_for_session_with_no_state_returns_any_non_draining_host() {
        // First-heartbeat-not-yet-arrived case: registered host has
        // default-zero state. Scheduler still picks it as a fallback
        // so brand-new hosts can take work immediately.
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let h = HostId::new();
        reg.register(h, b1);

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h);
    }

    // ADR 0015 M5: readiness-gate scheduler tests.

    #[test]
    fn pick_for_session_requires_digest_in_ready_images() {
        // Two hosts both have capacity; only one reports the
        // required digest in `ready_images`. The scheduler must
        // route to that host even if the other has more free RAM.
        use engram_protocol::heartbeat::ManifestDigest;
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_unready = HostId::new();
        let h_ready = HostId::new();
        reg.register(h_unready, b1);
        reg.register(h_ready, b2);

        let digest = ManifestDigest::new("sha256:abcd");
        let mut ready_set = std::collections::HashSet::new();
        ready_set.insert(digest.clone());

        reg.update_state(
            h_unready,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        reg.update_state(
            h_ready,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1_024,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: ready_set,
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: Some(digest),
            exclude_host: None,
        };
        let (picked, _) = match reg.pick_for_session(&ctx) {
            Ok(v) => v,
            Err(e) => panic!("expected pick to succeed, got {e:?}"),
        };
        assert_eq!(
            picked, h_ready,
            "readiness gate must beat raw capacity affinity",
        );
    }

    #[test]
    fn pick_for_session_returns_image_not_ready_when_no_host_carries_digest() {
        // Two hosts, neither has prefetched the requested image.
        // The scheduler must surface `ImageNotReady` carrying the
        // missing digest, not fall back to a non-ready host.
        use engram_protocol::heartbeat::ManifestDigest;
        let reg = stub_registry();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h1 = HostId::new();
        let h2 = HostId::new();
        reg.register(h1, b1);
        reg.register(h2, b2);

        let digest = ManifestDigest::new("sha256:missing");
        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: Some(digest.clone()),
            exclude_host: None,
        };
        match reg.pick_for_session(&ctx) {
            Err(PickError::ImageNotReady(d)) => assert_eq!(d, digest),
            Err(PickError::NoCapacity) => panic!("expected ImageNotReady, got NoCapacity"),
            Ok(_) => panic!("expected ImageNotReady, got Ok"),
        }
    }

    #[test]
    fn pick_for_session_no_digest_required_still_schedules_on_any_host() {
        // Restore + admin flows pass `required_image_digest = None`;
        // the readiness gate must not interfere with them.
        let reg = stub_registry();
        let (b, _d) = dummy_backend();
        let h = HostId::new();
        reg.register(h, b);

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: None,
            exclude_host: None,
        };
        let (picked, _) = match reg.pick_for_session(&ctx) {
            Ok(v) => v,
            Err(e) => panic!("expected pick to succeed, got {e:?}"),
        };
        assert_eq!(picked, h);
    }

    // ---------- ADR 0015 M3 ----------

    #[tokio::test]
    async fn invalidate_sandbox_removes_single_entry_and_returns_prior_owner() {
        let reg = stub_registry();
        let (b, _d) = dummy_backend();
        let host = HostId::new();
        reg.register(host, b);
        let sb = SandboxId::new();
        reg.record_sandbox_owner(sb, host);

        let prev = reg.invalidate_sandbox(sb);
        assert_eq!(prev, Some(host));
        assert!(
            reg.sandbox_owner.get(&sb).is_none(),
            "the row must be gone after invalidate_sandbox",
        );
        // Second invalidate is a no-op (idempotent).
        assert_eq!(reg.invalidate_sandbox(sb), None);
    }

    #[tokio::test]
    async fn unregister_purges_every_sandbox_owner_entry_for_that_host() {
        let reg = stub_registry();
        let (b_a, _d_a) = dummy_backend();
        let (b_b, _d_b) = dummy_backend();
        let host_a = HostId::new();
        let host_b = HostId::new();
        reg.register(host_a, b_a);
        reg.register(host_b, b_b);
        let sb_a1 = SandboxId::new();
        let sb_a2 = SandboxId::new();
        let sb_b = SandboxId::new();
        reg.record_sandbox_owner(sb_a1, host_a);
        reg.record_sandbox_owner(sb_a2, host_a);
        reg.record_sandbox_owner(sb_b, host_b);

        reg.unregister(host_a);

        assert!(
            reg.sandbox_owner.get(&sb_a1).is_none(),
            "host_a's first sandbox row must be purged",
        );
        assert!(
            reg.sandbox_owner.get(&sb_a2).is_none(),
            "host_a's second sandbox row must be purged",
        );
        assert_eq!(
            reg.sandbox_owner.get(&sb_b).map(|r| *r.value()),
            Some(host_b),
            "host_b's row must survive",
        );
        assert!(!reg.contains(host_a));
        assert!(reg.contains(host_b));
    }

    #[tokio::test]
    async fn resolve_owner_falls_back_to_pg_on_cache_miss() {
        // PG knows the binding; the cache doesn't (coord restart
        // hasn't repopulated yet). resolve_owner should consult PG,
        // confirm the host is registered + fresh, and repair the
        // cache.
        let stub = Arc::new(StubMeta::default());
        let host = HostId::new();
        let sb = SandboxId::new();
        stub.bind(sb, host, SessionState::Active);
        let reg = HostRegistry::new(stub.clone());
        let (b, _d) = dummy_backend();
        reg.register(host, b);

        let (resolved_host, _backend) = reg.resolve_owner(sb).await.expect("PG fallback");
        assert_eq!(resolved_host, host);
        assert_eq!(
            reg.sandbox_owner.get(&sb).map(|r| *r.value()),
            Some(host),
            "resolve_owner must repair the cache on a successful PG hit",
        );
    }

    #[tokio::test]
    async fn resolve_owner_returns_host_lost_when_session_is_host_lost() {
        // PG says the session has moved to HostLost. The registry
        // must surface 410 (SandboxError::HostLost), not 404 — that's
        // the whole point of M3.
        let stub = Arc::new(StubMeta::default());
        let host = HostId::new();
        let sb = SandboxId::new();
        stub.bind(sb, host, SessionState::HostLost);
        let reg = HostRegistry::new(stub.clone());
        let (b, _d) = dummy_backend();
        reg.register(host, b);

        match reg.resolve_owner(sb).await {
            Err(SandboxError::HostLost) => {}
            Err(e) => panic!("expected SandboxError::HostLost, got Err({e:?})"),
            Ok(_) => panic!("expected SandboxError::HostLost, got Ok"),
        }
    }

    #[tokio::test]
    async fn resolve_owner_returns_host_lost_when_pg_host_is_unregistered() {
        // PG returns a current owner whose host isn't in the
        // registry — host hasn't dialled back in yet, or the
        // registry just got purged. Either way, 410 is correct:
        // we know who *should* serve the call and can't reach them.
        let stub = Arc::new(StubMeta::default());
        let host = HostId::new();
        let sb = SandboxId::new();
        stub.bind(sb, host, SessionState::Active);
        let reg = HostRegistry::new(stub.clone());
        // No reg.register(host, ...) — host is missing from the
        // in-memory registry.

        match reg.resolve_owner(sb).await {
            Err(SandboxError::HostLost) => {}
            Err(e) => panic!("expected SandboxError::HostLost, got Err({e:?})"),
            Ok(_) => panic!("expected SandboxError::HostLost, got Ok"),
        }
    }

    #[tokio::test]
    async fn resolve_owner_returns_not_found_when_pg_has_no_row() {
        // No row at all → genuine 404, not 410.
        let stub = Arc::new(StubMeta::default());
        let reg = HostRegistry::new(stub.clone());
        let sb = SandboxId::new();

        match reg.resolve_owner(sb).await {
            Err(SandboxError::NotFound) => {}
            Err(e) => panic!("expected SandboxError::NotFound, got Err({e:?})"),
            Ok(_) => panic!("expected SandboxError::NotFound, got Ok"),
        }
    }

    #[tokio::test]
    async fn resolve_owner_soft_invalidates_entry_past_ttl_and_consults_pg() {
        // Cache says host_a owns the sandbox; the host's
        // last_observed_heartbeat is more than TTL ago. The fast
        // path must drop to PG. In this test PG agrees with the
        // cache and the host is still registered (and stale, but
        // the act of `update_state` would have refreshed it; we
        // simulate the gap manually via the test-only TTL).
        let stub = Arc::new(StubMeta::default());
        let host = HostId::new();
        let sb = SandboxId::new();
        stub.bind(sb, host, SessionState::Active);
        // Sub-millisecond TTL → every read sees the entry as stale.
        let reg = HostRegistry::new_with_ttl(
            stub.clone() as Arc<dyn engram_core::traits::MetadataStore>,
            Duration::from_nanos(1),
        );
        let (b, _d) = dummy_backend();
        reg.register(host, b);
        reg.record_sandbox_owner(sb, host);
        // Wait a tick so the heartbeat timestamp is "in the past"
        // by more than the (~1ns) TTL on a coarse-resolution clock.
        tokio::time::sleep(Duration::from_millis(2)).await;

        // The entry is past TTL → the fast path falls through to
        // PG, which still returns the same host. Because the host
        // is also past TTL, the read-through path treats it as
        // unreachable and returns HostLost (the conservative
        // M3 contract: "an operator-paused host cannot serve").
        match reg.resolve_owner(sb).await {
            Err(SandboxError::HostLost) => {}
            Err(e) => panic!("expected SandboxError::HostLost from TTL fallback, got Err({e:?})"),
            Ok(_) => panic!("expected SandboxError::HostLost from TTL fallback, got Ok"),
        }
    }
}
