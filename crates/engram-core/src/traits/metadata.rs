use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::MetaError;
use crate::types::event::PersistedEvent;
use crate::types::host::{HostRecord, HostStatus};
use crate::types::ids::{HostId, SandboxId, SessionId, SnapshotId};
use crate::types::registry::{EnabledImage, HarnessPack, RegistryCredential, SessionSecrets};
use crate::types::session::{Session, SessionSpec, SessionStatus};
use crate::types::snapshot::SnapshotRecord;

/// Sealed cold-tier blob ref + accompanying envelope-encryption
/// fields, mirroring the shape that `engram-crypto::CredCipher::seal`
/// produces (and that `registry_credentials` / `session_secrets`
/// rows already use). The plaintext blob URL never lands in
/// Postgres — `engram-coordinator::blob::open_blob_ref` unseals on
/// demand.
#[derive(Clone, Debug)]
pub struct SealedBlobRef {
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
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
    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError>;
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError>;
    async fn set_session_status(
        &self,
        id: SessionId,
        status: SessionStatus,
    ) -> Result<(), MetaError>;
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

    // ---- cold-tier (ADR 0005 / Stage 4+) ----

    /// Return the most-recently-created snapshot for `sid` whose
    /// `blob_present = true`, paired with its sealed blob ref. The
    /// cross-host cold-resume path (Stage 6) opens the sealed ref via
    /// the deployment KEK and asks the picked host to download +
    /// untar. `Ok(None)` when no cold copy exists.
    async fn latest_cold_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<(SnapshotRecord, SealedBlobRef)>, MetaError>;

    /// Atomic flush: write the sealed blob ref onto a snapshot row,
    /// clear its `local_path`, transition the owning session
    /// `Idle → ColdEvicted`, set `cold_evicted_at`. Idempotent: when
    /// the snapshot already has `blob_present = true`, no rows are
    /// modified and `Ok(())` is returned. The Postgres impl runs
    /// this in a single transaction.
    async fn flush_to_cold(
        &self,
        session_id: SessionId,
        snapshot_id: SnapshotId,
        sealed: SealedBlobRef,
        flushed_at: DateTime<Utc>,
    ) -> Result<(), MetaError>;

    /// Drop the local-path mark on a snapshot row whose bytes are
    /// already in cold tier. Used by the disk-pressure detector's
    /// cheap-drop path: when `blob_present` is already true we just
    /// reclaim disk by removing the local copy + clearing the column.
    async fn clear_local_path(&self, snapshot_id: SnapshotId) -> Result<(), MetaError>;

    /// Return every session with `status = 'idle'`. Used by Stage 5's
    /// admin `flush-idle` endpoint and Stage 7's disk-pressure
    /// detector's victim picker.
    async fn list_idle_sessions(&self) -> Result<Vec<Session>, MetaError>;

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
