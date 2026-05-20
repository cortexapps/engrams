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

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::{HarnessDial, HostClient};
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
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
}

/// Heartbeat-derived view of a host. Updated by the WS supervisor
/// every ~5s; read by [`HostRegistry::pick_for_session`] when scheduling.
#[derive(Clone, Debug, Default)]
pub struct HostState {
    pub capacity: HostCapacityReport,
    pub local_snapshots: Vec<LocalSnapshotReport>,
    pub draining: bool,
    /// ADR 0014: per-template warm-slot inventory from the host's
    /// most recent heartbeat. Empty for hosts without a warm pool.
    pub warm_slots: Vec<engram_protocol::heartbeat::WarmSlotReport>,
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
    /// In production, `backend` is a `GrpcHostClient` from
    /// `state.services.host_pool` (ADR 0013); in `--mode=all` it's
    /// the in-proc `LocalHostClient`.
    pub fn register(&self, host_id: HostId, backend: Arc<dyn HostClient>) {
        self.hosts.insert(
            host_id,
            HostEntry {
                backend,
                state: RwLock::new(HostState::default()),
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

    /// Session scheduler. Inputs the per-host heartbeat state (capacity,
    /// local snapshots, draining) and ranks:
    ///
    /// 1. Host with `prefer_snapshot_id` in `local_snapshots` — zero-
    ///    cost hot-tier hit.
    /// 2. Host with the largest free capacity (`total - used`), filtered
    ///    against `memory_mib` if provided.
    /// 3. Else any non-draining host.
    /// 4. Else `None` (the coordinator surfaces this as a 503 / typed
    ///    `BackendError::NotSupported("no host available")`).
    ///
    /// The pre-v5 warm-pool tier was deleted with ADR 0008 — chunked-
    /// OCI rootfs + canonical-memory restore made the create-time
    /// savings warm pools amortised no longer worth the complexity.
    pub fn pick_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
    ) -> Option<(HostId, Arc<dyn HostClient>)> {
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

        // Capacity-fit: largest free RAM that meets `memory_mib`.
        let need = ctx.memory_mib.unwrap_or(0) as u64;
        let mut best_cap: Option<(u64, HostId, Arc<dyn HostClient>)> = None;
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

    /// ADR 0014: try the warm-pool lease path before falling through
    /// to cold-create. Walks candidate hosts that report at least
    /// one available slot for `template_ref` (via heartbeat
    /// `warm_slots`), asks each in turn for a lease, then on the
    /// first `Granted` outcome dispatches `LaunchWarmSandbox` with
    /// the per-session agent + policy. Returns `Ok(Some(...))` on a
    /// successful warm activation, `Ok(None)` when no host had a
    /// slot (caller should fall back to cold-create), or `Err(_)`
    /// only for genuinely fatal conditions.
    ///
    /// `Stale{current_ref}` outcomes are skipped silently — the
    /// host's pool is on an older template_ref than coord knows.
    /// Future M1.x: surface this back to the template-cache so the
    /// coord lazily updates. v1 just tries the next host.
    pub async fn try_warm_lease_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        template_ref: engram_core::types::ids::TemplateRef,
        agent: engram_core::types::sandbox::AgentSpec,
        policy: engram_core::types::egress::SessionEgressPolicy,
        // ADR 0014 M1.12 (option D): the session's chosen harness
        // pack URI. The host swaps the warm slot's bake-time stub
        // harness for this URI's ext4 before start_agent dials
        // bootstrap. `None` for sessions without a harness
        // (`kind = none`) — the bake-time stub stays attached.
        harness_pack_uri: Option<String>,
        // The session's canonical harness name (e.g. `"claude"`).
        // Bootstrap exec's `/run/engram/harnesses/<name>/harness`
        // inside the VM, so the host must build the harness ext4
        // with this directory name — not whatever the URI's last
        // path segment happens to be (which would yield
        // `harness-claude` for the `harness-claude` repo URI).
        // `None` iff `harness_pack_uri` is `None`.
        harness_name: Option<String>,
    ) -> Result<Option<(HostId, SandboxId)>, SandboxError> {
        use engram_core::traits::host_client::WarmLeaseOutcome;
        let candidates = self.candidates_with_warm_slot(template_ref);
        if candidates.is_empty() {
            return Ok(None);
        }
        // Sequential rather than parallel — v1 fleets are small
        // (single-digit hosts) and the lease RPC is sub-ms when the
        // pool has capacity. Parallel-ask becomes interesting at
        // fleet sizes where the wait-for-first-yes wins; defer the
        // tokio::select! variant until measured demand.
        let _ = ctx; // ScheduleContext not consumed today; reserved for
                     // memory-fit / repo affinity refinements in M1.9+.
        for (host_id, backend) in candidates {
            let outcome = match backend.lease_warm_sandbox(template_ref).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(
                        %host_id,
                        %template_ref,
                        error = %e,
                        "warm-lease RPC failed; trying next host",
                    );
                    continue;
                }
            };
            match outcome {
                WarmLeaseOutcome::Granted(sandbox_id) => {
                    // Launch on the same host. If it fails after a
                    // successful lease, the leased slot is effectively
                    // orphaned — that's a tracked-but-non-blocking
                    // issue for v1; the warm pool's refill loop will
                    // reach target again on its own. Surface the error
                    // so the caller can cold-fall-back.
                    if let Err(e) = backend
                        .launch_warm_sandbox(
                            sandbox_id,
                            agent.clone(),
                            policy.clone(),
                            harness_pack_uri.clone(),
                            harness_name.clone(),
                        )
                        .await
                    {
                        tracing::warn!(
                            %host_id,
                            %sandbox_id,
                            %template_ref,
                            error = %e,
                            "warm-launch failed after lease; falling back to cold-create",
                        );
                        // Best-effort: try to destroy the orphaned slot
                        // so the autoscaler doesn't double-count it.
                        let _ = backend.destroy(sandbox_id).await;
                        continue;
                    }
                    self.sandbox_owner.insert(sandbox_id, host_id);
                    return Ok(Some((host_id, sandbox_id)));
                }
                WarmLeaseOutcome::Stale { current_ref } => {
                    tracing::debug!(
                        %host_id,
                        requested = %template_ref,
                        host_current = %current_ref,
                        "host reports stale template_ref; trying next",
                    );
                    continue;
                }
                WarmLeaseOutcome::NoCapacity => continue,
            }
        }
        Ok(None)
    }

