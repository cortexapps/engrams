//! Coordinator-side registry of connected hosts.
//!
//! Replaces the Phase 1+2 single in-process `Arc<dyn SandboxBackend>` —
//! each host registers a backend (typically a [`RemoteSandboxBackend`]
//! wrapping a WS connection, or in `--mode=all` a local trait object).
//! `HostRegistry` itself implements [`SandboxBackend`] by:
//!
//! 1. Routing `create` / `restore` to a scheduler-picked host and
//!    recording the resulting `SandboxId → HostId` so subsequent calls
//!    against that id reach the right host.
//! 2. Routing `exec_stream` / `snapshot` / `destroy` by looking up the
//!    `HostId` for the supplied `SandboxId`.
//! 3. Aggregating `list` across all connected hosts.
//!
//! Phase 3a single-host: the scheduler is trivial — pick the only
//! registered host. Phase 3b grows this into a real ranking with
//! snapshot affinity / warm pool / capacity inputs.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SnapshotId};
use engram_protocol::heartbeat::{HostCapacityReport, LocalSnapshotReport, WarmPoolReport};
use parking_lot::RwLock;

/// Per-host record kept in-memory. Backend dispatches RPCs over the
/// wire (or directly in `--mode=all`); the heartbeat-derived state
/// fields drive the scheduler in Phase 3b.
struct HostEntry {
    backend: Arc<dyn SandboxBackend>,
    state: RwLock<HostState>,
}

/// Heartbeat-derived view of a host. Updated by the WS supervisor
/// every ~5s; read by [`HostRegistry::pick_for_session`] when scheduling.
#[derive(Clone, Debug, Default)]
pub struct HostState {
    pub capacity: HostCapacityReport,
    pub warm_pools: Vec<WarmPoolReport>,
    pub local_snapshots: Vec<LocalSnapshotReport>,
    pub draining: bool,
}

/// Inputs the scheduler considers when picking a host. [`SandboxBackend`]
/// trait calls don't carry these, so handlers that want session-aware
/// scheduling call [`HostRegistry::create_for_session`] explicitly. The
/// trait impl uses [`HostRegistry::pick_any`] (which is the warm-pool
/// replenish path — anonymous slots, no affinity).
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
}

#[derive(Default)]
pub struct HostRegistry {
    hosts: DashMap<HostId, HostEntry>,
    /// Sandbox-to-host ownership map. Populated on `create` / `restore`,
    /// consulted by every method that takes an existing `SandboxId`.
    /// Without this the registry can't route `exec_stream(id)` because
    /// `id` is local to the host that produced it.
    sandbox_owner: DashMap<SandboxId, HostId>,
}

