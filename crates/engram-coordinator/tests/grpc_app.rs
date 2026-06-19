//! In-process integration tests for the app-gRPC surface (ADR 0039 /
//! ADR 0051). The coordinator's web-facing REST surface is gone; the
//! orchestrator drives the coordinator exclusively over the four tonic
//! services in `grpc_app/` (SessionService, ShellRelayService,
//! FleetService, ImageService). This file is the gRPC replacement for the
//! REST coverage that `tests/api.rs` used to carry over `tower::oneshot`
//! against `api::router`.
//!
//! It serves the real `grpc_app::server` router on an ephemeral
//! 127.0.0.1 port and drives real tonic clients through it, exercising:
//!   - bearer auth: valid / missing / wrong → Unauthenticated; a server
//!     configured with ZERO tokens fails closed (the deliberate opposite
//!     of the old axum empty-list dev bypass);
//!   - SessionService create → get → list → delete round-trips;
//!   - bad-UUID → InvalidArgument, not-found → NotFound error mapping;
//!   - FleetService list_hosts + get_storage_summary;
//!   - ImageService list_enabled_images.
//!
//! The `MockMetadataStore` below is a self-contained copy of the fixture
//! in `tests/api.rs` (the two test binaries can't import each other's
//! mocks). It implements OUR base's full `MetadataStore` trait against
//! in-memory `HashMap`s, so create/get/list/delete actually persist and
//! the round-trip assertions are real.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
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
use parking_lot::Mutex;

// ---------------------------------------------------------------------
// MockMetadataStore — in-memory MetadataStore for end-to-end gRPC tests.
// A self-contained copy of tests/api.rs::MockMetadataStore (minus the
// retired user_id field), tracking rows so create→get→list→delete
// round-trips are exercised for real.
// ---------------------------------------------------------------------

#[derive(Default)]
struct MockMetadataStore {
    sessions: Mutex<HashMap<SessionId, Session>>,
    snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
    snapshots_by_id: Mutex<HashMap<engram_core::types::SnapshotId, SnapshotRecord>>,
    enabled: Mutex<HashMap<String, engram_core::types::EnabledImage>>,
    events: Mutex<HashMap<SessionId, Vec<PersistedEvent>>>,
    next_event_idx: Mutex<HashMap<SessionId, i64>>,
    live_disk_manifests: Mutex<HashMap<SessionId, engram_core::types::manifest::ManifestRef>>,
    chunk_generation: std::sync::atomic::AtomicU64,
    hosts: Mutex<HashMap<HostId, HostRecord>>,
    lease_held: std::sync::atomic::AtomicBool,
}

impl MockMetadataStore {
    fn new() -> Self {
        Self::default()
    }

    fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        let id = SessionId::new();
        let session = Session {
            id,
            status: SessionState::Pending,
            host_id: None,
            sandbox_id: None,
            created_at: Utc::now(),
            image: spec.image,
            mode: spec.mode,
            last_active_at: Utc::now(),
            live_disk_manifest: None,
        };
        self.sessions.lock().insert(id, session);
        Ok(id)
    }

    async fn create_session_created(
        &self,
        session_id: SessionId,
        spec: SessionSpec,
        host_id: engram_core::HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        let session = Session {
            id: session_id,
            status: SessionState::Created,
            host_id: Some(host_id),
            sandbox_id: Some(sandbox_id),
            created_at: Utc::now(),
            image: spec.image,
            mode: spec.mode,
            last_active_at: Utc::now(),
            live_disk_manifest: None,
        };
        self.sessions.lock().insert(session_id, session);
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        self.sessions
            .lock()
            .get(&id)
            .cloned()
            .ok_or(MetaError::NotFound)
    }

    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(self
            .sessions
            .lock()
            .values()
            .filter(|s| s.status.is_live())
            .cloned()
            .collect())
    }

    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        let prev = s.status;
        prev.try_transition_to(target)
            .map_err(|e| MetaError::Conflict(e.to_string()))?;
        s.status = target;
        s.last_active_at = Utc::now();
        Ok(prev)
    }

    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.host_id = host_id;
        Ok(())
    }

    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<engram_core::SandboxId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.sandbox_id = sandbox_id;
        if sandbox_id.is_none() && self.live_disk_manifests.lock().remove(&id).is_some() {
            self.chunk_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    async fn update_live_disk_manifest(
        &self,
        session_id: SessionId,
        sandbox_id: engram_core::SandboxId,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) -> Result<engram_core::traits::UpdateOutcome, MetaError> {
        let bound = self
            .sessions
            .lock()
            .get(&session_id)
            .and_then(|s| s.sandbox_id);
        if bound != Some(sandbox_id) {
            return Ok(engram_core::traits::UpdateOutcome::DroppedStale);
        }
        self.live_disk_manifests
            .lock()
            .insert(session_id, manifest_ref);
        self.chunk_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(engram_core::traits::UpdateOutcome::Applied)
    }

    async fn chunk_generation(&self) -> Result<u64, MetaError> {
        Ok(self
            .chunk_generation
            .load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError> {
        self.hosts.lock().insert(host.id, host);
        Ok(())
    }

    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        let mut rows: Vec<HostRecord> = self.hosts.lock().values().cloned().collect();
        rows.sort_by_key(|h| h.id);
        Ok(rows)
    }

    async fn set_host_status(&self, _id: HostId, _status: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }

    async fn touch_host_heartbeat(
        &self,
        _: HostId,
        _: engram_core::types::host::HostHeartbeat,
    ) -> Result<(), MetaError> {
        Ok(())
    }

    async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError> {
        match self.hosts.lock().get_mut(&id) {
            Some(h) => {
                h.cordoned = cordoned;
                Ok(())
            }
            None => Err(MetaError::NotFound),
        }
    }

    async fn list_stale_hosts(&self, _threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }

    async fn mark_host_dead_and_orphan_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        let mut g = self.sessions.lock();
        let mut affected = Vec::new();
        for s in g.values_mut() {
            if s.host_id == Some(host_id) && !s.status.is_terminal() {
                let prev = s.status;
                s.host_id = None;
                s.sandbox_id = None;
                s.status = SessionState::HostLost;
                s.last_active_at = Utc::now();
                affected.push((s.id, prev));
            }
        }
        Ok(affected)
    }

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
        self.snapshots_by_id.lock().insert(snap.id, snap.clone());
        if let Some(sid) = snap.session_id {
            let mut by_session = self.snapshots.lock();
            let rows = by_session.entry(sid).or_default();
            if let Some(existing) = rows.iter_mut().find(|r| r.id == snap.id) {
                *existing = snap;
            } else {
                rows.push(snap);
            }
        }
        Ok(())
    }

    async fn get_snapshot(
        &self,
        id: engram_core::types::SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self.snapshots_by_id.lock().get(&id).cloned())
    }

    async fn try_acquire_session_lease(
        &self,
        _session_id: SessionId,
        _sandbox_id: Option<engram_core::SandboxId>,
        _locked_by: &str,
    ) -> Result<bool, MetaError> {
        Ok(!self.lease_held.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn list_snapshots_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(self.snapshots.lock().get(&sid).cloned().unwrap_or_default())
    }

    async fn latest_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self
            .snapshots
            .lock()
            .get(&sid)
            .and_then(|v| v.last().cloned()))
    }

    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError> {
        if !self.sessions.lock().contains_key(&session_id) {
            return Err(MetaError::NotFound);
        }
        let mut counters = self.next_event_idx.lock();
        let counter = counters.entry(session_id).or_insert(0);
        let idx = *counter;
        *counter += 1;
        drop(counters);
        let event = PersistedEvent {
            idx,
            kind: kind.to_string(),
            payload,
            created_at: Utc::now(),
            recovery_epoch: 0,
            rewound_at: None,
        };
        self.events
            .lock()
            .entry(session_id)
            .or_default()
            .push(event);
        Ok(idx)
    }

    async fn list_session_events_since(
        &self,
        session_id: SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(self
            .events
            .lock()
            .get(&session_id)
            .into_iter()
            .flat_map(|v| v.iter().filter(|e| e.idx > since).cloned())
            .take(limit.max(0) as usize)
            .collect())
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
        ei: engram_core::types::EnabledImage,
    ) -> Result<(), MetaError> {
        self.enabled.lock().insert(ei.image_uri.clone(), ei);
        Ok(())
    }

    async fn list_enabled_images(
        &self,
    ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
        Ok(self.enabled.lock().values().cloned().collect())
    }

    async fn get_enabled_image(
        &self,
        uri: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(self.enabled.lock().get(uri).cloned())
    }

    async fn get_enabled_image_any(
        &self,
        uri: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(self.enabled.lock().get(uri).cloned())
    }

    async fn soft_delete_enabled_image(
        &self,
        uri: &str,
    ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
        match self.enabled.lock().remove(uri) {
            Some(_) => Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled),
            None => Err(MetaError::NotFound),
        }
    }

    async fn delete_enabled_image(&self, uri: &str) -> Result<(), MetaError> {
        self.enabled.lock().remove(uri);
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

// ---------------------------------------------------------------------
// Fixture: a fully-wired AppState over in-memory components, mirroring
// tests/api.rs::build_app_with_tokens but with the app-gRPC bearer
// allow-list (`app_grpc_tokens`) set instead of the REST `auth_tokens`.
// `app_grpc_tokens` empty == fail closed.
// ---------------------------------------------------------------------

/// Token the test server is configured with — the happy-path credential.
const TEST_TOKEN: &str = "test-app-grpc-token";

fn test_state(app_grpc_tokens: Vec<String>) -> (Arc<AppState>, Arc<MockMetadataStore>) {
    let meta = MockMetadataStore::arc();
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let blob = || {
        Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        ))
    };
    let services = Services {
        meta: meta.clone(),
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
        default_image_version: "warm-bootstrap".into(),
        app_grpc_tokens,
        ..CoordinatorConfig::default()
    };
    (Arc::new(AppState::new(cfg, services)), meta)
}

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
async fn dial(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC")
}

