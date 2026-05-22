use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::event::PersistedEvent;
use crate::types::host::{HostCapacity, HostRecord, HostStatus};
use crate::types::ids::{HostId, SandboxId, SessionId};
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

    /// Atomically (a) mark `host_id` as `Dead`, (b) clear `host_id` on
    /// every session pointed at it, (c) transition those sessions to
    /// `Dead`. Returns the affected SessionIds so the caller
    /// can emit per-session `StatusChanged` events. Postgres uses a
    /// single transaction; the Mock takes its sessions mutex once.
    /// Idempotent on a host already marked Dead — returns an empty
    /// vec since no sessions still point at it.
    async fn mark_host_dead_and_reassign_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SessionId>, MetaError>;

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

    /// ADR 0007 chunk-store GC: enumerate every `manifest_id`
    /// referenced by a live snapshot row. The chunk store's
    /// `gc::run` takes this set as its "do not delete" filter.
    /// Empty default returns no manifests — backends without a
    /// real DB (mocks) can opt out by leaving the default.
    ///
    /// Notes for callers:
    /// - Returns DISTINCT ids; versions aren't surfaced because
    ///   `gc::run` preserves every version of every live id.
    /// - Doesn't include manifest_ids that only enabled images
    ///   reference (no snapshot has been taken yet). Operators
    ///   set a generous `retain_for` window to compensate, or
    ///   layer enabled-image manifests on top before calling
    ///   `gc::run`.
    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }

    /// Symmetric to `list_live_disk_manifest_ids` for the memory
    /// side. ADR 0007 Phase 5 introduced `memory_manifest_id` on
    /// the `snapshots` table; the GC sweep needs both axes so
    /// chunks for retired session memories get collected on the
    /// same cadence as disk chunks. Default `Ok(vec![])` keeps
    /// in-memory test impls quiet — the real query lives in
    /// `engram-postgres`.
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
}