impl HostRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a host. Replaces any prior registration for the same id
    /// (host reconnect after a network blip uses the same `HostId`).
    pub fn register(&self, host_id: HostId, backend: Arc<dyn SandboxBackend>) {
        self.hosts.insert(
            host_id,
            HostEntry {
                backend,
                state: RwLock::new(HostState::default()),
            },
        );
    }

    /// Update a host's heartbeat-derived state. Called by the WS
    /// supervisor on each inbound `Heartbeat`; the scheduler reads
    /// the most recent value when picking a host.
    pub fn update_state(&self, host_id: HostId, state: HostState) {
        if let Some(entry) = self.hosts.get(&host_id) {
            *entry.value().state.write() = state;
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

    /// Drop a host. Sandbox ownership rows for sandboxes created by
    /// this host stay around — they're meaningless without their host
    /// but cheap to leave; they get cleaned up the next time the
    /// session migrates (Phase 3d) or the coordinator restarts.
    pub fn unregister(&self, host_id: HostId) {
        self.hosts.remove(&host_id);
    }

    /// Currently-connected host count. Used by `/healthz` and by tests.
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    pub fn host_ids(&self) -> Vec<HostId> {
        self.hosts.iter().map(|e| *e.key()).collect()
    }

    /// Trivial Phase-3a scheduler: pick any non-draining registered
    /// host. Used by the `SandboxBackend` trait impl for warm-pool
    /// replenish (anonymous slots — no session affinity).
    fn pick_any(&self) -> Option<(HostId, Arc<dyn SandboxBackend>)> {
        self.hosts
            .iter()
            .find(|e| !e.value().state.read().draining)
            .or_else(|| self.hosts.iter().next())
            .map(|e| (*e.key(), e.value().backend.clone()))
    }

    /// Phase 3b session scheduler. Inputs the per-host heartbeat state
    /// (capacity / warm pools / local snapshots / draining) and ranks:
    ///
    /// 1. Host with `prefer_snapshot_id` in `local_snapshots` — zero-
    ///    cost hot-tier hit.
    /// 2. Host with `(repo, image_version)` in `warm_pools` (highest
    ///    `ready` count wins).
    /// 3. Host with the largest free capacity (`total - used`), filtered
    ///    against `memory_mib` if provided.
    /// 4. Else any non-draining host.
    /// 5. Else `None` (the coordinator surfaces this as a 503 / typed
    ///    `BackendError::NotSupported("no host available")`).
    pub fn pick_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
    ) -> Option<(HostId, Arc<dyn SandboxBackend>)> {
        // Snapshot-affinity: any host carrying the requested snapshot.
        if let Some(target) = ctx.prefer_snapshot_id {
            for entry in self.hosts.iter() {
                let st = entry.value().state.read();
                if st.draining {
                    continue;
                }
                if st.local_snapshots.iter().any(|s| s.snapshot_id == target) {
                    return Some((*entry.key(), entry.value().backend.clone()));
                }
            }
        }

        // Warm-pool affinity: highest `ready` count for this
        // `image_version` wins. The pool's `repo` field is diagnostic
        // only — host-side `PooledBackend` keys on image_version
        // alone, so two repos sharing an image share warm slots,
        // which is the right behaviour: the pool's job is to amortise
        // create-time cost per image, not enforce per-repo isolation.
        let mut best_warm: Option<(u32, HostId, Arc<dyn SandboxBackend>)> = None;
        for entry in self.hosts.iter() {
            let st = entry.value().state.read();
            if st.draining {
                continue;
            }
            for pool in &st.warm_pools {
                if pool.image_version == ctx.image_version && pool.ready > 0 {
                    let candidate = (pool.ready, *entry.key(), entry.value().backend.clone());
                    best_warm = match best_warm {
                        Some((cur, _, _)) if cur >= pool.ready => best_warm,
                        _ => Some(candidate),
                    };
                }
            }
        }
        if let Some((_, host_id, backend)) = best_warm {
            return Some((host_id, backend));
        }

        // Capacity-fit: largest free RAM that meets `memory_mib`.
        let need = ctx.memory_mib.unwrap_or(0) as u64;
        let mut best_cap: Option<(u64, HostId, Arc<dyn SandboxBackend>)> = None;
        for entry in self.hosts.iter() {
            let st = entry.value().state.read();
            if st.draining {
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
            return Some((host_id, backend));
        }

        // Fallback: capacity reports may be empty (3a registrations,
        // or hosts that haven't sent their first heartbeat). Pick any
        // non-draining host.
        self.pick_any()
    }

    /// Pick a host for `ctx`, then call `create` on that host's
    /// backend. Caller is responsible for `assign_session_host`. Used
    /// by `api::sessions::create_session` for session-aware scheduling.
    pub async fn create_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        spec: SandboxSpec,
    ) -> Result<(HostId, SandboxId), SandboxError> {
        let (host_id, backend) = self.pick_for_session(ctx).ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.create(spec).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok((host_id, sandbox_id))
    }

    /// Same as `create_for_session` but for restoring from a snapshot.
    /// Routes to the host carrying `snapshot_id` if any; else falls
    /// through to capacity-based pick.
    pub async fn restore_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        src: std::path::PathBuf,
    ) -> Result<(HostId, SandboxId), SandboxError> {
        let (host_id, backend) = self.pick_for_session(ctx).ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(src).await?;
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

    fn lookup(&self, sandbox_id: SandboxId) -> Result<Arc<dyn SandboxBackend>, SandboxError> {
        let host_id = self
            .sandbox_owner
            .get(&sandbox_id)
            .map(|r| *r.value())
            .ok_or(SandboxError::NotFound)?;
        self.hosts
            .get(&host_id)
            .map(|e| e.value().backend.clone())
            .ok_or(SandboxError::NotFound)
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
impl SandboxBackend for HostRegistry {
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
        let backend = self.lookup(id)?;
        backend.exec_stream(id, cmd).await
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        dest: &std::path::Path,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let backend = self.lookup(id)?;
        backend.snapshot(id, dest).await
    }

    async fn restore(&self, src: std::path::PathBuf) -> Result<SandboxId, SandboxError> {
        let (host_id, backend) = self.pick_any().ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(src).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok(sandbox_id)
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let backend = self.lookup(id)?;
        let result = backend.destroy(id).await;
        // Drop the ownership row regardless — even if destroy errors,
        // the id is no longer routable to anything sensible. A future
        // call against the same id will fail with NotFound, which is
        // the correct semantics.
        self.sandbox_owner.remove(&id);
        result
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        let backend = self.lookup(id)?;
        backend.start_agent(id, agent).await
    }

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        // Fan out to every currently-registered host's backend so
        // the FC sandbox listeners can route inbound vsock harness
        // dials into the same hub. Hosts registered after this call
        // miss it — re-call after registering new hosts in
        // `--mode=all` flows where order can drift.
        for entry in self.hosts.iter() {
            entry.value().backend.set_harness_sink(sink.clone());
        }
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
        let backend = self.lookup(id).ok()?;
        backend.guest_ip(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};

    fn live_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            mounts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn create_then_destroy_routes_through_recorded_host() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
        let reg = HostRegistry::new();
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
        let reg = HostRegistry::new();
        let err = reg.create(live_spec()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no hosts connected"),
            "error must explain why: {msg}"
        );
    }

    #[tokio::test]
    async fn exec_against_unknown_sandbox_id_returns_not_found() {
        let reg = HostRegistry::new();
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

    fn dummy_backend() -> (Arc<dyn SandboxBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path())) as _,
            dir,
        )
    }

    #[test]
    fn pick_for_session_prefers_host_with_target_snapshot() {
        let reg = HostRegistry::new();
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
                warm_pools: Vec::new(),
                local_snapshots: Vec::new(),
                draining: false,
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
                warm_pools: Vec::new(),
                local_snapshots: vec![LocalSnapshotReport {
                    snapshot_id: snap,
                    session_id: engram_core::SessionId::new(),
                    size_bytes: 1,
                    replicated: false,
                    last_accessed_at: chrono::Utc::now(),
                }],
                draining: false,
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: Some(snap),
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(
            picked, h_with_snap,
            "snapshot affinity must override raw capacity"
        );
    }

    #[test]
    fn pick_for_session_falls_through_to_warm_pool_then_capacity() {
        let reg = HostRegistry::new();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_warm = HostId::new();
        let h_big = HostId::new();
        reg.register(h_warm, b1);
        reg.register(h_big, b2);

        reg.update_state(
            h_warm,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 0,
                },
                warm_pools: vec![WarmPoolReport {
                    image_version: "v".into(),
                    ready: 2,
                    target: 4,
                }],
                local_snapshots: Vec::new(),
                draining: false,
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
                warm_pools: Vec::new(),
                local_snapshots: Vec::new(),
                draining: false,
            },
        );

        // Warm-pool hit beats raw capacity.
        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_warm);

        // Different repo, same image: warm pool still applies because
        // the host-side pool keys on image_version only. The h_warm
        // host's warm slot for image "v" services any repo using v.
        let ctx = ScheduleContext {
            repo: "other",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_warm);

        // Different image: warm-pool match doesn't apply, capacity
        // wins (h_big has 16x more free RAM than h_warm).
        let ctx = ScheduleContext {
            repo: "r",
            image_version: "other-image",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_big);
    }

    #[test]
    fn pick_for_session_skips_draining_hosts() {
        let reg = HostRegistry::new();
        let (b1, _d1) = dummy_backend();
        let (b2, _d2) = dummy_backend();
        let h_drain = HostId::new();
        let h_ready = HostId::new();
        reg.register(h_drain, b1);
        reg.register(h_ready, b2);

        // Draining host has the warm pool match and capacity, but
        // shouldn't be picked.
        reg.update_state(
            h_drain,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                warm_pools: vec![WarmPoolReport {
                    image_version: "v".into(),
                    ready: 4,
                    target: 4,
                }],
                local_snapshots: Vec::new(),
                draining: true,
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
                warm_pools: Vec::new(),
                local_snapshots: Vec::new(),
                draining: false,
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_ready);
    }

    #[tokio::test]
    async fn record_sandbox_owner_lets_existing_id_route_post_restart() {
        // Simulates the post-restart flow: the coordinator runs
        // `repopulate_routing` from sessions.sandbox_id/host_id and
        // calls `record_sandbox_owner` to seed the in-memory map.
        // The host then dials back in and `register`s. From that
        // point, exec/snapshot/destroy on the pre-existing sandbox_id
        // route to the right backend without going through `create`.
        let reg = HostRegistry::new();
        let host = HostId::new();
        let sandbox = SandboxId::new();

        // Pre-restart state: the row says sandbox X is on host Y.
        reg.record_sandbox_owner(sandbox, host);

        // Host hasn't dialed back yet — lookup fails because no
        // backend is registered.
        let err = reg
            .destroy(sandbox)
            .await
            .expect_err("no backend registered yet");
        assert!(matches!(err, SandboxError::NotFound));

        // Host re-registers (same host_id). Now routing works.
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
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
        let reg = HostRegistry::new();
        let (b1, _d1) = dummy_backend();
        let h = HostId::new();
        reg.register(h, b1);

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h);
    }
}
