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

use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::Utc;
use engram_core::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Deserialize, Default)]
pub struct LogQuery {
    /// Only `"conversation"` (or unset, defaults to conversation) is
    /// accepted post-ADR-0005. Anything else → 400.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap on number of rows returned. Defaults to 200, hard-capped
    /// at 1000 to keep responses bounded.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ConversationEntry {
    pub idx: i64,
    pub kind: String,
    pub at: chrono::DateTime<Utc>,
    pub payload: serde_json::Value,
}

pub async fn log(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(params): Query<LogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _session = state.services.meta.get_session(id).await?;
    let limit = params.limit.unwrap_or(200).clamp(1, 1000);

    match params.kind.as_deref().unwrap_or("conversation") {
        "conversation" => {
            let rows = state
                .services
                .meta
                .list_session_events_since(id, -1, limit)
                .await?;
            let entries: Vec<ConversationEntry> = rows
                .into_iter()
                .map(|e| ConversationEntry {
                    idx: e.idx,
                    kind: e.kind,
                    at: e.created_at,
                    payload: e.payload,
                })
                .collect();
            Ok(Json(json!({
                "session_id": id,
                "kind": "conversation",
                "events": entries,
            })))
        }
        other => Err(ApiError::BadRequest(format!(
            "unknown kind `{other}` — only `conversation` is supported"
        ))),
    }
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
    use engram_core::types::session::HarnessSpec;
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
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:test".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
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

        let resp = log(
            State(state),
            Path(session_id),
            Query(LogQuery {
                kind: None, // defaults to conversation
                limit: None,
            }),
        )
        .await
        .expect("log conversation");

        let v = resp.0;
        assert_eq!(v["kind"], "conversation");
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], "harness_run_started");
        assert_eq!(events[1]["kind"], "harness_idle");
    }

    #[tokio::test]
    async fn unknown_kind_returns_400() {
        let session_id = engram_core::SessionId::new();
        let (state, _local) = build_state_for_session(ephemeral_session(session_id));

        let err = log(
            State(state),
            Path(session_id),
            Query(LogQuery {
                kind: Some("workspace".into()),
                limit: None,
            }),
        )
        .await
        .expect_err("workspace kind retired in ADR 0005");
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
