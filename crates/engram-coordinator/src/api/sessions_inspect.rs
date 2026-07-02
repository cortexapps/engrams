//! `GET /sessions/:id/log` — conversation log handler.
//!
//! ADR 0005 retired the git-shaped surfaces (`?kind=workspace`,
//! `/diff`, `/fork`, `/checkpoint`) along with the `git_workdir`
//! coord-side bare-clone manager that backed them. The remaining log
//! handler reads `session_events` straight from Postgres.
//!
//! `?kind=` is kept as a query parameter solely so callers that hit
//! `?kind=conversation` (the only valid value, also the default) keep
//! working without a 400. Any other value returns 400 — the workspace
//! variant is gone.

use chrono::Utc;
use engram_core::types::SessionState;
use engram_core::SessionId;
use serde::Serialize;
#[cfg(test)]
use serde_json::json;

use crate::cow_state::{fetch_for_host, CowStateView};
use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Serialize, Debug)]
pub struct ConversationEntry {
    pub idx: i64,
    pub kind: String,
    pub at: chrono::DateTime<Utc>,
    pub payload: serde_json::Value,
}

// ----------------------------------------------------------------
// ADR 0051: transport-agnostic `_core` entry points for the app-gRPC
// SessionService. Same logic as the axum handlers, reshaped to return
// the plain bodies the gRPC converters consume. The axum handlers below
// delegate to these.
// ----------------------------------------------------------------

/// gRPC `GetLog` core. `kind` defaults to `conversation`; anything else is a
/// 400. `limit` defaults to 200, hard-capped at 1000.
pub(crate) async fn get_log_core(
    state: &SharedState,
    id: SessionId,
    kind: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<ConversationEntry>, ApiError> {
    let _session = state.services.meta.get_session(id).await?;
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    match kind.as_deref().unwrap_or("conversation") {
        "conversation" => {
            let rows = state
                .services
                .meta
                .list_session_events_since(id, -1, limit)
                .await?;
            Ok(rows
                .into_iter()
                .map(|e| ConversationEntry {
                    idx: e.idx,
                    kind: e.kind,
                    at: e.created_at,
                    payload: e.payload,
                })
                .collect())
        }
        other => Err(ApiError::BadRequest(format!(
            "unknown kind `{other}` — only `conversation` is supported"
        ))),
    }
}

/// gRPC `GetCowState` core. `None` when the session has no live sandbox
/// (Idle / HostLost / terminal / Pending) or the host doesn't know it.
pub(crate) async fn cow_state_core(
    state: &SharedState,
    id: SessionId,
) -> Result<Option<CowStateView>, ApiError> {
    let session = state.services.meta.get_session(id).await?;
    let (host_id, sandbox_id) = match (session.host_id, session.sandbox_id, session.status) {
        (
            Some(h),
            Some(sb),
            SessionState::Active | SessionState::Created | SessionState::GuestReady,
        ) => (h, sb),
        _ => {
            // No live sandbox for this session (Idle, HostLost,
            // terminal, or still Pending).
            return Ok(None);
        }
    };
    let backend = state.host_registry.backend_of(host_id).ok_or_else(|| {
        ApiError::HostLost(format!(
            "session {id} bound to host {host_id} which is no longer registered"
        ))
    })?;
    let records = fetch_for_host(&state.cow_state_cache, host_id, backend)
        .await
        .map_err(ApiError::from)?;
    let Some(record) = records.into_iter().find(|r| r.sandbox_id == sandbox_id) else {
        // Host doesn't know this sandbox (transient mid-create, or
        // it's a non-chunk-tracked backend on this host).
        return Ok(None);
    };
    // Memory-tier enrichment from the session's latest snapshot
    // row. Same shape as the per-host handler.
    let (memory_manifest, last_snapshot_at) =
        match state.services.meta.latest_snapshot_for_session(id).await {
            Ok(Some(rec)) => (rec.memory_manifest, Some(rec.created_at)),
            Ok(None) => (None, None),
            Err(_) => (None, None),
        };
    Ok(Some(CowStateView::from_record(
        &record,
        Some(id),
        memory_manifest,
        last_snapshot_at,
    )))
}

/// ADR 0028 A.log: one checkpoint in a session's chain — the data
/// behind the durability timeline + the (future) fork-point picker.
#[derive(Serialize)]
pub struct CheckpointSummary {
    pub snapshot_id: String,
    pub created_at: chrono::DateTime<Utc>,
    pub size_bytes: u64,
    /// The (memory, disk, event-log) coherence triple's third leg —
    /// the transcript cursor a rung-1 rewind / fork would cut at.
    pub events_cursor: Option<i64>,
    /// HEAD-verified durable in BlobStorage (rung-1-eligible).
    pub recoverable: bool,
    /// True for the newest checkpoint — the always-pinned rung-1
    /// recovery anchor.
    pub is_latest: bool,
}

/// gRPC `ListCheckpoints` core. Newest-first checkpoint chain.
pub(crate) async fn checkpoints_core(
    state: &SharedState,
    id: SessionId,
) -> Result<Vec<CheckpointSummary>, ApiError> {
    state.services.meta.get_session(id).await?;
    let rows = state.services.meta.list_snapshots_for_session(id).await?;
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| CheckpointSummary {
            snapshot_id: r.id.to_string(),
            created_at: r.created_at,
            size_bytes: r.size_bytes,
            events_cursor: r.events_cursor,
            recoverable: r.recoverable,
            // list_snapshots_for_session is ORDER BY created_at DESC,
            // so index 0 is the latest = the rung-1 anchor.
            is_latest: i == 0,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::SessionMode;
    use engram_core::types::Session;
    use engram_core::types::SessionState;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_for_session(session: Session) -> (SharedState, TempDir) {
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(local.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::with_noop_hub(backend.clone()),
        );
        host_registry.register(engram_core::HostId::new(), local_host);
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, local)
    }

    fn ephemeral_session(id: engram_core::SessionId) -> Session {
        Session {
            id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            selected_skills: Vec::new(),
        }
    }

    #[tokio::test]
    async fn log_conversation_returns_session_events_from_postgres() {
        let session_id = engram_core::SessionId::new();
        let (state, _local) = build_state_for_session(ephemeral_session(session_id));

        // Append a couple of synthetic events directly via meta so we
        // don't have to set up a full agent run.
        state
            .services
            .meta
            .append_session_event(session_id, "harness_run_started", json!({}))
            .await
            .unwrap();
        state
            .services
            .meta
            .append_session_event(session_id, "harness_idle", json!({}))
            .await
            .unwrap();

        let events = get_log_core(
            &state, session_id, None, // kind defaults to conversation
            None, // limit defaults to 200
        )
        .await
        .expect("log conversation");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "harness_run_started");
        assert_eq!(events[1].kind, "harness_idle");
    }

    #[tokio::test]
    async fn unknown_kind_returns_400() {
        let session_id = engram_core::SessionId::new();
        let (state, _local) = build_state_for_session(ephemeral_session(session_id));

        let err = get_log_core(&state, session_id, Some("workspace".into()), None)
            .await
            .expect_err("workspace kind retired in ADR 0005");
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
