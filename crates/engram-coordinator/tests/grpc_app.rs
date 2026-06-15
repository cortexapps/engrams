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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{grpc_app, AppState, CoordinatorConfig, Services};
use engram_core::traits::{MetadataStore, SealedSecretRow};
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SessionId};
use engram_protocol::app;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;

/// Stateful in-memory `MetadataStore` backed by a real `HashMap` so
/// sealed-secret operations actually persist across calls.
struct StubMeta {
    secrets: Mutex<HashMap<String, SealedSecretRow>>,
}

impl StubMeta {
    fn new() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
        }
    }
}

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
    async fn set_host_cordoned(&self, _id: HostId, _cordoned: bool) -> Result<(), MetaError> {
        Ok(())
    }
    async fn touch_host_heartbeat(
        &self,
        _id: HostId,
        _hb: engram_core::types::HostHeartbeat,
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
    async fn put_sealed_secret(
        &self,
        key: &str,
        wrapped_dek: Vec<u8>,
        nonce: Vec<u8>,
        ciphertext: Vec<u8>,
        key_id: String,
    ) -> Result<(), MetaError> {
        let row = SealedSecretRow {
            key: key.to_string(),
            wrapped_dek,
            nonce,
            ciphertext,
            key_id,
        };
        self.secrets.lock().unwrap().insert(key.to_string(), row);
        Ok(())
    }
    async fn has_sealed_secret(&self, key: &str) -> Result<bool, MetaError> {
        Ok(self.secrets.lock().unwrap().contains_key(key))
    }
    async fn get_sealed_secret(
        &self,
        key: &str,
    ) -> Result<Option<engram_core::traits::SealedSecretRow>, MetaError> {
        Ok(self.secrets.lock().unwrap().get(key).cloned())
    }
    async fn delete_sealed_secret(&self, key: &str) -> Result<(), MetaError> {
        self.secrets.lock().unwrap().remove(key);
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
        meta: Arc::new(StubMeta::new()),
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
/// matching bearer, passes auth. All five services are now implemented
/// (Tasks 10-13). Proves each service is mounted and answering.
#[tokio::test]
async fn app_grpc_scaffold_answers_unimplemented_with_valid_bearer() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;

    let channel = dial(addr).await;

    // Task 10: ListSessions is implemented — returns an empty list
    // against the stub MetadataStore (no sessions exist).
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

    // Task 13: Fleet is now implemented — ListHosts returns empty list.
    let resp = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_hosts(app::ListHostsRequest::default())
    .await
    .expect("FleetService ListHosts must succeed (Task 13 implemented)");
    assert!(resp.into_inner().hosts.is_empty(), "stub: no hosts");

    // Task 13: Image is now implemented — ListEnabledImages returns empty list.
    let resp = app::image_service_client::ImageServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_enabled_images(app::ListEnabledImagesRequest::default())
    .await
    .expect("ImageService ListEnabledImages must succeed (Task 13 implemented)");
    assert!(resp.into_inner().images.is_empty(), "stub: no images");

    // Task 13: Secret is now implemented — HasSecret returns false (stub).
    let resp = app::secret_service_client::SecretServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .has_secret(app::HasSecretRequest::default())
    .await
    .expect("SecretService HasSecret must succeed (Task 13 implemented)");
    assert!(!resp.into_inner().exists, "stub returns false");

    let outbound = tokio_stream::iter(Vec::<app::RelayShellRequest>::new());
    let err = app::shell_relay_service_client::ShellRelayServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .relay(outbound)
    .await
    .expect_err("Relay: empty stream must return InvalidArgument (no open frame)");
    // Task 12: Relay went live in Task 12; an empty inbound stream →
    // InvalidArgument "relay stream ended before open frame" (the old
    // Unimplemented stub is gone).
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert_eq!(err.message(), "relay stream ended before open frame");

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

/// Task 13 / M1: harness_secret_id is wired; gate (image lookup) runs first.
///
/// With a non-enabled image the gate sets `is_builtin_claude = false` and
/// returns early (no unseal). The request then fails at image resolution with
/// `InvalidArgument` (image not enabled). The secret id is irrelevant — it is
/// never consulted when the image gate says "not claude".
#[tokio::test]
async fn create_session_with_missing_harness_secret_gate_runs_first() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;
    let channel = dial(addr).await;

    let err = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .create_session(app::CreateSessionRequest {
        image_uri: "localhost:5001/demo:warm".into(),
        mode: "agent".into(),
        prompt: None,
        harness_secret_id: Some("nonexistent-secret".into()),
        secrets: std::collections::HashMap::new(),
        user_email: None,
        user_name: None,
    })
    .await
    .expect_err("non-enabled image → error");
    // After M1: the image gate runs BEFORE unseal. The stub returns
    // Ok(None) for get_enabled_image, so is_builtin_claude = false, and
    // build_harness_secret_env returns empty env without touching the
    // secret store. The session handler then fails at image resolution
    // (image not enabled → InvalidArgument). The secret is never read.
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "non-enabled image must return InvalidArgument (gate before unseal — M1): {err:?}"
    );

    server.abort();
}