/// tonic interceptor that stamps `Authorization: Bearer <token>` onto
/// every outbound request.
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

/// Minimal `EnabledImage` row for the ImageService list assertion. An
/// empty `manifest_toml` parses to a `None` manifest and falls back to
/// rendering the `image_uri` only (see `EnabledImageSummary::from`).
fn enabled_image(uri: &str) -> engram_core::types::EnabledImage {
    let now = Utc::now();
    engram_core::types::EnabledImage {
        id: uuid::Uuid::new_v4(),
        image_uri: uri.to_string(),
        manifest_toml: String::new(),
        manifest_digest: "sha256:0000".to_string(),
        disk_manifest: None,
        base_snapshot_id: None,
        base_snapshot_disk_manifest: None,
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        soft_deleted_at: None,
    }
}

// =====================================================================
// Auth (ADR 0039 §5) — replaces tests/api.rs auth_* REST middleware tests
// =====================================================================

/// Happy path: a server configured with `TEST_TOKEN`, called with a
/// matching bearer, passes auth on every service. Proves each of the
/// four services is mounted and answering against the in-memory state.
#[tokio::test]
async fn app_grpc_valid_bearer_reaches_every_service() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;

    // SessionService.ListSessions — empty against fresh state.
    let resp = app::session_service_client::SessionServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect("ListSessions must succeed with a valid bearer");
    assert!(
        resp.into_inner().sessions.is_empty(),
        "fresh state has no sessions"
    );

    // FleetService.ListHosts — empty.
    let resp = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_hosts(app::ListHostsRequest::default())
    .await
    .expect("FleetService ListHosts must succeed");
    assert!(resp.into_inner().hosts.is_empty(), "no hosts");

    // ImageService.ListEnabledImages — empty.
    let resp = app::image_service_client::ImageServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_enabled_images(app::ListEnabledImagesRequest::default())
    .await
    .expect("ImageService ListEnabledImages must succeed");
    assert!(resp.into_inner().images.is_empty(), "no images");

    // ShellRelayService.Relay — an empty inbound stream returns
    // InvalidArgument (no open frame), proving the service is mounted and
    // the auth check passed (we'd see Unauthenticated otherwise).
    let outbound = tokio_stream::iter(Vec::<app::RelayShellRequest>::new());
    let err = app::shell_relay_service_client::ShellRelayServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .relay(outbound)
    .await
    .expect_err("Relay: empty stream must return InvalidArgument (no open frame)");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    server.abort();
}