    /// Hosts whose most recent heartbeat reported at least one
    /// available warm slot for `template_ref`. Ordered by available
    /// count descending so the fullest pool gets asked first.
    fn candidates_with_warm_slot(
        &self,
        template_ref: engram_core::types::ids::TemplateRef,
    ) -> Vec<(HostId, Arc<dyn HostClient>)> {
        let mut scored: Vec<(u32, HostId, Arc<dyn HostClient>)> = Vec::new();
        for entry in self.hosts.iter() {
            let st = entry.value().state.read();
            if st.draining {
                continue;
            }
            let available = st
                .warm_slots
                .iter()
                .find(|s| s.template_ref == template_ref)
                .map(|s| s.available)
                .unwrap_or(0);
            if available == 0 {
                continue;
            }
            scored.push((available, *entry.key(), entry.value().backend.clone()));
        }
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        scored.into_iter().map(|(_, h, b)| (h, b)).collect()
    }

    /// Same as `create_for_session` but for restoring from a snapshot.
    /// Routes to the host carrying `snapshot_id` if any; else falls
    /// through to capacity-based pick. ADR 0007 Phase 6: takes a
    /// `SnapshotMetadata` directly (backends look up their own
    /// per-snapshot staging dir from the chunked manifest refs).
    pub async fn restore_for_session(
        &self,
        ctx: &ScheduleContext<'_>,
        metadata: SnapshotMetadata,
    ) -> Result<(HostId, SandboxId), SandboxError> {
        let (host_id, backend) = self.pick_for_session(ctx).ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(metadata).await?;
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

    fn lookup(&self, sandbox_id: SandboxId) -> Result<Arc<dyn HostClient>, SandboxError> {
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
        let backend = self.lookup(id)?;
        backend.exec_stream(id, cmd).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let backend = self.lookup(id)?;
        backend.snapshot(id).await
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let backend = self.lookup(id)?;
        backend.commit_snapshot(id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let backend = self.lookup(id)?;
        backend.abort_snapshot(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        let (host_id, backend) = self.pick_any().ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(metadata).await?;
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

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let backend = self.lookup(id)?;
        backend.start_agent(id, agent, policy).await
    }

    async fn apply_egress_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let backend = self.lookup(policy.sandbox_id)?;
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
        let backend = self.lookup(id).ok()?;
        backend.guest_ip(id).await
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        if let Ok(backend) = self.lookup(sandbox_id) {
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
        let backend = self.lookup(sandbox_id)?;
        backend.send_prompt(sandbox_id, text).await
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let backend = self.lookup(sandbox_id)?;
        backend.acquire_shell(sandbox_id).await
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let backend = self.lookup(sandbox_id)?;
        backend.release_shell(sandbox_id).await
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};

    fn live_spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
            canonical_memory_manifest: None,
        }
    }

    #[tokio::test]
    async fn create_then_destroy_routes_through_recorded_host() {
        let dir = tempfile::tempdir().unwrap();
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_sandbox_process::ProcessBackend::new(dir.path()));
        let backend: Arc<dyn HostClient> =
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(raw));
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
                    warm_slots: Vec::new(),
                },
                local_snapshots: Vec::new(),
                draining: false,
                warm_slots: Vec::new(),
            },
        );
        reg.update_state(
            h_with_snap,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 1,
                    warm_slots: Vec::new(),
                },
                local_snapshots: vec![LocalSnapshotReport {
                    snapshot_id: snap,
                    session_id: engram_core::SessionId::new(),
                    size_bytes: 1,
                    replicated: false,
                    last_accessed_at: chrono::Utc::now(),
                }],
                draining: false,
                warm_slots: Vec::new(),
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
    fn pick_for_session_chooses_largest_free_capacity() {
        let reg = HostRegistry::new();
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
                    warm_slots: Vec::new(),
                },
                local_snapshots: Vec::new(),
                draining: false,
                warm_slots: Vec::new(),
            },
        );
        reg.update_state(
            h_big,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                    warm_slots: Vec::new(),
                },
                local_snapshots: Vec::new(),
                draining: false,
                warm_slots: Vec::new(),
            },
        );

        let ctx = ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
        };
        let (picked, _) = reg.pick_for_session(&ctx).unwrap();
        assert_eq!(picked, h_big, "larger free capacity wins");
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

        // Draining host has more free capacity, but shouldn't be picked.
        reg.update_state(
            h_drain,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                    warm_slots: Vec::new(),
                },
                local_snapshots: Vec::new(),
                draining: true,
                warm_slots: Vec::new(),
            },
        );
        reg.update_state(
            h_ready,
            HostState {
                capacity: HostCapacityReport {
                    total_mib: 1024,
                    used_mib: 512,
                    running_sandboxes: 0,
                    warm_slots: Vec::new(),
                },
                local_snapshots: Vec::new(),
                draining: false,
                warm_slots: Vec::new(),
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

    // ----- ADR 0014 M1.8 warm-lease scheduler tests -----

    /// Recording HostClient that returns a pre-programmed
    /// WarmLeaseOutcome on `lease_warm_sandbox` and captures every
    /// `launch_warm_sandbox` call. Lets the test prime an outcome
    /// per host + assert exactly which host got the launch.
    #[derive(Clone)]
    struct WarmRecorder {
        outcome: engram_core::traits::host_client::WarmLeaseOutcome,
        launch_log:
            Arc<parking_lot::Mutex<Vec<(SandboxId, engram_core::types::sandbox::AgentSpec)>>>,
    }

    #[async_trait]
    impl HostClient for WarmRecorder {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
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
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn start_agent(
            &self,
            _: SandboxId,
            _: engram_core::types::sandbox::AgentSpec,
            _: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn apply_egress_policy(
            &self,
            _: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn guest_ip(&self, _: SandboxId) -> Option<String> {
            None
        }
        async fn bind_session(&self, _: SessionId, _: SandboxId) {}
        async fn unbind_session(&self, _: SessionId) {}
        async fn send_prompt(&self, _: SandboxId, _: String) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn acquire_shell(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn release_shell(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn lease_warm_sandbox(
            &self,
            _: engram_core::types::ids::TemplateRef,
        ) -> Result<engram_core::traits::host_client::WarmLeaseOutcome, SandboxError> {
            Ok(self.outcome.clone())
        }
        async fn launch_warm_sandbox(
            &self,
            sandbox_id: SandboxId,
            agent: engram_core::types::sandbox::AgentSpec,
            _policy: engram_core::types::egress::SessionEgressPolicy,
            _harness_pack_uri: Option<String>,
            _harness_name: Option<String>,
        ) -> Result<(), SandboxError> {
            self.launch_log.lock().push((sandbox_id, agent));
            Ok(())
        }
    }

    fn warm_state(template_ref: engram_core::types::ids::TemplateRef, available: u32) -> HostState {
        HostState {
            capacity: HostCapacityReport {
                total_mib: 4096,
                used_mib: 0,
                running_sandboxes: 0,
                warm_slots: Vec::new(),
            },
            local_snapshots: Vec::new(),
            draining: false,
            warm_slots: vec![engram_protocol::heartbeat::WarmSlotReport {
                template_ref,
                available,
                target: available.max(1),
            }],
        }
    }

    fn empty_policy(session_id: SessionId) -> engram_core::types::egress::SessionEgressPolicy {
        engram_core::types::egress::SessionEgressPolicy {
            session_id,
            sandbox_id: SandboxId::new(),
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: Default::default(),
            network_allow_host_patterns: Default::default(),
            secrets: Default::default(),
            secret_mode: Default::default(),
        }
    }

    fn ctx() -> ScheduleContext<'static> {
        ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: Some(64),
        }
    }

    fn agent() -> engram_core::types::sandbox::AgentSpec {
        engram_core::types::sandbox::AgentSpec {
            argv: vec!["/bin/echo".into(), "warm".into()],
            env: Default::default(),
        }
    }

    /// Happy path: one host reports available warm slots for the
    /// requested template; scheduler leases, launches, records
    /// ownership.
    #[tokio::test]
    async fn warm_lease_grants_on_host_with_available_slot() {
        let template_ref = engram_core::types::ids::TemplateRef::new();
        let warm_sandbox = SandboxId::new();
        let launch_log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = Arc::new(WarmRecorder {
            outcome: engram_core::traits::host_client::WarmLeaseOutcome::Granted(warm_sandbox),
            launch_log: launch_log.clone(),
        });

        let reg = HostRegistry::new();
        let host_id = HostId::new();
        reg.register(host_id, recorder.clone() as Arc<dyn HostClient>);
        reg.update_state(host_id, warm_state(template_ref, 1));

        let result = reg
            .try_warm_lease_for_session(
                &ctx(),
                template_ref,
                agent(),
                empty_policy(SessionId::new()),
                None,
                None,
            )
            .await
            .expect("warm lease must not error");
        let (got_host, got_sandbox) = result.expect("warm lease must grant");

        assert_eq!(got_host, host_id, "lease must come from the warm-slot host");
        assert_eq!(
            got_sandbox, warm_sandbox,
            "lease must surface the host's sandbox_id"
        );
        assert_eq!(launch_log.lock().len(), 1, "exactly one launch must fire");
        assert_eq!(launch_log.lock()[0].0, warm_sandbox);
        assert_eq!(
            reg.sandbox_owner.get(&warm_sandbox).map(|r| *r.value()),
            Some(host_id),
            "ownership row must point at the host that launched",
        );
    }

    /// Zero-slot host is skipped silently — the candidate filter
    /// drops hosts whose heartbeat reports no inventory for the
    /// requested template.
    #[tokio::test]
    async fn warm_lease_skips_zero_slot_host() {
        let template_ref = engram_core::types::ids::TemplateRef::new();
        let launch_log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        // Recorder programmed to "Granted" — but the candidate
        // filter shouldn't even reach it.
        let recorder = Arc::new(WarmRecorder {
            outcome: engram_core::traits::host_client::WarmLeaseOutcome::Granted(SandboxId::new()),
            launch_log: launch_log.clone(),
        });

        let reg = HostRegistry::new();
        let host_id = HostId::new();
        reg.register(host_id, recorder as Arc<dyn HostClient>);
        // available=0 — host reports it has no warm slot for this template.
        reg.update_state(host_id, warm_state(template_ref, 0));

        let result = reg
            .try_warm_lease_for_session(
                &ctx(),
                template_ref,
                agent(),
                empty_policy(SessionId::new()),
                None,
                None,
            )
            .await
            .expect("warm lease must not error");
        assert!(result.is_none(), "no candidate ⇒ no lease");
        assert!(
            launch_log.lock().is_empty(),
            "zero-slot host must not be asked to launch",
        );
    }

    /// Stale outcome is skipped — host's pool is on an older
    /// template_ref. Scheduler falls through to cold-create when
    /// no other host has a Granted outcome to offer.
    #[tokio::test]
    async fn warm_lease_stale_outcome_falls_through() {
        let template_ref = engram_core::types::ids::TemplateRef::new();
        let stale_other = engram_core::types::ids::TemplateRef::new();
        let launch_log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = Arc::new(WarmRecorder {
            outcome: engram_core::traits::host_client::WarmLeaseOutcome::Stale {
                current_ref: stale_other,
            },
            launch_log: launch_log.clone(),
        });

        let reg = HostRegistry::new();
        let host_id = HostId::new();
        reg.register(host_id, recorder as Arc<dyn HostClient>);
        // Heartbeat says the host has slots — but the lease itself
        // returns Stale (template_ref skew).
        reg.update_state(host_id, warm_state(template_ref, 1));

        let result = reg
            .try_warm_lease_for_session(
                &ctx(),
                template_ref,
                agent(),
                empty_policy(SessionId::new()),
                None,
                None,
            )
            .await
            .expect("warm lease must not error");
        assert!(result.is_none(), "stale-only outcomes ⇒ no lease");
        assert!(
            launch_log.lock().is_empty(),
            "stale outcome must not trigger launch",
        );
    }

    /// Draining host is excluded from the candidate set even if
    /// heartbeat reported warm slots — drains stop fresh work.
    #[tokio::test]
    async fn warm_lease_skips_draining_host() {
        let template_ref = engram_core::types::ids::TemplateRef::new();
        let launch_log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = Arc::new(WarmRecorder {
            outcome: engram_core::traits::host_client::WarmLeaseOutcome::Granted(SandboxId::new()),
            launch_log: launch_log.clone(),
        });

        let reg = HostRegistry::new();
        let host_id = HostId::new();
        reg.register(host_id, recorder as Arc<dyn HostClient>);
        let mut st = warm_state(template_ref, 5);
        st.draining = true;
        reg.update_state(host_id, st);

        let result = reg
            .try_warm_lease_for_session(
                &ctx(),
                template_ref,
                agent(),
                empty_policy(SessionId::new()),
                None,
                None,
            )
            .await
            .expect("warm lease must not error");
        assert!(result.is_none(), "draining host must be skipped");
        assert!(launch_log.lock().is_empty(), "no launch on draining host");
    }
}
