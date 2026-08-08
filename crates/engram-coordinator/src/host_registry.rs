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
use engram_core::traits::{HarnessDial, HostClient, MetadataStore, SessionFence};
use engram_core::types::sandbox::{
    AgentSpec, ExecRequest, ExecStream, SandboxSpec, SessionFileMetadata, SessionFileSpec,
    SessionFileStream, WriteFileResult, WriteFileSpec,
};
use engram_core::types::session::SessionState;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SessionId};

/// Per-host record kept in-memory. ADR 0047 demoted this to a pure
/// CONNECTION record: the heartbeat-derived scheduling state that used
/// to live here moved into the `hosts` row (every coordinator replica
/// schedules from PG via [`crate::placement`]); what remains is the
/// dialable backend and a freshness stamp for the routing cache.
struct HostEntry {
    backend: Arc<dyn HostClient>,
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
///    sites (`reconcile`) call
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
    /// ADR 0047: on-demand dialer for hosts this replica hasn't
    /// registered yet (their heartbeats land on a sibling pod).
    /// `backend_for` falls back to the PG row's `host_addr`, warms the
    /// pool, and registers the entry — the same self-heal the
    /// heartbeat handler does, made read-through. `None` in
    /// `--mode=all` / tests (single pod, backends registered directly).
    /// Behind a lock because the registry is constructed before the
    /// pool and shared as `Arc` — `set_dialer` wires it post-hoc.
    dialer: parking_lot::RwLock<Option<Arc<engram_protocol::grpc_pool::GrpcHostPool>>>,
    /// Soft-invalidation horizon for `last_observed_heartbeat`. Entries
    /// older than this fall through to a PG read on the next
    /// `resolve_owner`. Configurable via `ENGRAM_HOST_REGISTRY_TTL_SECS`;
    /// default 60s — well over the 5s heartbeat cadence but under
    /// the dead-host detector's 30s threshold, so the TTL only fires
    /// when *detection itself* is broken or paused.
    ttl: Duration,
    /// ADR 0098 D1: wall clock for heartbeat-freshness decisions.
    /// Defaults to `SystemClock` at construction; the simulation
    /// harness swaps it when it builds the registry.
    clock: Arc<dyn engram_core::traits::Clock>,
}

impl HostRegistry {
    /// Construct a registry backed by `meta` for read-through cache
    /// misses. TTL is read from `ENGRAM_HOST_REGISTRY_TTL_SECS`
    /// (default 60s).
    pub fn new(meta: Arc<dyn MetadataStore>) -> Self {
        Self {
            hosts: DashMap::new(),
            sandbox_owner: DashMap::new(),
            meta,
            dialer: parking_lot::RwLock::new(None),
            ttl: crate::placement::placement_ttl(),
            clock: Arc::new(engram_core::traits::SystemClock::new()),
        }
    }

