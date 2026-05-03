use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::event::PersistedEvent;
use crate::types::host::{HostRecord, HostStatus};
use crate::types::ids::{HostId, SandboxId, SessionId};
use crate::types::image::ImageVersion;
use crate::types::registry::{HarnessPack, RegistryCredential};
use crate::types::session::{Session, SessionSpec, SessionStatus};
use crate::types::snapshot::SnapshotRecord;

/// Authoritative source of truth. Postgres-backed in v1; trait exists so
/// we can support SQLite for embedded deployments later.
#[async_trait]
pub trait MetadataStore: Send + Sync {
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

    // ---- images ----
    async fn upsert_image_version(&self, version: ImageVersion) -> Result<(), MetaError>;
    async fn latest_ready_image(&self, repo: &str) -> Result<Option<ImageVersion>, MetaError>;

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
}
