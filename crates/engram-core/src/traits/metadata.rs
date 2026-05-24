use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::event::PersistedEvent;
use crate::types::host::{HostCapacity, HostRecord, HostStatus};
use crate::types::ids::{HostId, SandboxId, SessionId};
use crate::types::manifest::ManifestRef;
use crate::types::registry::{EnabledImage, HarnessPack, RegistryCredential, SessionSecrets};
use crate::types::session::{Session, SessionSpec, SessionState};
use crate::types::snapshot::SnapshotRecord;

/// Authoritative source of truth. Postgres-backed in v1; trait exists so
/// we can support SQLite for embedded deployments later.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    // ---- liveness ----
    //
    // Cheap connectivity check for readiness probes. Default is
    // `Ok(())` so in-memory test stores don't need to override.
    // Postgres-backed impls should issue a `SELECT 1` against the
    // pool so a coordinator with a broken DB connection fails its
    // `/readyz` probe instead of receiving traffic that 503s.
    async fn ping(&self) -> Result<(), MetaError> {
        Ok(())
    }

    // ---- sessions ----
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError>;

    /// Atomically insert a session in `Created` status with `host_id`
    /// and `sandbox_id` already bound. Used by the create-session API
    /// to only persist a row once scheduling has succeeded — so a
    /// transient capacity blip or unrecoverable scheduling error
    /// doesn't leave a `Pending` row that nothing will ever advance.
    ///
    /// ADR 0015 M2: the row enters life at `Created` (sandbox bound,
    /// nothing else proven) rather than `Active` (the prior shape).
    /// The create handler transitions to `Active` only after
    /// `start_agent` succeeds, so `Active` actually implies "agentd
    /// reachable + harness running" — no more silent dishonest-Active
    /// rows where start_agent later failed but the column already
    /// said Active.
    ///
    /// The caller mints the `SessionId` ahead of scheduling (because
    /// vm_spec env / harness arg construction needs it before the
    /// sandbox exists). The impl persists with that exact id.
    ///
    /// TODO(self-healing reconciler): a future version of this API
    /// reintroduces a "queue-and-retry" path — pending sessions
    /// persist with the full vm_spec captured, and a background
    /// reconciler retries scheduling against later-arriving capacity.
    /// At that point this method might fall back to "insert Pending
    /// if scheduling fails, the reconciler picks it up later". The
    /// reason we don't do that today: SessionSpec doesn't carry
    /// vm_spec / harness resolution context, and capturing it
    /// requires schema work that's bigger than the v1 fix.
    async fn create_session_created(
        &self,
        session_id: SessionId,
        spec: SessionSpec,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError>;

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError>;
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError>;

    /// ADR 0009 reconcile pass: enumerate the `(session_id,
    /// sandbox_id)` pairs for every `status='active'` session
    /// assigned to `host_id` whose `sandbox_id` is populated. The
    /// reconcile pass intersects this against the host's
    /// heartbeat-reported `running_sandboxes`. Missing-from-host
    /// → strike counter increments; N strikes → flip per ADR §3.
    ///
    /// Default impl scans `list_active_sessions()` and filters in
    /// memory — fine for in-memory test mocks. Postgres overrides
    /// with an indexed `WHERE (host_id, status)` query so the
    /// per-heartbeat cost stays O(sandboxes-on-host), not
    /// O(total-active-sessions).
    async fn list_active_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId)>, MetaError> {
        let all = self.list_active_sessions().await?;
        Ok(all
            .into_iter()
            .filter_map(|s| match (s.status, s.host_id, s.sandbox_id) {
                (SessionState::Active, Some(h), Some(sb)) if h == host_id => Some((s.id, sb)),
                _ => None,
            })
            .collect())
    }
    /// ADR 0015 M2: the single validated entry point for `UPDATE
    /// sessions SET status = ...`. Reads the current state, runs
    /// [`SessionState::try_transition_to`] against `target`, and
    /// writes the UPDATE atomically. Returns the previous state on
    /// success — callers use it as the `from` of the `StatusChanged`
    /// event they emit.
    ///
    /// Errors:
    /// - [`MetaError::NotFound`] — no row with this `id`.
    /// - [`MetaError::Conflict`] — the transition is not in the
    ///   legality table. The message is the rendered
    ///   [`crate::types::session::IllegalTransition`] so logs and HTTP
    ///   bodies show both sides.
    ///
    /// Impls must do the SELECT and UPDATE under a single row-level
    /// lock to prevent two concurrent callers racing on the same
    /// (from, to) — without that, both could validate against the
    /// same pre-state and the second's UPDATE silently breaks the
    /// invariant.
    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError>;
    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError>;

    /// Persist the in-memory `SandboxId` of the live sandbox serving
    /// this session. Set to `Some` after `host_registry.create_for_session`
    /// returns, cleared to `None` on evict/migrate. The coordinator
    /// uses these rows to rebuild its in-memory routing maps after
    /// a restart.
    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<(), MetaError>;

    /// ADR 0015 M3: PG-authoritative lookup for "which host owns this
    /// sandbox right now, and what state is its session in?"
    /// `HostRegistry` calls this on cache miss (or after the per-host
    /// TTL elapses) to repair stale `sandbox_owner` entries.
    ///
    /// Returns `(host_id, session_status)` for the session whose
    /// `sandbox_id` column matches, or `None` when no row points at
    /// this sandbox (already orphaned, never existed, or migrated
    /// away). The `host_id` reflects the row's *current* binding —
    /// callers should compare it against their in-memory cache and
    /// repair if drifted. `session_status` lets `HostRegistry` decide
    /// between `SandboxError::HostLost` (host known dead, 410) and
    /// `SandboxError::NotFound` (sandbox unknown, 404) without a
    /// second round-trip.
    ///
    /// Default scans `list_active_sessions` and filters in memory —
    /// fine for in-memory test mocks. Postgres overrides with a
    /// single-row indexed query.
    async fn host_for_sandbox(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<Option<(HostId, SessionState)>, MetaError> {
        let all = self.list_active_sessions().await?;
        Ok(all
            .into_iter()
            .find_map(|s| match (s.host_id, s.sandbox_id) {
                (Some(h), Some(sb)) if sb == sandbox_id => Some((h, s.status)),
                _ => None,
            }))
    }

    // ---- hosts ----
    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError>;
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError>;
    async fn set_host_status(&self, id: HostId, status: HostStatus) -> Result<(), MetaError>;

    /// Record a heartbeat from `host_id`: bump `last_heartbeat_at` to
    /// NOW(), set `status`, and persist `capacity` so cross-pod
    /// `/api/hosts` reads stay consistent (the in-memory
    /// `host_registry` only knows about hosts whose WS connects to
    /// *this* coord pod, so the API view has to fall back to
    /// Postgres for any host owned by a sibling). Distinct from
    /// `set_host_status` because drain/dead transitions imply
    /// nothing about liveness and must not refresh the dead-host
    /// detector's timestamp. The WS dialer's heartbeat handler is
    /// the only caller; in `--mode=all` the in-process timer calls
    /// `upsert_host` instead.
    async fn touch_host_heartbeat(
        &self,
        id: HostId,
        status: HostStatus,
        capacity: HostCapacity,
    ) -> Result<(), MetaError>;

    /// List hosts whose `last_heartbeat_at` is older than `threshold_secs`
    /// AND whose status is `Ready` or `Draining`. The dead-host detector
    /// polls this every ~10s and races other coordinator replicas via
    /// `pg_try_advisory_lock` for the right to evacuate each candidate.
    /// `Dead` rows are filtered out so a still-running coordinator
    /// replica's detector doesn't keep trying to re-kill them.
    async fn list_stale_hosts(&self, threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError>;

    /// Atomically (a) mark `host_id` as `Dead`, (b) clear `host_id`
    /// and `sandbox_id` on every non-terminal session pointed at it,
    /// (c) transition those sessions to `HostLost`.
    ///
    /// ADR 0015 M2: the orphaned sessions move to `HostLost` rather
    /// than straight to `Dead`. The caller then runs a per-session
    /// snapshot check and drives the second-stage transition:
    /// `HostLost -> Idle` if a recoverable snapshot exists,
    /// `HostLost -> Dead` otherwise. Splitting the two stages makes
    /// "host went away" a distinct lifecycle moment from "the
    /// session is unrecoverable," which is what M4 (session
    /// migration) needs as an entry point.
    ///
    /// Returns `(SessionId, previous_state)` pairs so the caller can
    /// emit honest `StatusChanged { from: previous_state, to:
    /// HostLost }` events instead of hand-encoding a placeholder
    /// `from`. Postgres uses a single transaction; the Mock takes
    /// its sessions mutex once. Idempotent on a host already marked
    /// Dead — returns an empty vec since no sessions still point at
    /// it.
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError>;

    // ---- snapshots ----
    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError>;
    async fn list_snapshots_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError>;
    async fn latest_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError>;
    /// ADR 0014 M1.11: fetch a single snapshot row by id. Used by
    /// the heartbeat-ack template enrichment path to surface the
    /// snapshot's persisted `disk_manifest` + `memory_manifest`
    /// to host-agents — without those, warm-pool refill on a
    /// fresh host has no way to materialize the rootfs file FC
    /// `load_snapshot` needs.
    ///
    /// Default returns `None` so backends that don't have a real
    /// DB (mocks, tests) opt out cleanly; callers that depend on
    /// the persisted shape (heartbeat enrichment) override.
    async fn get_snapshot(
        &self,
        _id: crate::types::SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(None)
    }

    /// Enumerate every disk `manifest_id` referenced by a live
    /// snapshot row. Today's sole caller is `reap_materialize_dir`,
    /// which uses the result as the "do not delete" filter when
    /// pruning assembled `<manifest_id>-vN.ext4` files on hosts.
    /// (The chunk-store GC that previously consumed this set was
    /// removed 2026-05-23 — see ADR 0015 M5.)
    ///
    /// Returns DISTINCT ids; versions aren't surfaced. Default
    /// `Ok(vec![])` keeps in-memory test impls quiet — the real
    /// query lives in `engram-postgres`.
    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }

    /// Symmetric memory-side companion to
    /// `list_live_disk_manifest_ids`. Currently unused by any
    /// in-tree caller after the chunk-store GC was removed, but
    /// kept on the trait so a future reintroduction has a
    /// pre-shaped seam.
    async fn list_live_memory_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }

    // ---- session event log ----

    /// Append an event to a session's persistent log. Returns the
    /// monotonic per-session `idx` assigned to this event. Allocation
    /// is atomic — concurrent appends to the same session never
    /// collide and never leave gaps. `kind` is the event discriminant
    /// (e.g. "stdout", "snapshot_taken"); `payload` is the wire JSON.
    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError>;

    /// Return events with `idx > since`, in idx order, capped at
    /// `limit`. Used by `GET /sessions/:id/events?since=N` (and by
    /// EventSource auto-reconnect via `Last-Event-ID`) to replay the
    /// log before tailing the live bus. Pass `since = -1` to start
    /// from the very first event.
    async fn list_session_events_since(
        &self,
        session_id: SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError>;

    // ---- registry credentials (Phase 5) ----

    /// Insert or replace a registry credential row. The
    /// `wrapped_dek` / `nonce` / `ciphertext` come from
    /// `engram-crypto::CredCipher::seal`. `(registry_host, username)`
    /// is the natural key — re-adding the same pair updates the
    /// cipher fields in place.
    async fn upsert_registry_credential(&self, cred: RegistryCredential) -> Result<(), MetaError>;

    async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError>;

    /// Look up the credential row for a given registry host. Returns
    /// `None` when no row exists — callers fall back to anonymous
    /// access (works for public registries and `localhost:5001`).
    /// When more than one row exists for the same host, returns the
    /// most-recently-updated row.
    async fn registry_credential_for_host(
        &self,
        registry_host: &str,
    ) -> Result<Option<RegistryCredential>, MetaError>;

    async fn delete_registry_credential(&self, registry_host: &str) -> Result<(), MetaError>;

    // ---- harness packs (Phase 5) ----

    async fn upsert_harness_pack(&self, pack: HarnessPack) -> Result<(), MetaError>;
    async fn list_harness_packs(&self) -> Result<Vec<HarnessPack>, MetaError>;
    async fn get_harness_pack(&self, name: &str) -> Result<Option<HarnessPack>, MetaError>;
    async fn delete_harness_pack(&self, name: &str) -> Result<(), MetaError>;

    // ---- enabled images (Phase 5b) ----
    //
    // Curated allowlist of image URIs that sessions may reference.
    // Manifest is fetched at enable time and persisted on the row,
    // so session-create has zero network dependency on the manifest
    // path. The host-agent still pulls the rootfs blob on first use,
    // but that's lazy + cached separately by digest.

    async fn upsert_enabled_image(&self, image: EnabledImage) -> Result<(), MetaError>;
    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError>;
    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError>;
    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError>;

    // ---- session secrets ----
    //
    // Per-request `secrets` overrides supplied at session-create,
    // sealed under the deployment KEK. The resume path opens the
    // sealed blob to rebuild the post-resume harness's launch env;
    // without persistence the in-VM bootstrap respawns a Claude
    // child with no OAuth token and the user gets re-prompted to
    // log in.
    async fn upsert_session_secrets(&self, secrets: SessionSecrets) -> Result<(), MetaError>;
    async fn get_session_secrets(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSecrets>, MetaError>;
    async fn delete_session_secrets(&self, session_id: SessionId) -> Result<(), MetaError>;

    // ----------------------------------------------------------------
    // ADR 0016 §A.1.5c — cross-replica idle-eviction guard.
    // Backed by the `eviction_inflight` table. The contract:
    //   - `try_acquire_eviction_lease` is atomic INSERT ... ON
    //     CONFLICT DO NOTHING. Returns Ok(true) if the row was
    //     inserted (caller owns the pipeline), Ok(false) if a
    //     row already exists (another caller is mid-pipeline).
    //   - `release_eviction_lease` is idempotent — extra calls
    //     against an already-deleted row are Ok(()). Used by the
    //     RAII guard's drop path.
    //   - `sweep_stale_eviction_leases` deletes rows older than
    //     `max_age` and returns them for warn-logging. Backs the
    //     coord-side stale-lease reaper.
    // ----------------------------------------------------------------

    /// Returns `Ok(true)` if the lease was acquired (row inserted),
    /// `Ok(false)` if a concurrent caller already holds it.
    ///
    /// Default impl unconditionally returns `Ok(true)` — the
    /// benign behaviour for test mocks / in-memory backends where
    /// concurrent coord-pod racing isn't a concern. `PostgresStore`
    /// overrides with the real INSERT ... ON CONFLICT DO NOTHING.
    async fn try_acquire_eviction_lease(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _locked_by: &str,
    ) -> Result<bool, MetaError> {
        Ok(true)
    }

    /// Idempotent. Drop-safe.
    async fn release_eviction_lease(&self, _session_id: SessionId) -> Result<(), MetaError> {
        Ok(())
    }

    /// Stale-lease reaper. Deletes rows where `locked_at < now() -
    /// max_age` and returns them so the caller can warn-log
    /// `(session_id, locked_by, locked_at)` per reaped row.
    async fn sweep_stale_eviction_leases(
        &self,
        _max_age: std::time::Duration,
    ) -> Result<Vec<StaleEvictionLease>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase B: write the host's freshly-flushed disk
    /// manifest into `sessions.live_disk_manifest_*`, gated by a
    /// `sandbox_id` match so a stale publish from a destroyed
    /// sandbox can't clobber a fresh binding. Bumps
    /// `chunk_generation` in the same transaction on `Applied` so
    /// Phase C's GC barrier sees an atomic step from "old pin set
    /// → write → new pin set" (no mid-sweep window where the new
    /// manifest exists but the generation hasn't ticked).
    ///
    /// `DroppedStale` means the UPDATE matched zero rows — either
    /// the session was destroyed between flush and publish, or the
    /// sandbox was rebound. Coord-side handler logs a `warn!` with
    /// the mismatch and returns 200 to the host (the host shouldn't
    /// retry; the staleness is structural).
    ///
    /// Default returns `Applied` and does nothing, so test mocks
    /// don't need to plumb the columns until they exercise Phase B.
    async fn update_live_disk_manifest(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _manifest_ref: ManifestRef,
    ) -> Result<UpdateOutcome, MetaError> {
        Ok(UpdateOutcome::Applied)
    }

    /// ADR 0016 Phase C support: read the one-row `chunk_generation`
    /// counter. Phase C's GC sweep reads this before listing the
    /// pin set and again at the end; if it ticked, the sweep
    /// restarts with a fresh pin set. Default `0` so non-PG
    /// backends don't have to track it.
    async fn chunk_generation(&self) -> Result<u64, MetaError> {
        Ok(0)
    }
}

/// Outcome of [`MetadataStore::update_live_disk_manifest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The session row was updated and `chunk_generation` ticked.
    /// Coord handler logs `info!`; host's coalescing mpsc moves on.
    Applied,
    /// The UPDATE matched zero rows — `sessions.sandbox_id !=
    /// publish.sandbox_id` (destroyed/rebound) or the session is
    /// gone. Coord handler logs `warn!` with the mismatch fields;
    /// host shouldn't retry.
    DroppedStale,
}

/// One row from [`MetadataStore::sweep_stale_eviction_leases`].
/// Carried so the coord-side sweeper can warn-log who held the
/// lease for how long before it was reaped.
#[derive(Clone, Debug)]
pub struct StaleEvictionLease {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub locked_by: String,
    pub locked_at: chrono::DateTime<chrono::Utc>,
}