    /// ADR 0047: wire the gRPC pool so this replica can dial hosts it
    /// has never fielded a heartbeat from (multi-replica routing).
    pub fn set_dialer(&self, pool: Arc<engram_protocol::grpc_pool::GrpcHostPool>) {
        *self.dialer.write() = Some(pool);
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
            dialer: parking_lot::RwLock::new(None),
            ttl,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
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

    /// Bump a host's freshness stamp. Called by the heartbeat handler
    /// on each inbound heartbeat so the M3 TTL path sees the host as
    /// fresh on the next `resolve_owner`. (ADR 0047: the heartbeat's
    /// scheduling payload goes to PG via `touch_host_heartbeat`; this
    /// is the only in-memory trace it leaves.)
    pub fn touch_seen(&self, host_id: HostId) {
        if let Some(entry) = self.hosts.get(&host_id) {
            entry
                .value()
                .last_observed_heartbeat
                .store(now_millis(), Ordering::Relaxed);
        }
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
    /// (`reconcile::flip_missing`) once the DB
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

    /// First registered backend — for ANONYMOUS flows only (the
    /// `HostClient` trait's `create`/`restore`, used by dev/test paths
    /// without session context). Session-aware scheduling lives in
    /// [`crate::placement`].
    fn pick_any(&self) -> Option<(HostId, Arc<dyn HostClient>)> {
        self.hosts
            .iter()
            .next()
            .map(|e| (*e.key(), e.value().backend.clone()))
    }

    /// ADR 0047: resolve a (placement-picked) host to its dialable
    /// backend. Fast path: registered on this replica. Read-through:
    /// the host's heartbeats land on a sibling pod — fetch its
    /// `host_addr` from the (fresh) PG row, warm the gRPC pool, and
    /// register the entry so subsequent calls take the fast path.
    pub async fn backend_for(&self, host_id: HostId) -> Result<Arc<dyn HostClient>, SandboxError> {
        if let Some(entry) = self.hosts.get(&host_id) {
            return Ok(entry.value().backend.clone());
        }
        self.dial_through(host_id).await
    }

    /// The slow half of [`Self::backend_for`]: PG row → freshness check
    /// → warm + register. `HostLost` when the row is missing, stale, or
    /// has no dial address.
    async fn dial_through(&self, host_id: HostId) -> Result<Arc<dyn HostClient>, SandboxError> {
        let pool = self.dialer.read().clone();
        let Some(pool) = pool else {
            // No dialer (mode=all / tests): an unregistered host is
            // simply unreachable from this process.
            return Err(SandboxError::HostLost);
        };
        let rows = self.meta.list_active_hosts().await.map_err(|e| {
            SandboxError::Vm(format!("list_active_hosts during dial-through: {e}").into())
        })?;
        let Some(row) = rows.into_iter().find(|r| r.id == host_id) else {
            return Err(SandboxError::HostLost);
        };
        let fresh = self
            .clock
            .now_utc()
            .signed_duration_since(row.last_heartbeat_at)
            .to_std()
            .map_or(true, |age| age <= self.ttl);
        if !fresh {
            return Err(SandboxError::HostLost);
        }
        let Some(addr) = row.host_addr else {
            return Err(SandboxError::HostLost);
        };
        pool.warm(host_id, addr).await?;
        let client = pool.get(host_id)?;
        let backend: Arc<dyn HostClient> = Arc::new(client);
        self.register(host_id, backend.clone());
        Ok(backend)
    }

    /// ADR 0046: restore the base snapshot onto an ALREADY-CHOSEN host (picked
    /// and reserved by the PG `reserve_placement` transaction) rather than
    /// picking here. Mirrors `restore_base_for_session` minus the pick — the
    /// capacity decision already happened durably in Postgres.
    pub async fn restore_base_on_host(
        &self,
        host_id: HostId,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
        fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        // The capacity decision already happened in `reserve_placement`; just
        // resolve the chosen host's backend (dialing through PG if this
        // replica hasn't seen the host yet — ADR 0047).
        let backend = self.backend_for(host_id).await?;
        let sandbox_id = backend
            .restore_base_for_session(metadata, session_env, selected_mounts, fence)
            .await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok(sandbox_id)
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
        // Active-class session in PG. Repair the cache iff the host is
        // reachable: registered-and-fresh on this replica, or (ADR
        // 0047) dialable through its fresh PG row — the host's
        // heartbeats may be landing on a sibling pod, which must not
        // read as HostLost here.
        let backend = match self.hosts.get(&host_id) {
            Some(entry) if self.entry_is_fresh(&entry) => entry.value().backend.clone(),
            Some(entry) => {
                // Registered but past the local TTL. PG decides: a
                // fresh row means the host is alive (its heartbeats
                // land on a sibling pod) — refresh our stamp and
                // serve; a stale row means genuinely silent → HostLost.
                drop(entry);
                let backend = self.pg_fresh_backend(host_id).await?;
                self.touch_seen(host_id);
                backend
            }
            None => self.dial_through(host_id).await?,
        };
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok((host_id, backend))
    }

    /// Confirm via the PG row that `host_id` is heartbeat-fresh, then
    /// return its (already-registered) backend. `HostLost` when the
    /// row is missing or stale.
    async fn pg_fresh_backend(&self, host_id: HostId) -> Result<Arc<dyn HostClient>, SandboxError> {
        let rows = self.meta.list_active_hosts().await.map_err(|e| {
            SandboxError::Vm(format!("list_active_hosts during freshness check: {e}").into())
        })?;
        let Some(row) = rows.into_iter().find(|r| r.id == host_id) else {
            return Err(SandboxError::HostLost);
        };
        let fresh = self
            .clock
            .now_utc()
            .signed_duration_since(row.last_heartbeat_at)
            .to_std()
            .map_or(true, |age| age <= self.ttl);
        if !fresh {
            return Err(SandboxError::HostLost);
        }
        self.hosts
            .get(&host_id)
            .map(|e| e.value().backend.clone())
            .ok_or(SandboxError::HostLost)
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
        // ADR 0050 C: the gRPC pool dials lazily and defers per-RPC
        // retry to the call site. A freshly-scaled host's first call
        // can come back `Unavailable` (connect not yet established);
        // retry a bounded number of times, re-resolving the owner each
        // attempt (the binding may have moved), before surfacing the
        // transient error as a retryable 503. Only the stream-ESTABLISH
        // call is retried here; a mid-stream drop ends without Exit so the
        // durable exec caller can re-attach with the same ticket + offsets.
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (_, backend) = self.resolve_owner(id).await?;
            match backend.exec_stream(id, cmd.clone()).await {
                Err(SandboxError::Unavailable(msg)) if attempt < MAX_ATTEMPTS => {
                    tracing::debug!(
                        sandbox_id = %id, attempt, error = %msg,
                        "exec_stream transient Unavailable; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
                other => return other,
            }
        }
    }

    async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.cancel_exec(id, exec_id).await
    }

    async fn write_files(
        &self,
        id: SandboxId,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        // Unlike exec, WriteFiles is idempotent: retrying the whole batch after
        // an Unavailable response merely replaces each file with the same bytes.
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (_, backend) = self.resolve_owner(id).await?;
            match backend.write_files(id, files.clone()).await {
                Err(SandboxError::Unavailable(msg)) if attempt < MAX_ATTEMPTS => {
                    tracing::debug!(
                        sandbox_id = %id, attempt, error = %msg,
                        "write_files transient Unavailable; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
                other => return other,
            }
        }
    }

    async fn upload_file(
        &self,
        id: SandboxId,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<SessionFileMetadata, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.upload_file(id, spec, bytes).await
    }

    async fn read_file(
        &self,
        id: SandboxId,
        path: String,
    ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.read_file(id, path).await
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.snapshot(id, fence).await
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.snapshot_begin(id, fence).await
    }

    async fn snapshot_wait(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.snapshot_wait(id, fence).await
    }

    async fn migration_presetup(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.migration_presetup(id, fence).await
    }

    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
        fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend
            .migration_capture_postcopy(id, export_id, fence)
            .await
    }

    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.migration_drain_wait(id).await
    }

    async fn migration_capture(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationCaptureOut, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.migration_capture(id, fence).await
    }

    async fn migration_commit(
        &self,
        id: SandboxId,
        export_id: &str,
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.migration_commit(id, export_id, fence).await
    }

    async fn migration_abort(
        &self,
        id: SandboxId,
        export_id: &str,
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.migration_abort(id, export_id, fence).await
    }

    async fn commit_snapshot(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.commit_snapshot(id, fence).await
    }

    async fn abort_snapshot(&self, id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.abort_snapshot(id, fence).await
    }

    async fn restore(
        &self,
        metadata: SnapshotMetadata,
        fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        let (host_id, backend) = self.pick_any().ok_or_else(Self::no_host_error)?;
        let sandbox_id = backend.restore(metadata, fence).await?;
        self.sandbox_owner.insert(sandbox_id, host_id);
        Ok(sandbox_id)
    }

    async fn destroy(&self, id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        // ADR 0050 E: retry transient `Unavailable` so a gRPC blip during
        // teardown doesn't leak the FC on the happy path (the host-local
        // teardown reconcile is the floor, but retrying here destroys it
        // immediately when the host is reachable). Re-resolve the owner
        // each attempt; a `HostLost` resolve means the host — and its FC —
        // are already gone, so there's nothing to retry.
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            let backend = match self.resolve_owner(id).await {
                Ok((_, b)) => b,
                Err(e) => break Err(e),
            };
            match backend.destroy(id, fence).await {
                Err(SandboxError::Unavailable(msg)) if attempt < MAX_ATTEMPTS => {
                    tracing::debug!(
                        sandbox_id = %id, attempt, error = %msg,
                        "destroy transient Unavailable; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
                other => break other,
            }
        };
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
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.start_agent(id, agent, policy, fence).await
    }

    /// ADR 0068: same single-resolve delegation as `start_agent` — this
    /// mode=all convenience impl has exactly one connected host in
    /// practice, so no retry logic is warranted for a read-only probe.
    async fn probe_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        let (_, backend) = self.resolve_owner(id).await?;
        backend.probe_sandbox(id).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        // Aggregate across all connected hosts. Errors from any one
        // host are surfaced; partial results aren't reported in 3a.
        //
        // Snapshot the backends FIRST (cheap Arc clones), dropping every
        // `hosts` shard guard, THEN await per host. Awaiting while a
        // DashMap `iter()` guard is live holds that shard's RwLock across
        // the suspension; a concurrent `register`/`unregister` that hashes
        // to the same shard then blocks on it — a permanent deadlock on a
        // SINGLE-THREADED executor (the DST sim), where the suspended
        // iterator can never be polled to release the guard. (Shard
        // assignment is `RandomState`-seeded and `available_parallelism`-
        // sized, so the collision was a ~1% getrandom-/host-count-
        // dependent hang — determinism-audit item 8.) Same rule
        // `backend_of` documents: clone the Arc, never hold the entry
        // across an `.await`.
        let backends: Vec<Arc<dyn HostClient>> = self
            .hosts
            .iter()
            .map(|e| e.value().backend.clone())
            .collect();
        let mut all = Vec::new();
        for backend in backends {
            all.extend(backend.list().await?);
        }
        Ok(all)
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<std::net::Ipv4Addr> {
        let (_, backend) = self.resolve_owner(id).await.ok()?;
        backend.guest_ip(id).await
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId, binding_epoch: u64) {
        if let Ok((_, backend)) = self.resolve_owner(sandbox_id).await {
            backend
                .bind_session(session_id, sandbox_id, binding_epoch)
                .await;
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        // No sandbox routing for the unbind — fan out to every
        // connected host so whichever one had the binding clears it.
        // Each host's `unbind_session` is a no-op for unknown session
        // ids, so the broadcast is cheap.
        //
        // Snapshot backends FIRST so no `hosts` shard guard is held across
        // the per-host `.await` (see `list` — the DashMap-guard-across-
        // await deadlock on a single-threaded executor).
        let backends: Vec<Arc<dyn HostClient>> = self
            .hosts
            .iter()
            .map(|e| e.value().backend.clone())
            .collect();
        for backend in backends {
            backend.unbind_session(session_id).await;
        }
    }

    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
        mode: Option<String>,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.send_prompt(sandbox_id, prompt_id, text, mode).await
    }

    async fn edit_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend
            .edit_queued_prompt(sandbox_id, prompt_id, text)
            .await
    }

    async fn dequeue_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.dequeue_queued_prompt(sandbox_id, prompt_id).await
    }

