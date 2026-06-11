//! Smoke test for the ADR 0039 app gRPC scaffold (plan Tasks 8 + 9).
//!
//! Serves the real `grpc_app::server` router on an ephemeral port and
//! asserts, through a real tonic client:
//!  - no/wrong bearer → `Code::Unauthenticated` (Task 9);
//!  - valid bearer → `Code::Unimplemented` "ADR 0039 phase 2",
//!    proving the five services are mounted and answering, without
//!    any business logic behind them yet (that's Tasks 10-13);
//!  - a server configured with ZERO tokens rejects everything
//!    (fail closed — the opposite of the axum surface's dev bypass).
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
/// `app_grpc_tokens` is the Task 9 service-bearer allow-list; empty =
/// fail closed.
fn test_state(app_grpc_tokens: Vec<String>) -> Arc<AppState> {
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
    let cfg = CoordinatorConfig {
        app_grpc_tokens,
        ..CoordinatorConfig::default()
    };
    Arc::new(AppState::new(cfg, services))
}

/// Token the test server is configured with — the happy-path credential.
const TEST_TOKEN: &str = "test-app-grpc-token";

/// Bring up the app-gRPC server on an ephemeral port; returns the dial
/// address and the join handle so the caller can `abort()` it.
async fn serve(state: Arc<AppState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming from listener");
    let handle = tokio::spawn(async move {
        let _ = grpc_app::server(state).serve_with_incoming(incoming).await;
    });
    (addr, handle)
}

/// Dial the app-gRPC server at `addr` and return a connected channel.
/// Extracted from the three tests below, which all open the same
/// plaintext-HTTP/2 channel to an ephemeral 127.0.0.1 port.
async fn dial(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC")
}

/// tonic interceptor that stamps `Authorization: Bearer <token>` onto
/// every outbound request — the only way to set per-call metadata short
/// of hand-building each request.
// `result_large_err`: the `Result<_, tonic::Status>` shape is what
// tonic's `with_interceptor` requires; can't box it away.
#[allow(clippy::result_large_err)]
fn bearer(
    token: &'static str,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("ascii header"),
        );
        Ok(req)
    }
}

/// Happy path: a server configured with `TEST_TOKEN`, called with a
/// matching bearer, passes auth.
///
/// Task 10: SessionService's unary six (ListSessions, GetSession,
/// DeleteSession, CreateSession, SendPrompt, Interrupt) are now real
/// implementations. `ListSessions` returns an empty list against the
/// in-memory state; the other four services still respond
/// `Unimplemented` "ADR 0039 phase 2" (Tasks 11-13). Proves all five
/// services are mounted on the listener.
#[tokio::test]
async fn app_grpc_scaffold_answers_unimplemented_with_valid_bearer() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;

    let channel = dial(addr).await;

    // Task 10: ListSessions is now implemented — returns an empty list
    // against the stub MetadataStore (no sessions exist in the
    // in-memory fixture). Previously returned Unimplemented; now Ok.
    let resp = app::session_service_client::SessionServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect("ListSessions must succeed with a valid bearer (Task 10 implemented)");
    assert!(
        resp.into_inner().sessions.is_empty(),
        "stub state has no sessions — list must be empty"
    );

    // One probe per remaining service proves all five are mounted on
    // the one listener (an unmounted service would answer
    // `Unimplemented` too via tonic's fallback — but with a different
    // message, so the message assert here plus these keep us honest).
    let err = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_hosts(app::ListHostsRequest::default())
    .await
    .expect_err("FleetService stubs must still refuse (Tasks 11-13 pending)");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let err = app::image_service_client::ImageServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_enabled_images(app::ListEnabledImagesRequest::default())
    .await
    .expect_err("ImageService stubs must still refuse (Tasks 11-13 pending)");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let err = app::secret_service_client::SecretServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .has_secret(app::HasSecretRequest::default())
    .await
    .expect_err("SecretService stubs must still refuse (Tasks 11-13 pending)");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    let outbound = tokio_stream::iter(Vec::<app::RelayShellRequest>::new());
    let err = app::shell_relay_service_client::ShellRelayServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .relay(outbound)
    .await
    .expect_err("ShellRelayService stubs must still refuse (Task 21 pending)");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "ADR 0039 phase 2");

    server.abort();
}

/// Task 9: on a token-configured server, a call with no bearer and a
/// call with the wrong bearer are both rejected `Unauthenticated` —
/// the check fires before the stub, so we never see `Unimplemented`.
#[tokio::test]
async fn app_grpc_rejects_missing_and_wrong_bearer() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;

    let channel = dial(addr).await;

    // No Authorization metadata at all.
    let err = app::session_service_client::SessionServiceClient::new(channel.clone())
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect_err("missing bearer must be refused");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    // A bearer that isn't in the allow-list.
    let err = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer("not-the-token"),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect_err("wrong bearer must be refused");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    server.abort();
}

/// Task 9 fail-closed: a server built with ZERO configured tokens
/// rejects everything — even a syntactically valid bearer. This is the
/// deliberate opposite of the axum surface's empty-list dev bypass; a
/// deployment that forgets `ENGRAM_APP_GRPC_TOKENS` serves a closed
/// door, not an open admin plane.
#[tokio::test]
async fn app_grpc_fails_closed_with_no_tokens_configured() {
    let (addr, server) = serve(test_state(vec![])).await;

    let channel = dial(addr).await;

    let err = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect_err("fail-closed: no tokens means reject everything");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    server.abort();
}