/// On a token-configured server, a call with no bearer and a call with
/// the wrong bearer are both rejected `Unauthenticated`. Replaces the
/// REST `auth_rejects_*` middleware tests.
#[tokio::test]
async fn app_grpc_rejects_missing_and_wrong_bearer() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
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

/// Fail-closed: a server built with ZERO configured tokens rejects
/// everything — even a syntactically valid bearer. The deliberate
/// opposite of the retired axum surface's empty-list dev bypass.
#[tokio::test]
async fn app_grpc_fails_closed_with_no_tokens_configured() {
    let (state, _meta) = test_state(vec![]);
    let (addr, server) = serve(state).await;
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

// =====================================================================
// SessionService CRUD — replaces the REST get/list/delete + error-mapping
// tests in tests/api.rs (the create path needs a live sandbox/host and is
// covered by the gated grpc_smoke.rs against a real stack).
// =====================================================================

/// Seed two sessions directly via the MetadataStore (the create RPC needs
/// a live sandbox), then drive GetSession / ListSessions / DeleteSession
/// over gRPC and assert the round-trips. Replaces REST
/// `get_session_*` / `list_sessions_*` / `delete_session_*`.
#[tokio::test]
async fn session_get_list_delete_round_trip() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);

    // Seed a live (Active) session and a terminal one so ListSessions
    // (live-only) filtering is exercised.
    let live_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed live session");
    // Drive it to Active so list_active_sessions returns it.
    for target in [SessionState::Created, SessionState::Active] {
        meta.transition_session(live_id, target)
            .await
            .expect("transition to active");
    }

    let dead_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed dead session");
    for target in [
        SessionState::Created,
        SessionState::Active,
        SessionState::Completed,
    ] {
        meta.transition_session(dead_id, target)
            .await
            .expect("transition to completed");
    }

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // ---- GetSession on the live id ----
    let got = client
        .get_session(app::GetSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("GetSession must succeed")
        .into_inner()
        .session
        .expect("session must be present");
    assert_eq!(got.id, live_id.to_string(), "GetSession id mismatch");
    assert_eq!(got.image, "localhost:5001/demo:warm", "image mismatch");

    // ---- ListSessions returns only the live row ----
    let list = client
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions must succeed")
        .into_inner();
    let ids: Vec<String> = list
        .sessions
        .iter()
        .filter_map(|item| item.session.as_ref().map(|s| s.id.clone()))
        .collect();
    assert!(
        ids.contains(&live_id.to_string()),
        "live session must be listed; got {ids:?}"
    );
    assert!(
        !ids.contains(&dead_id.to_string()),
        "completed session must NOT be in the live list; got {ids:?}"
    );

    // ---- DeleteSession transitions to terminal (row persists) ----
    client
        .delete_session(app::DeleteSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("DeleteSession must succeed");

    // The row still exists, now terminal, and no longer in the live list.
    let post = client
        .get_session(app::GetSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("GetSession after delete must still return the row")
        .into_inner()
        .session
        .expect("row persists after delete");
    assert!(
        matches!(
            post.status.as_str(),
            "completed" | "failed" | "dead" | "host_lost"
        ),
        "deleted session must be terminal, got {:?}",
        post.status
    );

    server.abort();
}

/// GetSession with a malformed (non-UUID) session id maps to
/// InvalidArgument; an unknown-but-valid UUID maps to NotFound. Replaces
/// REST `get_session_returns_400_for_malformed_id` /
/// `get_session_returns_404_for_unknown_id`.
#[tokio::test]
async fn session_get_bad_uuid_and_not_found_mapping() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let err = client
        .get_session(app::GetSessionRequest {
            session_id: "not-a-uuid".to_string(),
        })
        .await
        .expect_err("malformed id must error");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "malformed session id → InvalidArgument: {err:?}"
    );

    let unknown = SessionId::new();
    let err = client
        .get_session(app::GetSessionRequest {
            session_id: unknown.to_string(),
        })
        .await
        .expect_err("unknown id must error");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "unknown session id → NotFound: {err:?}"
    );

    server.abort();
}

