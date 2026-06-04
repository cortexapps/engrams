use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::event::{ArtifactRow, PersistedEvent};
use crate::types::host::{HostCapacity, HostRecord, HostStatus};
use crate::types::ids::{HostId, SandboxId, SessionId};
use crate::types::manifest::ManifestRef;
use crate::types::registry::{
    EnableJob, EnableJobState, EnabledImage, RegistryCredential, SessionSecrets,
};
use crate::types::session::{Session, SessionSpec, SessionState};
use crate::types::snapshot::SnapshotRecord;

/// ADR 0021 P1.8: outcome of [`MetadataStore::soft_delete_enabled_image`].
/// `Disabled` is the happy path; `AlreadyDisabled` keeps the disable
/// endpoint idempotent for retries; `Blocked` carries the offending
/// sessions so the operator can decide whether to force-stop them or
/// wait.
#[derive(Clone, Debug)]
pub enum DisableEnabledImageOutcome {
    /// Row was live; `soft_deleted_at` flipped to `NOW()` in this call.
    Disabled,
    /// Row was already soft-deleted before this call; no change made.
    /// Returned instead of an error so the disable endpoint can be
    /// safely retried (operator hit it twice, two coord pods raced).
    AlreadyDisabled,
    /// One or more sessions in `{pending, created, active, evacuating}`
    /// reference the image. `Vec` is bounded to the first 16 sessions
    /// (`ORDER BY created_at ASC LIMIT 16`) — enough for an operator to
    /// triage without unbounded response size.
    Blocked(Vec<(SessionId, String)>),
}

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

    /// Every live (non-terminal, non-`host_lost`) session: `pending`,
    /// `created`, `guest_ready`, `active`, `idle`, `evacuating`,
    /// `evicting`. This is the rehydration source for the coord's
    /// in-memory routing maps (`repopulate_routing`) — every state
    /// that can carry a live `sandbox_id` binding (`evicting`
    /// included: the sandbox stays bound while the eviction pipeline
    /// runs) MUST be listed here, or a coord restart strands the
    /// session with an unroutable sandbox.
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

    /// ADR 0016 Phase B commit 7 — restart-time rehydration source.
    /// Returns one record per Active session bound to a sandbox on
    /// `host_id`, including the effective disk manifest the host
    /// should rebuild `ChunkedDiskBackend` from: the newer of
    /// `sessions.live_disk_manifest_*` (last FlushScheduler publish)
    /// and the latest recoverable snapshot's `disk_manifest`.
    /// Same resolver semantic as `effective_resume_disk_manifest`
    /// (commit 6); kept server-side so the host issues one query
    /// per restart instead of N+1 round-trips.
    ///
    /// Default impl: scans `list_active_sessions()` + walks
    /// `list_session_snapshots(...)` per row. Postgres overrides
    /// with a single LEFT JOIN. The default exists so test mocks
    /// without a snapshot index still satisfy the trait.
    ///
    /// `Option<ManifestRef>` is `None` when:
    /// - The session never published a live manifest AND has no
    ///   recoverable snapshot — legacy session or sandbox without
    ///   chunked-disk tracking. Host skips rehydration (no
    ///   `attach_chunked_disk` to call).
    /// - The session's status changed mid-query (defensive).
    async fn list_active_sandboxes_on_host_with_disk_manifest(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, Option<ManifestRef>)>, MetaError> {
        let all = self.list_active_sessions().await?;
        let mut out = Vec::new();
        for s in all {
            if !matches!(s.status, SessionState::Active) {
                continue;
            }
            let (Some(h), Some(sb)) = (s.host_id, s.sandbox_id) else {
                continue;
            };
            if h != host_id {
                continue;
            }
            // Pick newer of live vs. latest snapshot. The default
            // impl can't cheaply query "latest snapshot"; default
            // to live_disk_manifest only and let PgMeta's override
            // do the JOIN.
            out.push((s.id, sb, s.live_disk_manifest));
        }
        Ok(out)
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
    /// ADR 0028 A.log: the session's `session_events.idx`
    /// high-water-mark at or before `at` — the event-log leg of a
    /// checkpoint's (memory, disk, event-log) coherence triple,
    /// resolved against the capture's pause instant. `None` when the
    /// session has no events yet (a cursor of "before everything").
    ///
    /// Default `Ok(None)` so mocks without an event log degrade to
    /// "no rewind information" rather than forcing every test double
    /// to model events.
    async fn latest_event_idx_at_or_before(
        &self,
        _sid: SessionId,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<i64>, MetaError> {
        Ok(None)
    }
    /// ADR 0028 Fix A: delete session-bound snapshot rows older than
    /// `retention`, EXCEPT each session's latest (the rung-1 recovery
    /// anchor — never collectible while the session row exists).
    /// Template snapshots (`session_id IS NULL`) are exempt. Returns
    /// the number of rows deleted; chunks they exclusively referenced
    /// become GC candidates via the existing pin-set machinery.
    ///
    /// Default `Ok(0)` so mocks without a snapshots table skip it.
    async fn prune_session_snapshots(
        &self,
        _retention: chrono::Duration,
    ) -> Result<u64, MetaError> {
        Ok(0)
    }
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

    /// ADR 0016 Phase C: pin-set source #3 — every recoverable
    /// snapshot's chunked-disk `ManifestRef` (id + version). Unlike
    /// `list_live_disk_manifest_ids` (which returns Uuid-only for
    /// the `reap_materialize_dir` reaper), the pin-set query needs
    /// the version so `chunk_store.get_manifest` reads the exact
    /// manifest that snapshot row depends on.
    ///
    /// Filtered to `recoverable=true` per the ADR's pin-set
    /// definition: non-recoverable snapshot rows are informational
    /// (bake-time / soft-evicted) and their chunks shouldn't keep
    /// BlobStorage growing forever.
    ///
    /// Default `Ok(vec![])` keeps mocks quiet; PG override returns
    /// DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    async fn list_recoverable_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: pin-set source #4 — every recoverable
    /// snapshot's chunked-memory `ManifestRef` (id + version).
    /// Memory-side mirror of [`list_recoverable_snapshot_disk_manifests`].
    /// VZ snapshots (no chunked memory) drop out via the partial
    /// index on `memory_manifest_id`.
    async fn list_recoverable_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
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

    /// ADR 0028 A.log: rung-1 recovery rewind. Tombstone every live
    /// event with `idx > events_cursor` (set `rewound_at = now()`),
    /// bump the session's `recovery_epoch`, and return a
    /// [`RewindSummary`] (rolled-back count + the surviving
    /// outside-world side-effects detected in that span). Idempotent
    /// in spirit: if nothing is past the cursor, returns
    /// `rolled_back == 0` and the caller emits no boundary.
    ///
    /// Default `Ok(RewindSummary::default())` so mocks without an
    /// event log are a clean no-op.
    async fn rewind_session_to_cursor(
        &self,
        _session_id: SessionId,
        _events_cursor: i64,
    ) -> Result<crate::types::event::RewindSummary, MetaError> {
        Ok(crate::types::event::RewindSummary::default())
    }

    // ---- file artifacts (ADR 0026) ----

    /// Record a shared file artifact for a session. `id` is the
    /// server-generated UUID that also names the blob key; the row
    /// cascades on session delete. `caption` is untrusted text the
    /// caller has already length-capped + control-char-stripped.
    async fn insert_artifact(
        &self,
        id: uuid::Uuid,
        session_id: SessionId,
        blob_key: &str,
        media_type: &str,
        size_bytes: i64,
        caption: Option<&str>,
    ) -> Result<(), MetaError>;

    /// Fetch one artifact **scoped to its session** (so one session can
    /// never read another's blob key). `None` if the `(session_id, id)`
    /// pair doesn't exist. Backs the serve endpoint
    /// `GET /sessions/:id/artifacts/:artifact_id`.
    async fn get_artifact(
        &self,
        session_id: SessionId,
        id: uuid::Uuid,
    ) -> Result<Option<ArtifactRow>, MetaError>;

    /// Per-session artifact usage `(count, total_bytes)` for the upload
    /// quota check. A fresh session with no artifacts returns `(0, 0)`.
    async fn artifact_usage(&self, session_id: SessionId) -> Result<(i64, i64), MetaError>;

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

    // ADR 0021 P1.5a retired the harness-pack registry methods that
    // used to live here. The harness is an image property baked at
    // image-bake time now; there's no deployment-wide table for the
    // MetadataStore to vend.

    // ---- enabled images (Phase 5b) ----
    //
    // Curated allowlist of image URIs that sessions may reference.
    // Manifest is fetched at enable time and persisted on the row,
    // so session-create has zero network dependency on the manifest
    // path. The host-agent still pulls the rootfs blob on first use,
    // but that's lazy + cached separately by digest.

    async fn upsert_enabled_image(&self, image: EnabledImage) -> Result<(), MetaError>;
    /// Live-only — filters `soft_deleted_at IS NULL`. Used by host
    /// advertisement, the dashboard's enabled-images list, and the
    /// session-create handler (where a disabled image must fail with
    /// "not enabled" rather than silently start a session against a
    /// deprecated image).
    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError>;
    /// Live-only lookup (`soft_deleted_at IS NULL`). Use this for
    /// the session-create path; resume callers should use
    /// [`Self::get_enabled_image_any`].
    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError>;
    /// ADR 0021 P1.8: resume-path lookup. Returns the row even if
    /// `soft_deleted_at` is set, so a session whose image was
    /// disabled while it was idle can still resume from the
    /// (chunks-still-pinned) lineage. New-session callers should
    /// use [`Self::get_enabled_image`] (live-filtered).
    async fn get_enabled_image_any(
        &self,
        image_uri: &str,
    ) -> Result<Option<EnabledImage>, MetaError>;
    /// ADR 0021 P1.8: guarded soft-delete. Inside one transaction:
    /// lock the `enabled_images` row, count sessions in
    /// `{pending, created, active, evacuating}` referencing the
    /// image, return [`DisableEnabledImageOutcome::Blocked`] (with
    /// up to 16 blocking sessions) when nonzero, else flip
    /// `soft_deleted_at = NOW()` and return
    /// [`DisableEnabledImageOutcome::Disabled`]. Idempotent: a
    /// second disable of an already-soft-deleted row returns
    /// [`DisableEnabledImageOutcome::AlreadyDisabled`] without
    /// re-bumping the timestamp.
    async fn soft_delete_enabled_image(
        &self,
        image_uri: &str,
    ) -> Result<DisableEnabledImageOutcome, MetaError>;
    /// ADR 0021 P1.8: physical delete reserved for the future
    /// chunk-GC sweeper. Routine "disable" goes through
    /// [`Self::soft_delete_enabled_image`]; only call this when the
    /// row's chunks have been confirmed unreferenced and removed
    /// from BlobStorage.
    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError>;

    /// ADR 0036 P4: content-keyed base-snapshot reuse. Find an
    /// enabled image (INCLUDING soft-deleted rows — their snapshots
    /// stay GC-pinned and restorable) whose bake produced the same
    /// disk content (`disk_manifest_*`, content-derived since ADR
    /// 0036) AND the same `manifest_toml`, and which carries a base
    /// snapshot. The enable pipeline reuses that snapshot instead of
    /// booting a capture VM: with both inputs equal, a fresh capture
    /// is equivalent for every session created from it (bundle
    /// generations are swapped to the host's current staging at
    /// session create — ADR 0035 Invariant 2 — so reuse does not
    /// freeze bundle freshness).
    ///
    /// Default `None`: stores without the query surface (test mocks)
    /// simply never reuse.
    async fn find_enabled_image_by_content(
        &self,
        disk_manifest: ManifestRef,
        manifest_toml: &str,
    ) -> Result<Option<EnabledImage>, MetaError> {
        let _ = (disk_manifest, manifest_toml);
        Ok(None)
    }

    // ---- enable jobs (ADR 0036) ----
    //
    // Async image-enable state machine: `POST /api/enabled-images`
    // records a row and returns 202; the coordinator's
    // `enable_scanner` claims jobs via a lease and drives
    // `pending → materializing → capturing → ready | failed`.
    // Default implementations error — only the Postgres store (the
    // one the scanner runs against) supports jobs; the many test
    // mocks of this trait don't need to stub them out.

    /// Insert a `pending` job for `image_uri`, or — when a
    /// non-terminal job for the same URI already exists (the partial
    /// unique index) — return that job instead. Re-POSTing an
    /// in-flight enable is a resume/no-op, never duplicate work.
    async fn create_or_get_enable_job(
        &self,
        image_uri: &str,
        manifest_digest: Option<&str>,
    ) -> Result<EnableJob, MetaError> {
        let _ = (image_uri, manifest_digest);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    async fn get_enable_job(&self, id: uuid::Uuid) -> Result<Option<EnableJob>, MetaError> {
        let _ = id;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Most-recent-first, bounded. The dashboard's images panel reads
    /// this to surface in-flight + recently-finished enables.
    async fn list_enable_jobs(&self, limit: u32) -> Result<Vec<EnableJob>, MetaError> {
        let _ = limit;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Atomically claim up to `limit` non-terminal jobs whose lease
    /// is free or expired (`claimed_at < now() - lease_secs`),
    /// stamping `(claimed_by, claimed_at)`. Multi-coordinator safety:
    /// one pod owns a job at a time; a crashed pod's claim expires
    /// and a peer re-claims. Progress checkpoints renew the claim.
    async fn claim_enable_jobs(
        &self,
        claimant: &str,
        lease_secs: u32,
        limit: u32,
    ) -> Result<Vec<EnableJob>, MetaError> {
        let _ = (claimant, lease_secs, limit);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Checkpoint materialize progress. Also renews the claim
    /// (`claimed_at = NOW()`) so a long materialize isn't stolen by
    /// a peer mid-run. `chunks_total` is stamped on first call.
    async fn update_enable_job_progress(
        &self,
        id: uuid::Uuid,
        chunks_done: u32,
        chunks_total: Option<u32>,
    ) -> Result<(), MetaError> {
        let _ = (id, chunks_done, chunks_total);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Move the job's state forward (also renews the claim, clears
    /// `error` on non-failed targets, and releases the claim on
    /// terminal states).
    async fn set_enable_job_state(
        &self,
        id: uuid::Uuid,
        state: EnableJobState,
    ) -> Result<(), MetaError> {
        let _ = (id, state);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Record a pipeline failure: bump `attempts`, store `error`,
    /// release the claim (so any pod's next tick can retry), keep
    /// the current state. Returns the post-bump attempt count — the
    /// scanner flips to `failed` once it exceeds the budget.
    async fn record_enable_job_failure(
        &self,
        id: uuid::Uuid,
        error: &str,
    ) -> Result<u32, MetaError> {
        let _ = (id, error);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Admin retry: `failed → pending`, resetting attempts/error/
    /// claim. Errors `NotFound` for unknown ids; `Conflict` when the
    /// job isn't in `failed`.
    async fn retry_enable_job(&self, id: uuid::Uuid) -> Result<EnableJob, MetaError> {
        let _ = id;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

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
    // ADR 0016 §A.1.5c — cross-replica per-session op lease.
    // Serializes mutually-exclusive session-lifecycle ops (idle
    // eviction, resume) so two coord pods can't drive the same
    // session at once. Backed by the `session_lease` table. The
    // contract:
    //   - `try_acquire_session_lease` is atomic INSERT ... ON
    //     CONFLICT DO NOTHING. Returns Ok(true) if the row was
    //     inserted (caller owns the pipeline), Ok(false) if a
    //     row already exists (another caller is mid-pipeline).
    //   - `release_session_lease` is idempotent — extra calls
    //     against an already-deleted row are Ok(()). Used by the
    //     RAII guard's drop path.
    //   - `sweep_stale_session_leases` deletes rows older than
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
    ///
    /// `sandbox_id` is diagnostic-only (records which sandbox the op
    /// concerns): `Some` for an eviction, `None` for a resume — no
    /// sandbox exists yet at resume-lease time, it's about to be
    /// created.
    async fn try_acquire_session_lease(
        &self,
        _session_id: SessionId,
        _sandbox_id: Option<SandboxId>,
        _locked_by: &str,
    ) -> Result<bool, MetaError> {
        Ok(true)
    }

    /// Idempotent. Drop-safe.
    async fn release_session_lease(&self, _session_id: SessionId) -> Result<(), MetaError> {
        Ok(())
    }

    /// Stale-lease reaper. Deletes rows where `locked_at < now() -
    /// max_age` and returns them so the caller can warn-log
    /// `(session_id, locked_by, locked_at)` per reaped row.
    async fn sweep_stale_session_leases(
        &self,
        _max_age: std::time::Duration,
    ) -> Result<Vec<StaleSessionLease>, MetaError> {
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

    /// ADR 0016 Phase C: bump `chunk_generation` independent of a
    /// flush. Used by `enable_image` and `record_snapshot` writes
    /// (commit 2 of the Phase C chain) to keep the barrier complete
    /// across all manifest-lineage-producing paths, not just live
    /// disk flushes. Default is a no-op so non-PG backends opt out
    /// cleanly. PG override increments the single row.
    async fn bump_chunk_generation(&self) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C: enumerate every chunked-disk `ManifestRef`
    /// currently advertised by an `enabled_images` row. Pin-set
    /// source #1: every enabled chunked image pins its base
    /// manifest's chunks. Harness-only images (NULL `disk_manifest_*`
    /// columns) drop out of the query naturally via the partial
    /// index from migration 0036.
    ///
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// returns DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    async fn list_enabled_image_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: enumerate every `(live_disk_manifest_id,
    /// live_disk_manifest_version)` pair currently advertised by a
    /// session row. Pin-set source #2 (the other two are
    /// `list_enabled_image_disk_manifest_ids` and
    /// `list_live_disk_manifest_ids` / `list_live_memory_manifest_ids`
    /// over snapshots).
    ///
    /// Returns DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// runs an indexed `WHERE live_disk_manifest_id IS NOT NULL`
    /// query against the partial index added by migration 0034.
    async fn list_live_session_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: upsert a chunk into the GC candidate table.
    /// Idempotent under `ON CONFLICT (content_hash) DO UPDATE SET
    /// last_seen_at = now()` so re-seeing a candidate refreshes the
    /// diagnostic timestamp without resetting `first_seen_at` (the
    /// 24h grace window must not be extended by repeated sightings).
    ///
    /// `hash` is the raw sha256 digest (`ChunkHash::as_bytes()`
    /// from `engram-chunk-store`). Trait stays primitive at the
    /// boundary to avoid an `engram-core` → `engram-chunk-store`
    /// dep cycle.
    async fn upsert_chunk_gc_candidate(&self, _hash: [u8; 32]) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C promote-pass query. Returns up to `limit`
    /// candidate hashes whose `first_seen_at` predates `cutoff`,
    /// ordered by `first_seen_at` so the oldest backlog drains
    /// first. The caller deletes each from BlobStorage and then
    /// passes the same hashes to [`delete_gc_candidates`].
    ///
    /// Batched (`limit` is required, not optional) so a single
    /// sweep with a large backlog can't OOM the coord pod.
    async fn list_expired_gc_candidates(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
        _limit: i64,
    ) -> Result<Vec<[u8; 32]>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: batch-delete candidate rows after the
    /// BlobStorage delete succeeded. Order doesn't matter; missing
    /// rows are silently skipped (idempotent so a re-run on a
    /// partial promote pass is safe).
    async fn delete_gc_candidates(&self, _hashes: &[[u8; 32]]) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C: paged read of the candidate table for the
    /// `GET /api/admin/chunk-gc/candidates` operator surface. When
    /// `before` is `Some`, filters to rows where `first_seen_at <
    /// before` (matches the promote-pass shape but exposed for
    /// inspection). When `None`, returns everything up to `limit`.
    /// Ordered by `first_seen_at` ascending so the oldest backlog
    /// comes first.
    async fn list_gc_candidates(
        &self,
        _limit: i64,
        _before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<GcCandidateRow>, MetaError> {
        Ok(Vec::new())
    }

    // ----------------------------------------------------------------
    // ADR 0035 — bundle-generation GC (mirrors the chunk GC trio).

    /// ADR 0035 §5: the bundle pin set — every `(drive_id, sha256)`
    /// some `snapshots.aux_bundles` row references. The union (plus
    /// the hosts' reported current generations) is what the GC keeps
    /// and what heartbeat acks advertise as `live_bundles`.
    async fn bundle_pin_set(&self) -> Result<Vec<crate::types::sandbox::AuxBundleRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0035 §5: idempotent candidate upsert; `first_seen_at`
    /// sticky, same grace semantics as the chunk variant. `sha256`
    /// is the 64-hex digest (text — bundle counts are tiny).
    async fn upsert_bundle_gc_candidate(&self, _sha256: &str) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0035 §5 promote-pass query (oldest first, batched).
    async fn list_expired_bundle_gc_candidates(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
        _limit: i64,
    ) -> Result<Vec<String>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0035 §5: batch-delete candidate rows after the blob
    /// delete succeeded. Idempotent.
    async fn delete_bundle_gc_candidates(&self, _sha256s: &[String]) -> Result<(), MetaError> {
        Ok(())
    }

    // ----------------------------------------------------------------
    // ADR 0018 commit 12b — evac_resumer scanner support.
    //
    // The scanner polls `Evacuating` sessions, picks a peer host,
    // and drives `Evacuating → Created → Active`. The retry counter
    // is a side-car on the `sessions` row (column `evac_attempts`,
    // migration 0037). The PG-backed `transition_session(Evacuating)`
    // resets the counter to 0 in the same UPDATE so re-entry from a
    // fresh drain starts fresh; bumps happen via `bump_evac_attempts`
    // (atomic UPDATE ... RETURNING).
    // ----------------------------------------------------------------

    /// Sessions currently in `Evacuating`, paired with their current
    /// `evac_attempts` count. The scanner uses this on every tick.
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// runs an indexed `WHERE status = 'evacuating'` query.
    async fn list_evacuating_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        Ok(Vec::new())
    }

    /// Atomically `evac_attempts = evac_attempts + 1 RETURNING
    /// evac_attempts`. Scanner calls this before each resume attempt;
    /// when the returned count exceeds the budget, scanner gives up
    /// and falls back to Idle. Default returns 1 so test mocks can
    /// observe the bump without persisting state.
    async fn bump_evac_attempts(&self, _session_id: SessionId) -> Result<u32, MetaError> {
        Ok(1)
    }

    // ----------------------------------------------------------------
    // ADR 0034 — eviction scanner + idle-detection backstop support.
    //
    // Mirrors the 12b evac shape above: `Evicting` rows carry a
    // side-car retry counter (column `evict_attempts`, migration
    // 0050) that `transition_session(Evicting)` resets in the same
    // UPDATE, and the coord-side eviction scanner sweeps the state on
    // a 10s tick. The backstop query is the L3 detector: it asks PG —
    // not the host's in-memory hub — which Active sessions have gone
    // silent, catching harness-detach / host-amnesia classes the hub
    // structurally cannot see.
    // ----------------------------------------------------------------

    /// Sessions currently in `Evicting`, paired with their current
    /// `evict_attempts` count. The eviction scanner uses this on
    /// every tick (and on its first tick after coord startup, which
    /// is what recovers rows wedged across a deploy). Default
    /// `Ok(vec![])` keeps in-memory mocks quiet; PG impl runs the
    /// partial-indexed `WHERE status = 'evicting'` query.
    async fn list_evicting_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        Ok(Vec::new())
    }

    /// Atomically `evict_attempts = evict_attempts + 1 RETURNING
    /// evict_attempts`. The eviction scanner calls this before each
    /// pipeline attempt; past the budget it falls back to HostLost
    /// (see ADR 0034 for why not Active/Idle/Dead). Default returns 1
    /// so test mocks can observe the bump without persisting state.
    async fn bump_evict_attempts(&self, _session_id: SessionId) -> Result<u32, MetaError> {
        Ok(1)
    }

    /// ADR 0034 L3 backstop: `Active` sessions with a bound sandbox
    /// whose newest `session_events` row is older than
    /// `idle_for_secs` (falling back to the session's `created_at`
    /// when no events exist yet). These are sessions the host-side
    /// idle detector has gone blind to — harness detached, host-agent
    /// restarted, hub bookkeeping lost — and they would otherwise sit
    /// Active forever. Returns `(session_id, sandbox_id,
    /// last_event_at)`; the caller nominates them into the Evicting
    /// lane. Default empty for non-PG mocks.
    async fn list_active_sessions_idle_past(
        &self,
        _idle_for_secs: i64,
    ) -> Result<Vec<(SessionId, SandboxId, chrono::DateTime<chrono::Utc>)>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0029: fleet-wide snapshot totals for the Storage surface —
    /// the count of `snapshots` rows and the sum of their
    /// `size_bytes`. A single cheap aggregate query (the Storage page
    /// polls on a slow cadence). Default zeros so non-PG mocks stay
    /// quiet; the PG impl runs `SELECT count(*), coalesce(sum(...),0)`.
    async fn snapshot_totals(&self) -> Result<SnapshotTotals, MetaError> {
        Ok(SnapshotTotals::default())
    }

    /// ADR 0029: the number of chunks currently parked in
    /// `chunk_gc_candidates` awaiting their grace window — the
    /// "gc pending" rollup on the Storage surface. A cheap
    /// `SELECT count(*)`; default `0` for non-PG mocks.
    async fn count_gc_candidates(&self) -> Result<u64, MetaError> {
        Ok(0)
    }
}

/// Fleet-wide snapshot aggregate from [`MetadataStore::snapshot_totals`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotTotals {
    pub count: u64,
    pub total_bytes: u64,
}

/// One row from [`MetadataStore::list_gc_candidates`]. Surfaced
/// verbatim through the admin `GET /chunk-gc/candidates` endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcCandidateRow {
    pub content_hash: [u8; 32],
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
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

/// One row from [`MetadataStore::sweep_stale_session_leases`].
/// Carried so the coord-side sweeper can warn-log who held the
/// lease for how long before it was reaped.
#[derive(Clone, Debug)]
pub struct StaleSessionLease {
    pub session_id: SessionId,
    pub sandbox_id: Option<SandboxId>,
    pub locked_by: String,
    pub locked_at: chrono::DateTime<chrono::Utc>,
}