/// Real crypto round-trip: StubMeta now persists; put → has(true) → get+inject → delete → has(false).
/// Proves that seal→store→unseal→inject runs with REAL crypto in-process.
#[tokio::test]
async fn secret_service_real_crypto_round_trip() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;
    let channel = dial(addr).await;
    let mut client = app::secret_service_client::SecretServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // put — real seal runs (EnvVarKeyProvider KEK, real CredCipher).
    client
        .put_secret(app::PutSecretRequest {
            key: "u1".into(),
            value: "s3cr3t-value".into(),
        })
        .await
        .expect("PutSecret");

    // has → true
    let has = client
        .has_secret(app::HasSecretRequest { key: "u1".into() })
        .await
        .expect("HasSecret after put");
    assert!(
        has.into_inner().exists,
        "has_secret must return true after put"
    );

    // delete
    client
        .delete_secret(app::DeleteSecretRequest { key: "u1".into() })
        .await
        .expect("DeleteSecret");

    // has → false
    let has2 = client
        .has_secret(app::HasSecretRequest { key: "u1".into() })
        .await
        .expect("HasSecret after delete");
    assert!(
        !has2.into_inner().exists,
        "has_secret must return false after delete"
    );

    server.abort();
}

/// Injection test: seal+store a secret, then verify seal→store→unseal round-trip
/// with real crypto by directly reading from StubMeta and calling CredCipher::open.
/// Proves the crypto path end-to-end in-process (no harness or image resolution needed).
#[tokio::test]
async fn secret_real_crypto_unseal_via_grpc_put() {
    // Build a state with real EnvVarKeyProvider (zeros KEK, "test:v1" key_id).
    let state = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state.clone()).await;
    let channel = dial(addr).await;
    let mut client = app::secret_service_client::SecretServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // Seal "oauth-tok-42" via the real gRPC path (real CredCipher + real KEK).
    client
        .put_secret(app::PutSecretRequest {
            key: "inject-test".into(),
            value: "oauth-tok-42".into(),
        })
        .await
        .expect("PutSecret must succeed");

    // The secret is in the StubMeta store. Prove get_sealed_secret returns Some.
    let row = state
        .services
        .meta
        .get_sealed_secret("inject-test")
        .await
        .expect("get_sealed_secret")
        .expect("row must be present after put");

    // Unseal manually to verify crypto end-to-end.
    let nonce: [u8; 12] = row.nonce.as_slice().try_into().expect("nonce length");
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: row.wrapped_dek,
        nonce,
        ciphertext: row.ciphertext,
        key_id: row.key_id,
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plaintext = cipher
        .open(&sealed)
        .await
        .expect("unseal must succeed with matching KEK");
    assert_eq!(
        std::str::from_utf8(&plaintext).unwrap(),
        "oauth-tok-42",
        "unsealed plaintext must match what was put"
    );
    println!("real-crypto: seal→store→unseal round-trip verified");

    server.abort();
}

/// Task 13: FleetService is now implemented — ListHosts returns empty list
/// (stub state), GetStorageSummary returns zeros.
#[tokio::test]
async fn fleet_service_list_hosts_and_storage_summary() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;
    let channel = dial(addr).await;
    let mut client = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    );

    let resp = client
        .list_hosts(app::ListHostsRequest::default())
        .await
        .expect("ListHosts must succeed (Task 13)");
    assert!(resp.into_inner().hosts.is_empty(), "stub: no hosts");

    let resp = client
        .get_storage_summary(app::GetStorageSummaryRequest::default())
        .await
        .expect("GetStorageSummary must succeed (Task 13)");
    let s = resp.into_inner();
    assert_eq!(s.tracked_sandboxes, 0);

    server.abort();
}

/// Task 13: ImageService is now implemented — ListEnabledImages returns empty list.
#[tokio::test]
async fn image_service_list_enabled_images() {
    let (addr, server) = serve(test_state(vec![TEST_TOKEN.into()])).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let resp = client
        .list_enabled_images(app::ListEnabledImagesRequest::default())
        .await
        .expect("ListEnabledImages must succeed (Task 13)");
    assert!(resp.into_inner().images.is_empty(), "stub: no images");

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