/// CreateSession against an image that is not enabled returns
/// InvalidArgument (the image gate runs before any sandbox work).
/// Replaces REST `create_session_with_unknown_image_returns_400`.
#[tokio::test]
async fn create_session_unknown_image_is_invalid_argument() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let err = client
        .create_session(app::CreateSessionRequest {
            selected_skills: Vec::new(),
            image_uri: "localhost:5001/never-enabled:warm".into(),
            mode: "agent".into(),
            prompt: None,
            harness_env: HashMap::new(),
            secrets: HashMap::new(),
            prompt_id: None,
        })
        .await
        .expect_err("non-enabled image must error");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "non-enabled image → InvalidArgument: {err:?}"
    );

    server.abort();
}

// =====================================================================
// FleetService — replaces REST storage_summary + (implicit) hosts list.
// =====================================================================

/// FleetService.ListHosts (empty) and GetStorageSummary (zeros) on a
/// fresh fleet. Replaces REST `storage_summary_zeros_on_empty_fleet`.
#[tokio::test]
async fn fleet_list_hosts_and_storage_summary() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let hosts = client
        .list_hosts(app::ListHostsRequest::default())
        .await
        .expect("ListHosts must succeed")
        .into_inner();
    assert!(hosts.hosts.is_empty(), "fresh fleet: no hosts");

    let summary = client
        .get_storage_summary(app::GetStorageSummaryRequest::default())
        .await
        .expect("GetStorageSummary must succeed")
        .into_inner();
    assert_eq!(summary.tracked_sandboxes, 0, "empty fleet: no sandboxes");
    assert_eq!(summary.snapshots, 0, "empty fleet: no snapshots");
    assert_eq!(summary.dirty_chunks, 0, "empty fleet: no dirty chunks");

    server.abort();
}

// =====================================================================
// ImageService — replaces REST enabled-images list.
// =====================================================================

/// ImageService.ListEnabledImages reflects an upserted enabled image.
/// Replaces REST list-enabled-images coverage.
#[tokio::test]
async fn image_list_enabled_images_reflects_store() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);

    // Empty to start.
    {
        let (addr, server) = serve(state.clone()).await;
        let channel = dial(addr).await;
        let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
            channel,
            bearer(TEST_TOKEN),
        );
        let resp = client
            .list_enabled_images(app::ListEnabledImagesRequest::default())
            .await
            .expect("ListEnabledImages must succeed")
            .into_inner();
        assert!(resp.images.is_empty(), "no images enabled yet");
        server.abort();
    }

    // Seed an enabled image and assert it surfaces over gRPC.
    meta.upsert_enabled_image(enabled_image("localhost:5001/demo:warm"))
        .await
        .expect("seed enabled image");

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );
    let resp = client
        .list_enabled_images(app::ListEnabledImagesRequest::default())
        .await
        .expect("ListEnabledImages must succeed")
        .into_inner();
    let uris: Vec<&str> = resp.images.iter().map(|i| i.image_uri.as_str()).collect();
    assert!(
        uris.contains(&"localhost:5001/demo:warm"),
        "enabled image must be listed; got {uris:?}"
    );

    server.abort();
}
