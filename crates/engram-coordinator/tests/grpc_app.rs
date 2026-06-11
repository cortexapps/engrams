//! Smoke test for the ADR 0039 app gRPC scaffold (plan Task 8).
//!
//! Serves the real `grpc_app::server` router on an ephemeral port and
//! asserts a tonic client gets `Code::Unimplemented` back — proving
//! the five services are mounted and answering, without any business
//! logic behind them yet (that's Tasks 10-13).
//!
//! The `StubMeta` mock below is a deliberate duplicate of the
//! minimal-mock pattern in `dead_host_mock.rs` — the richer fixtures
//! in `api.rs` live inside that test binary and aren't importable.
//! The stubs never touch state, so every method is inert.

use std::sync::Arc;

use async_trait::async_trait;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{grpc_app, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SessionId};
use engram_protocol::app;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;

/// Inert in-memory `MetadataStore`. The Task 8 stubs never reach the
/// store; this only exists so `Services`/`AppState` can be built.
struct StubMeta;

#[async_trait]
impl MetadataStore for StubMeta {
    async fn create_session(&self, _spec: SessionSpec) -> Result<SessionId, MetaError> {
        Ok(SessionId::new())
    }
    async fn create_session_created(
        &self,
        _session_id: SessionId,
        _spec: SessionSpec,
        _host_id: HostId,
        _sandbox_id: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_session(&self, _id: SessionId) -> Result<Session, MetaError> {
        Err(MetaError::NotFound)
    }
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(Vec::new())
    }
    async fn transition_session(
        &self,
        _id: SessionId,
        _target: SessionState,
    ) -> Result<SessionState, MetaError> {
        Err(MetaError::NotFound)
    }
    async fn assign_session_host(
        &self,
        _id: SessionId,
        _host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn assign_session_sandbox(
        &self,
        _id: SessionId,
        _sandbox_id: Option<engram_core::SandboxId>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_host(&self, _h: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn set_host_status(&self, _id: HostId, _s: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }
    async fn touch_host_heartbeat(
        &self,
        _id: HostId,
        _s: HostStatus,
        _cap: engram_core::types::HostCapacity,
        _util: engram_core::types::HostUtilization,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_stale_hosts(&self, _threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        _host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        Ok(Vec::new())
    }
    async fn record_snapshot(&self, _s: SnapshotRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_snapshots_for_session(
        &self,
        _id: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn latest_snapshot_for_session(
        &self,
        _id: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(None)
    }
    async fn append_session_event(
        &self,
        _s: SessionId,
        _k: &str,
        _p: serde_json::Value,
    ) -> Result<i64, MetaError> {
        Ok(0)
    }
    async fn list_session_events_since(
        &self,
        _s: SessionId,
        _since: i64,
        _limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(Vec::new())
    }
    async fn insert_artifact(
        &self,
        _: uuid::Uuid,
        _: SessionId,
        _: &str,
        _: &str,
        _: i64,
        _: Option<&str>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_artifact(
        &self,
        _: SessionId,
        _: uuid::Uuid,
    ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
        Ok(None)
    }
    async fn artifact_usage(&self, _: SessionId) -> Result<(i64, i64), MetaError> {
        Ok((0, 0))
    }
    async fn upsert_registry_credential(
        &self,
        _: engram_core::types::RegistryCredential,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_registry_credentials(
        &self,
    ) -> Result<Vec<engram_core::types::RegistryCredential>, MetaError> {
        Ok(Vec::new())
    }
    async fn registry_credential_for_host(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
        Ok(None)
    }
    async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_enabled_image(
        &self,
        _: engram_core::types::EnabledImage,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_enabled_images(
        &self,
    ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
        Ok(Vec::new())
    }
    async fn get_enabled_image(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(None)
    }
    async fn get_enabled_image_any(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(None)
    }
    async fn soft_delete_enabled_image(
        &self,
        _: &str,
    ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
        Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled)
    }
    async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_session_secrets(
        &self,
        _: engram_core::types::SessionSecrets,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_session_secrets(
        &self,
        _: SessionId,
    ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
        Ok(None)
    }
    async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
        Ok(())
    }
}

/// Minimal fully-wired `AppState`, mirroring the in-memory fixture in
/// `api.rs::build_app_with_tokens` (which isn't importable from here).
fn test_state() -> Arc<AppState> {
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let blob = || {
        Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        ))
    };
    let services = Services {
        meta: Arc::new(StubMeta),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob: blob(),
        chunk_store: engram_chunk_store::ChunkStore::new(blob()),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
    };
    Arc::new(AppState::new(CoordinatorConfig::default(), services))
}

#[tokio::test]
async fn app_grpc_scaffold_answers_unimplemented() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming from listener");
    let server = tokio::spawn(grpc_app::server(test_state()).serve_with_incoming(incoming));

    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC");

    // The plan's Task 8 outcome: ListSessions answers Unimplemented.
    let err = app::session_service_client::SessionServiceClient::new(channel.clone())
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect_err("scaffold must refuse ListSessions");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    // One probe per remaining service proves all five are mounted on
    // the one listener (an unmounted service would answer
    // `Unimplemented` too via tonic's fallback — but with a different
    // message, so the message assert above plus these keep us honest).
    let err = app::fleet_service_client::FleetServiceClient::new(channel.clone())
        .list_hosts(app::ListHostsRequest::default())
        .await
        .expect_err("scaffold must refuse ListHosts");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let err = app::image_service_client::ImageServiceClient::new(channel.clone())
        .list_enabled_images(app::ListEnabledImagesRequest::default())
        .await
        .expect_err("scaffold must refuse ListEnabledImages");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let err = app::secret_service_client::SecretServiceClient::new(channel.clone())
        .has_secret(app::HasSecretRequest::default())
        .await
        .expect_err("scaffold must refuse HasSecret");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let outbound = tokio_stream::iter(Vec::<app::RelayShellRequest>::new());
    let err = app::shell_relay_service_client::ShellRelayServiceClient::new(channel)
        .relay(outbound)
        .await
        .expect_err("scaffold must refuse Relay");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    server.abort();
}