    async fn tool_result(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        result_json: String,
    ) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend
            .tool_result(sandbox_id, tool_call_id, result_json)
            .await
    }

    async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.interrupt(sandbox_id).await
    }

    async fn pause(&self, sandbox_id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.pause(sandbox_id, fence).await
    }

    async fn resume(&self, sandbox_id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.resume(sandbox_id, fence).await
    }

    async fn start_browser(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.start_browser(sandbox_id).await
    }

    async fn stop_browser(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.stop_browser(sandbox_id).await
    }

    async fn start_ide(&self, sandbox_id: SandboxId) -> Result<u16, SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.start_ide(sandbox_id).await
    }

    async fn stop_ide(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.stop_ide(sandbox_id).await
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.proxy_shell(sandbox_id).await
    }

    async fn proxy_port(
        &self,
        sandbox_id: SandboxId,
        port: u16,
    ) -> Result<engram_core::types::port::PortTunnel, SandboxError> {
        let (_, backend) = self.resolve_owner(sandbox_id).await?;
        backend.proxy_port(sandbox_id, port).await
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
        async fn transition_session_created(
            &self,
            _: engram_core::SessionId,
            _: SandboxId,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn reserve_and_persist_create(
            &self,
            _: engram_core::traits::SessionCreateWriteSet,
            _: &[HostId],
            _: usize,
        ) -> Result<engram_core::traits::CreateDisposition, engram_core::MetaError> {
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
            _: engram_core::types::BindingDisposition,
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
            _: engram_core::types::host::HostHeartbeat,
        ) -> Result<(), engram_core::MetaError> {
            unreachable!()
        }
        async fn set_host_cordoned(
            &self,
            _: HostId,
            _: bool,
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
        ) -> Result<bool, engram_core::MetaError> {
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
            swap_mib: None,
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

        reg.destroy(sandbox_id, SessionFence::unfenced())
            .await
            .unwrap();
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
                    exec_id: None,
                    stdout_offset: None,
                    stderr_offset: None,
                    wake: None,
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
            .destroy(sandbox, SessionFence::unfenced())
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
        reg.destroy(sandbox, SessionFence::unfenced())
            .await
            .expect("routing reaches backend");
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
