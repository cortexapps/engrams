//! Integration test for the coordinator surface.
//!
//! ADR 0051 made the coordinator gRPC-only: the web-facing axum REST
//! routes (`/sessions`, `/storage/*`, `/registries/*`, `/enabled-images`,
//! the admin GC/flush surface, …) were deleted. Their logic now lives in
//! `_core` fns called by the app-gRPC services in `grpc_app/*`.
//!
//! This file therefore exercises the surface two ways:
//!  - the SURVIVING axum routes (healthz, the internal host→coord
//!    ingestion routes under `/hosts/*` + `/sessions/:id/harness-events`,
//!    the in-guest forge seam, the bearer gate on the internal router)
//!    via `tower::ServiceExt::oneshot` against `api::router`, exactly as
//!    before; and
//!  - the MIGRATED session/exec/snapshot/events/storage behaviour via the
//!    real app-gRPC server (`grpc_app::server`) driven by typed tonic
//!    clients — the SAME `_core` fns, asserting `tonic::Code` in place of
//!    the old HTTP status (the `into_status` mapping in `grpc_app/mod.rs`
//!    is the reference: NotFound↔404, InvalidArgument↔400,
//!    FailedPrecondition↔409/410, Unavailable↔503).
//!
//! Both surfaces share one `MockMetadataStore` + `TestFixture`, so the
//! gRPC tests reuse the same image/host/secret seeding the axum tests
//! always used.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{api, grpc_app, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SessionId};
use engram_protocol::app;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use futures::StreamExt as _;
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tonic::Code;
use tower::ServiceExt;

// ---------------------------------------------------------------------
// MockMetadataStore — in-memory MetadataStore for end-to-end API tests.
// Behaviour intentionally mirrors the Postgres impl's contracts (NotFound
// when a row is missing, success on upsert, etc.).
// ---------------------------------------------------------------------

#[derive(Default)]
struct MockMetadataStore {
    sessions: Mutex<HashMap<SessionId, Session>>,
    snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
    /// ADR 0020: snapshots keyed by id, for `get_snapshot` — covers both
    /// session captures and template/base snapshots (session_id = None),
    /// which the per-session `snapshots` map can't hold.
    snapshots_by_id: Mutex<HashMap<engram_core::types::SnapshotId, SnapshotRecord>>,
    enabled: Mutex<HashMap<String, engram_core::types::EnabledImage>>,
    events: Mutex<HashMap<SessionId, Vec<PersistedEvent>>>,
    next_event_idx: Mutex<HashMap<SessionId, i64>>,
    /// ADR 0016 Phase B: track `update_live_disk_manifest` writes so
    /// the round-trip test can assert the row was updated.
    live_disk_manifests: Mutex<HashMap<SessionId, engram_core::types::manifest::ManifestRef>>,
    /// ADR 0016 Phase C: in-memory mirror of `chunk_generation` so
    /// tests can assert the barrier ticked atomically with the
    /// session-row write.
    chunk_generation: std::sync::atomic::AtomicU64,
    /// ADR 0047: placement reads host rows now — the fixture seeds the
    /// test host here and `mark_host_ready_for` mutates its
    /// `ready_images`.
    hosts: Mutex<HashMap<HostId, HostRecord>>,
}

impl MockMetadataStore {
    fn new() -> Self {
        Self::default()
    }

    fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Test-only accessor: snapshot all session rows currently in the
    /// store. Used to assert post-conditions about failed sessions.
    fn all_sessions(&self) -> Vec<Session> {
        self.sessions.lock().values().cloned().collect()
    }
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        let id = SessionId::new();
        let session = Session {
            id,
            user_id: spec.user_id,
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
            user_id: spec.user_id,
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
        // ADR 0016 Phase B: clear live manifest + bump generation on
        // unbind, matching the PgMeta semantics.
        if sandbox_id.is_none() && self.live_disk_manifests.lock().remove(&id).is_some() {
            self.chunk_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    // ADR 0016 Phase B: trait extension. Matches the PG / MiniMeta
    // sandbox_id-guard semantics so the round-trip test verifies
    // the handler routes Applied vs DroppedStale correctly.
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
        // Mock doesn't track heartbeat timestamps; existing tests
        // don't exercise the dead-host detector path.
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
        // Keyed-by-id mirror covers template snapshots (session_id=None)
        // that the per-session map can't hold; `get_snapshot` reads it.
        self.snapshots_by_id.lock().insert(snap.id, snap.clone());
        if let Some(sid) = snap.session_id {
            self.snapshots.lock().entry(sid).or_default().push(snap);
        }
        Ok(())
    }

    async fn get_snapshot(
        &self,
        id: engram_core::types::SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self.snapshots_by_id.lock().get(&id).cloned())
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
        // Mirror Postgres semantics: refuse to log against a session
        // we've never seen, allocate a monotonic per-session idx.
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
    // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
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
        // Mock doesn't model soft-delete state separately — same
        // data as get_enabled_image. Real PG impl returns rows
        // regardless of soft_deleted_at; tests that need to
        // exercise that distinction should construct an
        // EnabledImage with soft_deleted_at = Some(...) and verify
        // their consumer's branch directly.
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
// Fixture: build a fully-wired axum router against in-memory components.
// ---------------------------------------------------------------------

fn build_app(meta: Arc<MockMetadataStore>) -> axum::Router {
    TestFixture::new(meta, InMemorySecretStore::new()).app
}

// ---------------------------------------------------------------------
// app-gRPC harness (ADR 0051). The web-facing session/exec/snapshot/
// events/storage REST routes were deleted; their `_core` fns are now
// reachable only through the app-gRPC services. These helpers stand up
// the real `grpc_app::server` over a `TestFixture`'s `AppState` and dial
// it with typed tonic clients — the established pattern from
// `tests/grpc_app.rs`.
// ---------------------------------------------------------------------

/// Token the gRPC test server is configured with (see
/// `TestFixture::new`, which seeds `app_grpc_tokens`).
const TEST_TOKEN: &str = "test-app-grpc-token";

/// A live app-gRPC server + a connected channel. Drop / `abort()` the
/// handle to tear the server down.
struct GrpcHarness {
    channel: tonic::transport::Channel,
    handle: tokio::task::JoinHandle<()>,
}

impl GrpcHarness {
    fn session(
        &self,
    ) -> app::session_service_client::SessionServiceClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl tonic::service::Interceptor + Clone,
        >,
    > {
        app::session_service_client::SessionServiceClient::with_interceptor(
            self.channel.clone(),
            bearer(TEST_TOKEN),
        )
    }

    fn fleet(
        &self,
    ) -> app::fleet_service_client::FleetServiceClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl tonic::service::Interceptor + Clone,
        >,
    > {
        app::fleet_service_client::FleetServiceClient::with_interceptor(
            self.channel.clone(),
            bearer(TEST_TOKEN),
        )
    }
}

impl Drop for GrpcHarness {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bring up `grpc_app::server(state)` on an ephemeral port and dial it.
async fn serve_grpc(state: Arc<AppState>) -> GrpcHarness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming from listener");
    let handle = tokio::spawn(async move {
        let _ = grpc_app::server(state).serve_with_incoming(incoming).await;
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC");
    GrpcHarness { channel, handle }
}

/// tonic interceptor stamping `Authorization: Bearer <token>` on every
/// outbound request (mirrors `tests/grpc_app.rs::bearer`).
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

/// Convenience: stand up a gRPC harness over a default `TestFixture`
/// (baseline images `r`, `warm/test`, `cortex/api` already seeded) +
/// the given store. Returns the harness; the fixture's `AppState` is
/// kept alive inside the spawned server.
async fn grpc_fixture(meta: Arc<MockMetadataStore>) -> GrpcHarness {
    let f = TestFixture::new(meta, InMemorySecretStore::new());
    serve_grpc(f.state).await
}

/// Create a session over gRPC against image `{repo}:warm-bootstrap`
/// (which the default fixture seeds) and return its id. The gRPC analog
/// of the old `api_create_session` helper.
async fn grpc_create_session(h: &GrpcHarness, repo: &str) -> SessionId {
    grpc_create_image(h, &format!("{repo}:warm-bootstrap")).await
}

/// Create an agent-mode session over gRPC against a full `image_uri`
/// (used by the manifest/secret tests, whose images carry a non-default
/// tag the fixture seeds via `write_image`).
async fn grpc_create_image(h: &GrpcHarness, image_uri: &str) -> SessionId {
    let resp = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: image_uri.to_string(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect("CreateSession must succeed in the test fixture")
        .into_inner();
    resp.session_id.parse().expect("session_id is a uuid")
}

/// Drive `Exec` to completion, collecting the streamed frames into
/// `(stdout, stderr, exit_status, wall_ms)`. The gRPC analog of the old
/// unary `/exec` round-trip.
async fn grpc_exec(
    h: &GrpcHarness,
    id: SessionId,
    req: app::ExecRequest,
) -> Result<(String, String, i32, u64), tonic::Status> {
    let stream = h
        .session()
        .exec(req_with_session(req, id))
        .await?
        .into_inner();
    let frames: Vec<app::ExecOutput> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_status: i32 = 0;
    let mut wall_ms = 0;
    for f in frames {
        match f.event {
            Some(app::exec_output::Event::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(app::exec_output::Event::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(app::exec_output::Event::Exit(e)) => {
                exit_status = e.exit_status.unwrap_or(0);
                wall_ms = e.rusage.map(|r| r.wall_ms).unwrap_or(0);
            }
            Some(app::exec_output::Event::Started(_)) | None => {}
        }
    }
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        exit_status,
        wall_ms,
    ))
}

/// Drive `Exec` and return the `Status` it fails with. The error can
/// surface either at stream-open or on the first poll (`exec_stream_core`
/// validates the session/sandbox before producing the body), so this
/// helper checks both.
async fn grpc_exec_err(h: &GrpcHarness, id: SessionId, req: app::ExecRequest) -> tonic::Status {
    match h.session().exec(req_with_session(req, id)).await {
        Err(status) => status,
        Ok(resp) => {
            let mut stream = resp.into_inner();
            loop {
                match stream.next().await {
                    Some(Err(status)) => return status,
                    Some(Ok(_)) => continue,
                    None => panic!("exec stream ended without the expected error"),
                }
            }
        }
    }
}

/// An `ExecRequest` carrying just a shell `command`, addressed to `id`.
fn exec_command(command: &str) -> app::ExecRequest {
    app::ExecRequest {
        session_id: String::new(),
        command: Some(command.to_string()),
        argv: vec![],
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
    }
}

/// Open `StreamEvents` for `id` (proto `since`: `None` = from the start /
/// "all"). Returns the live stream so the caller can drain it while
/// concurrently triggering actions, or asserts the open error.
async fn open_events(
    h: &GrpcHarness,
    id: SessionId,
    since: Option<i64>,
) -> Result<tonic::Streaming<app::SessionEvent>, tonic::Status> {
    h.session()
        .stream_events(app::StreamEventsRequest {
            session_id: id.to_string(),
            since,
        })
        .await
        .map(|r| r.into_inner())
}

/// Drain a `SessionEvent` stream for up to `budget`, returning the
/// collected events. Stops early if the stream ends.
async fn collect_events(
    mut stream: tonic::Streaming<app::SessionEvent>,
    budget: Duration,
) -> Vec<app::SessionEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(ev))) => events.push(ev),
            // End-of-stream, error, or budget elapsed → done.
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    events
}

/// Snapshot `id` over gRPC (the old `POST /sessions/:id/snapshot`).
async fn grpc_snapshot(
    h: &GrpcHarness,
    id: SessionId,
) -> Result<app::SnapshotResponse, tonic::Status> {
    h.session()
        .snapshot(app::SnapshotRequest {
            session_id: id.to_string(),
        })
        .await
        .map(|r| r.into_inner())
}

/// Evict `id` locally over gRPC (the old `DELETE /sessions/:id/local`).
async fn grpc_evict_local(h: &GrpcHarness, id: SessionId) -> Result<(), tonic::Status> {
    h.session()
        .evict_local(app::EvictLocalRequest {
            session_id: id.to_string(),
        })
        .await
        .map(|_| ())
}

/// Resume `id` over gRPC (the old `POST /sessions/:id/resume`).
async fn grpc_resume(h: &GrpcHarness, id: SessionId) -> Result<app::ResumeResponse, tonic::Status> {
    h.session()
        .resume(app::ResumeRequest {
            session_id: id.to_string(),
        })
        .await
        .map(|r| r.into_inner())
}

/// Stamp the session id onto an `ExecRequest` (the routing field).
fn req_with_session(mut req: app::ExecRequest, id: SessionId) -> app::ExecRequest {
    req.session_id = id.to_string();
    req
}

/// Like `build_app` but seeds the bearer-token allow-list. Used by the
/// auth middleware tests; everything else relies on the default empty
/// list (auth-disabled).
fn build_app_with_tokens(meta: Arc<MockMetadataStore>, tokens: Vec<String>) -> axum::Router {
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
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
        default_image_version: "warm-bootstrap".into(),
        auth_tokens: tokens,
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new(cfg, services));
    api::router(state)
}

/// ADR 0023: build a router with a configured (mock) git forge + a
/// seeded session whose credential-broker token is `broker-tok-123`.
/// Returns the router, the session id, and the forge handle (so tests
/// can assert recorded change requests).
async fn build_forge_app() -> (axum::Router, SessionId, Arc<engram_git_dev::StaticGitForge>) {
    let meta = Arc::new(MockMetadataStore::new());
    let session_id = meta
        .create_session(engram_core::types::session::SessionSpec {
            image: "cortexapps/engrams:warm-bootstrap".to_string(),
            mode: Default::default(),
            user_id: None,
        })
        .await
        .expect("seed session");
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let forge = Arc::new(engram_git_dev::StaticGitForge::github("ghs_test_xyz"));
    let services = Services {
        meta,
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
        blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        )),
        chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
            engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            ),
        )),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        ..CoordinatorConfig::default()
    };
    let mut app = AppState::new(cfg, services);
    app.forge = Some(forge.clone());
    let state = Arc::new(app);
    state
        .git_broker_tokens
        .insert(session_id, "broker-tok-123".to_string());
    (api::router(state), session_id, forge)
}

#[tokio::test]
async fn forge_git_credential_gated_by_broker_token() {
    let (app, sid, _forge) = build_forge_app().await;
    let uri = format!("/api/v1/sessions/{sid}/git-credential");

    // No token → 401.
    let resp = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let resp = app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header("authorization", "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Valid broker token → 200 + the minted credential.
    let resp = app
        .oneshot(
            Request::get(&uri)
                .header("authorization", "Bearer broker-tok-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["username"], "x-access-token");
    assert_eq!(v["password"], "ghs_test_xyz");
}

#[tokio::test]
async fn forge_create_pull_request_opens_and_records() {
    let (app, sid, forge) = build_forge_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/sessions/{sid}/pull-request"))
                .header("content-type", "application/json")
                .header("authorization", "Bearer broker-tok-123")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "repo": "cortexapps/engrams",
                        "head_branch": "feat/x",
                        "base_branch": "main",
                        "title": "Add x",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["url"].as_str().unwrap().contains("cortexapps/engrams"),
        "unexpected PR url: {v:?}"
    );

    let recorded = forge.recorded_pull_requests();
    assert_eq!(recorded.len(), 1, "forge should have recorded one PR");
    assert_eq!(recorded[0].0.to_string(), "cortexapps/engrams");
    assert_eq!(recorded[0].1.title, "Add x");
}

#[tokio::test]
async fn forge_forward_runs_the_core_for_split_hosts() {
    // ADR 0023 split-mode forwarding: an FC host can't run the forge sink
    // locally, so it POSTs the in-guest ForgeRequest (with its broker
    // token) to /api/hosts/forge and gets back the same ForgeResponse the
    // vsock path would produce.
    let (app, sid, _forge) = build_forge_app().await;

    // Valid broker token in the body → a minted credential.
    let req = engram_harness_proto::ForgeRequest {
        session_id: sid,
        broker_token: "broker-tok-123".to_string(),
        op: engram_harness_proto::ForgeOp::FetchCredential {
            host: "github.com".to_string(),
            owner: None,
        },
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/hosts/forge")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["Credential"]["password"]
            .as_str()
            .is_some_and(|p| !p.is_empty()),
        "expected a minted credential, got {v:?}",
    );

    // Wrong broker token → ForgeResponse::Error (200, error in the body),
    // never a minted credential — the body token is still validated.
    let bad = engram_harness_proto::ForgeRequest {
        session_id: sid,
        broker_token: "wrong".to_string(),
        op: engram_harness_proto::ForgeOp::FetchCredential {
            host: "github.com".to_string(),
            owner: None,
        },
    };
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/hosts/forge")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&bad).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v.get("Error").is_some(),
        "bad token must yield Error, got {v:?}"
    );
}

/// Test fixture exposing the meta store so individual tests can
/// populate images / secrets before exercising the API. The default
/// `build_app` discards the handle (most tests don't care).
struct TestFixture {
    app: axum::Router,
    /// ADR 0051: the wired `AppState`, kept so gRPC-migrated tests can
    /// stand up the real app-gRPC server (`grpc_app::server`) over the
    /// same store/host/secret wiring the axum surface uses.
    state: Arc<AppState>,
    meta: Arc<MockMetadataStore>,
    /// ADR 0015 M5: pinned to the single in-process host so test
    /// helpers can flip its `ready_images` set in lockstep with
    /// `seed_enabled` calls. Production hosts populate this from
    /// the prefetch supervisor + heartbeat, but the in-test ProcessBackend
    /// has no chunks to prefetch — we just declare it ready.
    test_host_id: engram_core::HostId,
}

impl TestFixture {
    fn new(meta: Arc<MockMetadataStore>, secrets: InMemorySecretStore) -> Self {
        // Separate tempdirs for each on-disk component so they can't
        // accidentally collide. All leak (`keep()`) because the axum
        // router needs to outlive this function — the OS cleans up
        // `/tmp` later.
        let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
        // Match production wiring: the host-side PooledBackend wraps
        // the real backend so the chunked-OCI / image-cache / egress
        // helpers fire the same way the multi-host setup runs them.
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_dir));
        let backend: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_host_agent::pooled_backend::PooledBackend::new(raw));
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(backend)),
            secrets: Arc::new(secrets),
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
            default_image_version: "warm-bootstrap".into(),
            // ADR 0051: the gRPC-migrated tests dial `grpc_app::server`
            // with `TEST_TOKEN`; seed the allow-list so the BearerAuth
            // gate (fail-closed when empty) lets them through.
            app_grpc_tokens: vec![TEST_TOKEN.to_string()],
            ..CoordinatorConfig::default()
        };
        // ADR 0015 M5: build the registry explicitly so we can keep
        // a handle to it and pin the host id we use to seed
        // `ready_images` from `write_image`.
        let host_registry = Arc::new(engram_coordinator::HostRegistry::new(meta.clone()));
        let test_host_id = engram_core::HostId::new();
        host_registry.register(test_host_id, services.host.clone());
        // ADR 0047: placement reads host rows — seed a fresh, ready,
        // schedulable row for the test host.
        meta.hosts.lock().insert(
            test_host_id,
            engram_core::types::host::HostRecord {
                id: test_host_id,
                hostname: "api-test-host".into(),
                cloud_metadata: Default::default(),
                capacity: engram_core::types::HostCapacity {
                    total_gb: 0,
                    used_gb: 0,
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: None,
                ready_images: Vec::new(),
                local_snapshots: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
            },
        );
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        let fx = Self {
            app: api::router(state.clone()),
            state,
            meta: meta.clone(),
            test_host_id,
        };
        // Seed baseline images for the repos most tests use against
        // `api_create_session`. Stage B1 made `image` resolve via
        // `enabled_images`; every test that doesn't explicitly enable
        // its own image needs one of these in the mock store to clear
        // the lookup gate.
        for repo in ["r", "warm/test", "cortex/api"] {
            fx.write_image(repo, "warm-bootstrap", r#"name = "baseline""#);
        }
        fx
    }

    /// Seed an `enabled_images` row in the mock store. The `rootfs`
    /// parameter is gone post-Stage-E — sandbox content flows from
    /// the registry through the host-agent's cache, not from the
    /// coordinator's filesystem. Tests retain `write_image` for
    /// continuity; the body is a thin wrapper over `seed_enabled`.
    ///
    /// ADR 0015 M5: also marks the test host ready for the new
    /// image's digest so the readiness gate in `pick_for_session`
    /// lets the next session create through. Production hosts
    /// populate this set via the prefetch supervisor; tests have
    /// no chunks to fault so we just declare the host ready.
    fn write_image(&self, repo: &str, tag: &str, manifest_toml: &str) {
        let uri = format!("{repo}:{tag}");
        let digest = seed_enabled(&self.meta, &uri, manifest_toml);
        self.mark_host_ready_for(digest);
    }

    fn mark_host_ready_for(&self, digest: engram_protocol::heartbeat::ManifestDigest) {
        // ADR 0047: readiness lives on the host ROW now.
        let mut hosts = self.meta.hosts.lock();
        let row = hosts
            .get_mut(&self.test_host_id)
            .expect("fixture seeds the test host row");
        let d = digest.as_str().to_string();
        if !row.ready_images.contains(&d) {
            row.ready_images.push(d);
        }
    }
}

/// Seed an `enabled_images` row directly into the mock store. Tests
/// that don't go through `TestFixture::write_image` (e.g. those wiring
/// a custom backend) can use this to clear the strict image-resolution
/// gate without spinning up the full fixture.
fn seed_enabled(
    store: &MockMetadataStore,
    uri: &str,
    manifest_toml: &str,
) -> engram_protocol::heartbeat::ManifestDigest {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut h);
    let digest = format!("sha256:{:08x}", h.finish());
    let now = chrono::Utc::now();
    // ADR 0020: every enabled image has a base snapshot. Seed a template
    // snapshot (session_id = None) and point the row at it, so the
    // create→restore path resolves it via `get_snapshot`. ProcessBackend
    // restore of a base snapshot with no captured artifact boots a fresh
    // sandbox (see ProcessBackend::restore).
    let base_snapshot_id = engram_core::types::SnapshotId::new();
    store.snapshots_by_id.lock().insert(
        base_snapshot_id,
        SnapshotRecord {
            id: base_snapshot_id,
            session_id: None,
            host_id: None,
            image_version: uri.to_string(),
            size_bytes: 0,
            created_at: now,
            last_accessed_at: now,
            disk_manifest: None,
            memory_manifest: None,
            recoverable: true,
            aux_bundles: vec![],
            events_cursor: None,
        },
    );
    store.enabled.lock().insert(
        uri.to_string(),
        engram_core::types::EnabledImage {
            id: uuid::Uuid::new_v4(),
            image_uri: uri.to_string(),
            manifest_toml: manifest_toml.to_string(),
            manifest_digest: digest.clone(),
            disk_manifest: None,
            base_snapshot_id: Some(base_snapshot_id),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            last_refreshed_at: now,
            created_at: now,
            updated_at: None,
            soft_deleted_at: None,
        },
    );
    engram_protocol::heartbeat::ManifestDigest::new(&digest)
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "response body was not valid JSON: {e}; body = {:?}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Walk the ADR 0015 M2 legality table from the session's current
/// state (whatever it is) to `target`. Tests in this file used to
/// call `set_session_status(id, X)` to force a row into a given
/// state; M2's `transition_session` validates every move, so tests
/// have to go through legal intermediates exactly the way prod does.
async fn seed_to(store: &MockMetadataStore, id: SessionId, target: SessionState) {
    use SessionState::*;
    let current = store.get_session(id).await.unwrap().status;
    let path: &[SessionState] = match (current, target) {
        // Already there — no-op.
        (a, b) if a == b => &[],
        // From Pending (just-created row).
        (Pending, Created) => &[Created],
        (Pending, GuestReady) => &[Created, GuestReady],
        (Pending, Active) => &[Created, Active],
        (Pending, Idle) => &[Created, Active, Idle],
        (Pending, HostLost) => &[Created, Active, HostLost],
        (Pending, Failed) => &[Failed],
        (Pending, Completed) => &[Created, Active, Completed],
        (Pending, Dead) => &[Created, Active, Dead],
        (Pending, Evicting) => &[Created, Active, Evicting],
        // From Active (post-create test that wants a later state).
        (Active, Idle) => &[Idle],
        (Active, HostLost) => &[HostLost],
        (Active, Completed) => &[Completed],
        (Active, Dead) => &[Dead],
        (Active, Failed) => &[Failed],
        (from, to) => panic!("seed_to: no path defined from {from:?} to {to:?}"),
    };
    for step in path {
        store.transition_session(id, *step).await.unwrap();
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Bearer-token middleware
// ---------------------------------------------------------------------

// ADR 0051: `auth_disabled_when_token_list_empty` and
// `human_routes_are_principal_authed_not_bearer_gated` targeted the
// deleted web-facing `GET /api/v1/sessions` route. The app-gRPC surface
// has no auth-disabled bypass and no principal layer — it is fail-closed
// (BearerAuth). Both removed assertions are now covered by
// `tests/grpc_app.rs::app_grpc_fails_closed_with_no_tokens_configured`
// (empty allow-list rejects everything) and
// `app_grpc_rejects_missing_and_wrong_bearer`.

// ADR 0031: the deployment bearer no longer gates *human* routes — those are
// authenticated by the principal layer (cookie / service-bearer / synthetic).
// The bearer now guards only the `internal` control-plane router (host →
// coord ingestion). These tests target an internal route (`/hosts/register`)
// to exercise that gate. A separate test below confirms human routes are
// principal-authed (synthetic admin → 200) regardless of the bearer.

const INTERNAL_ROUTE: &str = "/api/v1/hosts/register";

#[tokio::test]
async fn auth_rejects_internal_request_without_authorization_header() {
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(json_request(Method::POST, INTERNAL_ROUTE, json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn auth_rejects_wrong_token_on_internal_route() {
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Bearer beta")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert!(v["message"].as_str().unwrap().contains("invalid"));
}

#[tokio::test]
async fn auth_rejects_non_bearer_scheme_on_internal_route() {
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Basic YWxwaGE=")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_valid_bearer_passes_internal_gate() {
    // A valid token clears the bearer layer; the handler then runs (and may
    // reject the empty body) — the point is it is NOT 401.
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Bearer alpha")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "valid bearer must clear the internal gate"
    );
}

#[tokio::test]
async fn auth_lets_healthz_through_without_token() {
    // Liveness probes don't carry secrets — `/healthz` must stay
    // outside the auth layer even when auth is enabled.
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn healthz_returns_ok_status_and_version() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["status"], "ok");
    assert!(v["version"].is_string());
}

#[tokio::test]
async fn create_session_requires_image() {
    // ADR 0051 (gRPC): `image_uri` is mandatory. The old axum wire shape
    // surfaced a missing `image` as 422 (serde extractor); the proto
    // request defaults `image_uri` to "", which the core rejects as a
    // bad request → InvalidArgument (the gRPC analog of the 4xx).
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: String::new(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect_err("missing image must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn create_session_dev_vm_mode_skips_harness_on_harnessed_image() {
    // ADR 0021 P1.6: `mode = dev_vm` against a harnessed image leaves
    // the harness undriven — `resolve_harness` returns `None`, so
    // the backend's `start_agent` receives an empty-argv AgentSpec
    // (readiness probe; no spawn). Test asserts the session still
    // reaches Active (the dev VM is just a shell-only session of the
    // same image).
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store.clone(), InMemorySecretStore::new());
    // `exec` here would crash the test if it ran (no such binary), so
    // a clean Active proves dev-VM mode bypassed the spawn.
    f.write_image(
        "demo/dev-vm-from-harnessed",
        "v1",
        r#"
            name = "demo-dev-vm"
            [harness]
            name = "would-be-harness"
            exec = "/this/path/does/not/exist"
        "#,
    );
    let h = serve_grpc(f.state).await;

    let resp = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "demo/dev-vm-from-harnessed:v1".into(),
            mode: "dev_vm".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect("dev_vm create must succeed")
        .into_inner();
    assert_eq!(resp.status, "active");
    let id: SessionId = resp.session_id.parse().unwrap();
    let session = store.get_session(id).await.unwrap();
    assert_eq!(
        session.mode,
        engram_core::types::session::SessionMode::DevVm,
    );
}

#[tokio::test]
async fn dev_vm_exec_inherits_image_env() {
    // Regression for the dev-VM env gap: an agentless session's only
    // entry point is `/exec`, and dev-VM mode spawns no harness. The
    // durable session env (the image manifest `[env]` + secrets) is the
    // sandbox-wide env applied to every process: ProcessBackend (this
    // test) applies it to each exec from `SandboxSpec.env`, and on
    // Firecracker agentd holds it from the bind and applies it to
    // exec/shell/harness alike — so `engram exec` sees the image's
    // environment instead of a bare process env, with no per-request
    // re-injection by the coord.
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store.clone(), InMemorySecretStore::new());
    f.write_image(
        "demo/env-inject",
        "v1",
        r#"
            name = "env-inject"
            [env]
            ENGRAM_TEST_IMAGE_VAR = "from-manifest"
        "#,
    );
    let h = serve_grpc(f.state).await;

    let resp = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "demo/env-inject:v1".into(),
            mode: "dev_vm".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect("dev_vm create")
        .into_inner();
    let id: SessionId = resp.session_id.parse().unwrap();

    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printenv ENGRAM_TEST_IMAGE_VAR"))
        .await
        .expect("exec must succeed");
    assert_eq!(
        stdout.trim(),
        "from-manifest",
        "dev-VM exec should inherit the image manifest's [env]",
    );
}

#[tokio::test]
async fn create_session_with_explicit_image_persists_full_row() {
    // The "automatic image selection" paths (latest-ready, default
    // tag) were removed in phase 2 — every session declares an
    // explicit image. This test locks the new happy path: explicit
    // image lands, Empty workspace + None harness defaults work,
    // session transitions to Active.
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store.clone(), InMemorySecretStore::new());
    f.write_image("cortex/api", "warm-pinned", r#"name = "cortex-api""#);
    let h = serve_grpc(f.state).await;

    let resp = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "cortex/api:warm-pinned".into(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect("create must succeed")
        .into_inner();
    assert_eq!(resp.image_version, "warm-pinned");
    assert_eq!(resp.status, "active");
    let id: SessionId = resp.session_id.parse().unwrap();
    let session = store.get_session(id).await.unwrap();
    assert_eq!(session.image, "cortex/api:warm-pinned");
    // ADR 0021 P1.3: a default-mode session is `Agent` (the harness,
    // if any, comes from the image). DevVm-vs-Agent is the session
    // axis now, not None-vs-Builtin.
    assert_eq!(
        session.mode,
        engram_core::types::session::SessionMode::Agent
    );
    assert_eq!(session.status, SessionState::Active);
}

#[tokio::test]
async fn create_session_with_unknown_image_returns_invalid_argument() {
    // ADR 0051: unenabled image → BadRequest core → InvalidArgument
    // (the old 400). The message must still call out the unenabled image.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "never-baked:x".into(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect_err("unenabled image must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(
        err.message().contains("not enabled"),
        "error must call out the unenabled image: got {:?}",
        err.message(),
    );
}

#[tokio::test]
async fn create_session_prompt_with_dev_vm_mode_is_400() {
    // ADR 0021 P1.3: `prompt` requires an agent to receive it. A
    // dev-VM session leaves the image's baked harness undriven, so
    // there's nothing on the other end of `prompt` — reject with 400
    // rather than silently dropping the prompt.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "r:warm-bootstrap".into(),
            mode: "dev_vm".into(),
            prompt: Some("do the thing".into()),
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect_err("prompt + dev_vm must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

// ADR 0021 P1.3 deleted `create_session_unknown_harness_name_is_400`:
// the session no longer selects which harness to attach (that's an
// image-manifest property baked at image-bake time). The equivalent
// post-0021 failure mode is "the image has no [harness] block, so
// even with mode=Agent the session runs as a dev VM" — which is *not*
// an error condition, it's the harness-less template case. The image-
// manifest validation in engram-image-builder catches a malformed
// `[harness]` block at bake; there's no per-session "unknown harness"
// path anymore.

#[tokio::test]
async fn get_session_returns_not_found_for_unknown_id() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let unknown = SessionId::new();
    let err = h
        .session()
        .get_session(app::GetSessionRequest {
            session_id: unknown.to_string(),
        })
        .await
        .expect_err("unknown session must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    // The `engram-error-slug` metadata carries the slug (into_status).
    assert_eq!(
        err.metadata()
            .get("engram-error-slug")
            .map(|v| v.as_bytes()),
        Some("not_found".as_bytes()),
    );
}

#[tokio::test]
async fn get_session_returns_invalid_argument_for_malformed_id() {
    // ADR 0051: `parse_session_id` maps a malformed uuid to
    // InvalidArgument (the old axum Path-deser 400).
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = h
        .session()
        .get_session(app::GetSessionRequest {
            session_id: "not-a-uuid".into(),
        })
        .await
        .expect_err("malformed id must be InvalidArgument");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn list_sessions_returns_empty_array_when_store_is_empty() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let resp = h
        .session()
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions must succeed")
        .into_inner();
    assert!(resp.sessions.is_empty(), "empty store → empty list");
}

#[tokio::test]
async fn storage_summary_zeros_on_empty_fleet() {
    // ADR 0029: with no registered hosts and an empty metadata store,
    // the Storage rollup (now FleetService::GetStorageSummary) returns a
    // well-formed all-zero shape — never `NaN`/null — so the page renders
    // cleanly on a fresh deployment. The mock store's default
    // `snapshot_totals` / `count_gc_candidates` supply the zeros.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let s = h
        .fleet()
        .get_storage_summary(app::GetStorageSummaryRequest::default())
        .await
        .expect("GetStorageSummary must succeed")
        .into_inner();
    assert_eq!(s.snapshots, 0);
    assert_eq!(s.snapshot_bytes, 0);
    assert_eq!(s.gc_pending, 0);
    assert_eq!(s.tracked_sandboxes, 0);
    assert_eq!(s.dirty_chunks, 0);
    assert_eq!(s.unflushed_bytes, 0);
    assert_eq!(s.avg_locality_pct, 0);
    assert!(s.rows.is_empty(), "rows must be empty on a fresh fleet");
}

#[tokio::test]
async fn list_sessions_returns_live_rows_only() {
    // The endpoint mirrors `list_active_sessions` semantics
    // (`SessionState::is_live`): live rows — including mid-pipeline
    // states like `evicting` — show up; terminal rows are filtered
    // out. Filtering at the surface keeps the default
    // `engram session list` view focused on live work. `evicting`
    // is the regression case: it was missing from the live set, so
    // mid-eviction sessions vanished from the list (and, worse, from
    // startup routing rehydration — prod session 5cfb90b8).
    let store = MockMetadataStore::arc();

    async fn mk(store: &MockMetadataStore, repo: &str) -> SessionId {
        store
            .create_session(SessionSpec {
                image: format!("{repo}:warm-bootstrap"),
                mode: engram_core::types::session::SessionMode::Agent,
                user_id: None,
            })
            .await
            .unwrap()
    }
    let active_id = mk(&store, "alive").await;
    seed_to(&store, active_id, SessionState::Active).await;
    let idle_id = mk(&store, "idle-too").await;
    seed_to(&store, idle_id, SessionState::Idle).await;
    let evicting_id = mk(&store, "mid-evict").await;
    seed_to(&store, evicting_id, SessionState::Evicting).await;
    let dead_id = mk(&store, "done").await;
    seed_to(&store, dead_id, SessionState::Completed).await;

    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    // ADR 0051: the trusted-caller gRPC ListSessions returns ALL active
    // sessions (the orchestrator scopes by ownership). `list_active_sessions`
    // / `SessionState::is_live` does the live-vs-terminal filtering.
    let resp = h
        .session()
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions")
        .into_inner();
    let ids: Vec<String> = resp
        .sessions
        .iter()
        .map(|s| s.session.as_ref().unwrap().id.clone())
        .collect();
    assert!(ids.contains(&active_id.to_string()), "active row missing");
    assert!(ids.contains(&idle_id.to_string()), "idle row missing");
    assert!(
        ids.contains(&evicting_id.to_string()),
        "evicting row missing — mid-eviction sessions must stay listed",
    );
    assert!(
        !ids.contains(&dead_id.to_string()),
        "completed sessions must be filtered out",
    );
}

#[tokio::test]
async fn list_sessions_serializes_full_session_record() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "cortex/api:warm-2026-04-27".into(),
            // ADR 0021 P1.3: per-session HarnessSpec is gone — the
            // harness is an image property. `mode = dev_vm` exercises
            // the non-default arm of the wire shape.
            mode: engram_core::types::session::SessionMode::DevVm,
            user_id: Some("user-42".into()),
        })
        .await
        .unwrap();

    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    let resp = h
        .session()
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions")
        .into_inner();
    let item = resp
        .sessions
        .iter()
        .find(|s| s.session.as_ref().map(|x| x.id.as_str()) == Some(&id.to_string()))
        .expect("created session must be in the list");
    let session = item.session.as_ref().expect("session present");
    // Lock the proto wire shape so the orchestrator can rely on it.
    // ADR 0005: workspace gone; ADR 0021 P1.3: per-session harness gone —
    // the proto `Session` has no such fields (the converter's exhaustive
    // destructure is the compile-time guard). ADR 0051: `user_id` is
    // intentionally OFF the coord contract (`session_to_proto` drops it;
    // attribution lives in the orchestrator's task model), so it carries
    // on the owner_email/owner_name annotation slots instead — empty here
    // because the mock resolves no owners.
    assert_eq!(session.image, "cortex/api:warm-2026-04-27");
    assert_eq!(session.mode, "dev_vm");
    assert_eq!(session.status, SessionState::Pending.as_str());
    assert!(!session.created_at.is_empty());
    assert!(!session.last_active_at.is_empty());
    assert_eq!(item.owner_email, None);
    assert_eq!(item.owner_name, None);
}

#[tokio::test]
async fn delete_session_marks_completed_and_returns_204() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    // The mock's create_session inserts at Pending (the legacy
    // insert-then-update path); production uses create_session_created
    // / create_session_created which insert at a later state. Walk
    // the legal transitions to Active so DELETE sees a session in a
    // state from which Completed is reachable.
    seed_to(&store, id, SessionState::Active).await;

    let h = serve_grpc(TestFixture::new(store.clone(), InMemorySecretStore::new()).state).await;
    h.session()
        .delete_session(app::DeleteSessionRequest {
            session_id: id.to_string(),
        })
        .await
        .expect("DeleteSession must succeed (the old 204)");

    let after = store.get_session(id).await.unwrap();
    assert_eq!(after.status, SessionState::Completed);
}

#[tokio::test]
async fn delete_session_not_found_for_unknown_id() {
    // ADR 0051: an unknown id surfaces NotFound from `terminate_session`'s
    // own `get_session` (matching the get/exec contract) before any
    // teardown — the old 404. (Idempotency applies to already-terminal
    // rows, not to never-existed ones.)
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let unknown = SessionId::new();
    let err = h
        .session()
        .delete_session(app::DeleteSessionRequest {
            session_id: unknown.to_string(),
        })
        .await
        .expect_err("delete of an unknown session must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

// ---------------------------------------------------------------------
// Snapshot / evict / resume lifecycle
//
// All four operations share state through `AppState` (the registry +
// metadata + sandbox), so each test reuses the same router across
// requests. State machine the tests pin down:
//
//   POST /sessions             -> Active
//   POST /sessions/:id/snapshot -> Active   (record persisted, sandbox alive)
//   DELETE /sessions/:id/local -> Idle     (requires snapshot + Active)
//   POST /sessions/:id/resume   -> Active   (requires Idle + snapshot)
// ---------------------------------------------------------------------

async fn post(app: axum::Router, uri: &str, body: Value) -> axum::http::Response<Body> {
    app.oneshot(json_request(Method::POST, uri, body))
        .await
        .unwrap()
}

/// Budget for draining a gRPC `StreamEvents` / exec stream that does not
/// terminate on its own (we rely on the timeout).
const BRIEF: Duration = Duration::from_millis(2_000);
/// Tight budget proving streamed exec chunks arrive incrementally (before
/// a later `sleep` in the same command would let a buffered impl coalesce).
const STREAMING_PROOF: Duration = Duration::from_millis(450);

// ---------------------------------------------------------------------
// Streaming exec
// ---------------------------------------------------------------------

#[tokio::test]
async fn exec_stream_emits_stdout_frames_then_exit() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let frames = h
        .session()
        .exec(req_with_session(exec_command("printf hello"), id))
        .await
        .expect("Exec stream must open")
        .into_inner()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("exec frames");

    // The first frame is `started`, the last is `exit`; stdout in between.
    assert!(
        matches!(
            frames.first().and_then(|f| f.event.as_ref()),
            Some(app::exec_output::Event::Started(_))
        ),
        "stream must open with a `started` frame",
    );
    let mut stdout = Vec::new();
    let mut saw_exit = None;
    for f in &frames {
        match &f.event {
            Some(app::exec_output::Event::Stdout(b)) => stdout.extend_from_slice(b),
            Some(app::exec_output::Event::Exit(e)) => saw_exit = Some(*e),
            _ => {}
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout), "hello");
    let exit = saw_exit.expect("stream must end with an exit frame");
    assert_eq!(exit.exit_status.unwrap_or(0), 0);
}

#[tokio::test]
async fn exec_stream_returns_not_found_for_unknown_session() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = grpc_exec_err(&h, SessionId::new(), exec_command("true")).await;
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn exec_stream_returns_failed_precondition_when_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    let err = grpc_exec_err(&h, id, exec_command("true")).await;
    // ADR 0051: no live sandbox → Conflict core → FailedPrecondition (409).
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
}

#[tokio::test]
async fn exec_stream_chunks_arrive_before_process_exits() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    // 500ms gap between chunks. If the surface buffered to completion we'd
    // never see chunk-1 inside the STREAMING_PROOF budget.
    let mut stream = h
        .session()
        .exec(req_with_session(
            exec_command("printf chunk-1; sleep 0.5; printf chunk-2"),
            id,
        ))
        .await
        .expect("Exec stream")
        .into_inner();

    let deadline = tokio::time::Instant::now() + STREAMING_PROOF;
    let mut saw_first_chunk = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(f))) => {
                if let Some(app::exec_output::Event::Stdout(b)) = f.event {
                    if b == b"chunk-1" {
                        saw_first_chunk = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_first_chunk,
        "first chunk must arrive before the second printf — buffering would delay it",
    );
}

// ---------------------------------------------------------------------
// StreamEvents bus (gRPC). The old SSE `/sessions/:id/events` route is
// gone; the same persistent-log replay + live broadcast now flows over
// `SessionService::StreamEvents`. The proto `SessionEvent` carries
// `{ idx, kind, payload_json }` — the `kind` strings are unchanged
// (state.rs `SessionEvent::kind`), and `payload_json` is the JSON-string
// form of the old SSE `data:` body.
// ---------------------------------------------------------------------

/// Pull the `chunk` field out of a stdout/stderr event's `payload_json`.
fn event_chunk(ev: &app::SessionEvent) -> String {
    serde_json::from_str::<Value>(&ev.payload_json)
        .ok()
        .and_then(|v| v["chunk"].as_str().map(str::to_string))
        .unwrap_or_default()
}

#[tokio::test]
async fn events_stream_returns_not_found_for_unknown_session() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    // `events_core` runs `get_session` before the stream is produced, so
    // the NotFound surfaces at open.
    let err = open_events(&h, SessionId::new(), None)
        .await
        .expect_err("unknown session must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn events_stream_opens_for_known_session() {
    // The gRPC analog of the old "announces SSE content-type" test: the
    // stream opens cleanly for a known session (the transport framing is
    // tonic's job now, not ours to assert).
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;
    open_events(&h, id, None)
        .await
        .expect("StreamEvents must open for a known session");
}

#[tokio::test]
async fn events_subscriber_sees_snapshot_lifecycle() {
    // Subscribe first, then trigger snapshot. The subscriber must
    // observe the snapshot_taken event.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let stream = open_events(&h, id, None).await.expect("open events");
    let collector = tokio::spawn(collect_events(stream, BRIEF));

    // Tiny pause so the broadcast Receiver is in receive state before we
    // publish.
    tokio::time::sleep(Duration::from_millis(50)).await;
    grpc_snapshot(&h, id).await.expect("snapshot");

    let events = collector.await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"snapshot_taken"),
        "subscriber must see snapshot_taken; got {kinds:?}",
    );
}

#[tokio::test]
async fn events_stream_fans_out_to_multiple_subscribers() {
    // Two StreamEvents subscribers on the same session both see an action.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let stream_a = open_events(&h, id, None).await.expect("open a");
    let stream_b = open_events(&h, id, None).await.expect("open b");
    let coll_a = tokio::spawn(collect_events(stream_a, BRIEF));
    let coll_b = tokio::spawn(collect_events(stream_b, BRIEF));

    tokio::time::sleep(Duration::from_millis(50)).await;
    grpc_snapshot(&h, id).await.expect("snapshot");

    let events_a = coll_a.await.unwrap();
    let events_b = coll_b.await.unwrap();
    for events in [&events_a, &events_b] {
        assert!(
            events.iter().any(|e| e.kind == "snapshot_taken"),
            "every subscriber must see snapshot_taken; got {events:?}",
        );
    }
}

#[tokio::test]
async fn events_subscriber_sees_evict_then_resume_lifecycle() {
    // Drive a full snapshot → evict → resume cycle and observe each
    // lifecycle event in the bus, in order.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let stream = open_events(&h, id, None).await.expect("open events");
    let collector = tokio::spawn(collect_events(stream, BRIEF));

    tokio::time::sleep(Duration::from_millis(50)).await;
    grpc_snapshot(&h, id).await.expect("snapshot");
    grpc_evict_local(&h, id).await.expect("evict");
    grpc_resume(&h, id).await.expect("resume");

    let events = collector.await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    for needle in ["snapshot_taken", "evicted", "resumed"] {
        assert!(
            names.contains(&needle),
            "missing lifecycle event {needle:?} in {names:?}",
        );
    }
    let pos = |kind: &str| names.iter().position(|n| *n == kind);
    assert!(pos("snapshot_taken").unwrap() < pos("evicted").unwrap());
    assert!(pos("evicted").unwrap() < pos("resumed").unwrap());
}

#[tokio::test]
async fn exec_publishes_lifecycle_to_event_bus() {
    // Exec over gRPC routes through the event bus so a separate
    // StreamEvents observer sees the exec lifecycle and stdout chunks.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let stream = open_events(&h, id, None).await.expect("open events");
    let collector = tokio::spawn(collect_events(stream, BRIEF));

    tokio::time::sleep(Duration::from_millis(50)).await;
    let (stdout, _, exit, _) = grpc_exec(&h, id, exec_command("printf wired"))
        .await
        .expect("exec");
    assert_eq!(stdout, "wired");
    assert_eq!(exit, 0);

    let events = collector.await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(names.contains(&"exec_started"), "got {names:?}");
    assert!(names.contains(&"stdout"), "got {names:?}");
    assert!(names.contains(&"exec_completed"), "got {names:?}");

    let bus_stdout: String = events
        .iter()
        .filter(|e| e.kind == "stdout")
        .map(event_chunk)
        .collect();
    assert_eq!(bus_stdout, "wired");
}

#[tokio::test]
async fn snapshot_records_a_snapshot_and_keeps_session_active() {
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store.clone()).await;
    let id = grpc_create_session(&h, "r").await;

    let resp = grpc_snapshot(&h, id).await.expect("snapshot must succeed");
    assert!(
        resp.snapshot_id.is_some_and(|s| !s.is_empty()),
        "snapshot_id must be present after wiring",
    );

    // The live sandbox stays bound — snapshot is a save-point, not eviction.
    let after = store.get_session(id).await.unwrap();
    assert_eq!(
        after.status,
        SessionState::Active,
        "snapshot must NOT change session status — eviction is a separate call",
    );

    // Metadata received the SnapshotRecord we wrote.
    let recorded = store.list_snapshots_for_session(id).await.unwrap();
    assert_eq!(recorded.len(), 1);

    // After snapshot the live sandbox should still respond to exec.
    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printf still-alive"))
        .await
        .expect("exec");
    assert_eq!(stdout, "still-alive");
}

#[tokio::test]
async fn snapshot_returns_not_found_for_unknown_session() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = grpc_snapshot(&h, SessionId::new())
        .await
        .expect_err("unknown session must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn snapshot_returns_failed_precondition_when_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    let err = grpc_snapshot(&h, id)
        .await
        .expect_err("no live sandbox must be a Conflict");
    // ADR 0051: Conflict core → FailedPrecondition (the old 409).
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
}

#[tokio::test]
async fn evict_local_requires_a_snapshot_to_exist() {
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store.clone()).await;
    let id = grpc_create_session(&h, "r").await;

    // No snapshot yet: evicting would lose state → Conflict (the old 409).
    let err = grpc_evict_local(&h, id)
        .await
        .expect_err("evict without a snapshot must be a Conflict");
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");

    // Session must still be Active and the sandbox still bound.
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
    );
}

#[tokio::test]
async fn evict_local_after_snapshot_drops_sandbox_and_marks_idle() {
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store.clone()).await;
    let id = grpc_create_session(&h, "r").await;

    grpc_snapshot(&h, id).await.expect("snapshot");
    grpc_evict_local(&h, id).await.expect("evict must succeed");
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Idle,
    );

    // Phase 4 Track B: an exec on an Idle session transparently
    // auto-resumes from the snapshot before routing the command. The
    // session ends up Active again and the exec succeeds.
    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("echo back"))
        .await
        .expect("auto-resume should bring the Idle session back transparently");
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
        "session is Active after auto-resume",
    );
    assert_eq!(stdout, "back\n");
}

#[tokio::test]
async fn evict_local_fails_precondition_when_session_not_active() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    // Session is Pending (never created sandbox), can't evict.
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    let err = grpc_evict_local(&h, id)
        .await
        .expect_err("non-Active session can't be evicted");
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
}

#[tokio::test]
async fn resume_fails_precondition_when_session_not_idle() {
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store).await;
    let id = grpc_create_session(&h, "r").await;
    // Session is Active — resume only valid from Idle.
    let err = grpc_resume(&h, id)
        .await
        .expect_err("resume of a non-Idle session must be a Conflict");
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
}

#[tokio::test]
async fn resume_gone_when_no_snapshot_exists() {
    // An Idle session with no SnapshotRecord: resume must surface Gone —
    // the session's snapshot is invalidated and engram is a one-shot task
    // runner. ADR 0051: Gone core → FailedPrecondition, with the
    // `snapshot_invalidated` slug riding in `engram-error-slug` metadata
    // to distinguish it from a plain conflict (the `into_status` mapping).
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    seed_to(&store, id, SessionState::Idle).await;
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    let err = grpc_resume(&h, id)
        .await
        .expect_err("resume with no snapshot must be Gone");
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert_eq!(
        err.metadata()
            .get("engram-error-slug")
            .map(|v| v.as_bytes()),
        Some("snapshot_invalidated".as_bytes()),
        "Gone must carry the snapshot_invalidated slug (distinct from a plain conflict)",
    );
}

#[tokio::test]
async fn snapshot_evict_resume_round_trips_workspace_state() {
    // The headline end-to-end test: write a file in a session, snapshot,
    // evict, resume, verify the file is still there in the resumed
    // sandbox. ProcessBackend's tarball path round-trips on-disk state;
    // memory state is by definition not preserved in dev.
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store.clone()).await;
    let id = grpc_create_session(&h, "r").await;

    // Write a marker file in the live sandbox.
    grpc_exec(&h, id, exec_command("echo persisted > marker"))
        .await
        .expect("write marker");

    grpc_snapshot(&h, id).await.expect("snapshot");
    grpc_evict_local(&h, id).await.expect("evict");
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Idle,
    );

    grpc_resume(&h, id).await.expect("resume must succeed");
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
    );

    // The marker file must still be there in the resumed sandbox.
    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("cat marker"))
        .await
        .expect("read marker");
    assert_eq!(
        stdout, "persisted\n",
        "resume must restore the workspace state captured by snapshot",
    );
}

#[tokio::test]
async fn exec_rejects_request_without_command_or_argv() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    seed_to(&store, id, SessionState::Active).await;
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;

    // Neither command nor argv → BadRequest core → InvalidArgument.
    let req = app::ExecRequest {
        session_id: String::new(),
        command: None,
        argv: vec![],
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
    };
    let err = grpc_exec_err(&h, id, req).await;
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn exec_rejects_empty_argv() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    seed_to(&store, id, SessionState::Active).await;
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;
    // Empty argv with no command → InvalidArgument.
    let req = app::ExecRequest {
        session_id: String::new(),
        command: None,
        argv: vec![],
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
    };
    let err = grpc_exec_err(&h, id, req).await;
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

// ---------------------------------------------------------------------
// End-to-end exec wiring (gRPC CreateSession → placement → sandbox.exec)
// ---------------------------------------------------------------------

#[tokio::test]
async fn exec_round_trips_stdout_when_session_has_live_sandbox() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "cortex/api").await;

    let (stdout, stderr, exit, _) = grpc_exec(&h, id, exec_command("printf hello-engram"))
        .await
        .expect("exec");
    assert_eq!(exit, 0);
    assert_eq!(stdout, "hello-engram");
    assert_eq!(stderr, "");
}

#[tokio::test]
async fn exec_returns_rusage_with_nonzero_wall_ms_for_slow_command() {
    // The coordinator measures wall-clock around the
    // backend.exec_stream → final Exit window. A `sleep 0.1` is short
    // enough not to slow CI but long enough that wall_ms is reliably
    // ≥ 50ms even on a loaded runner.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let (_, _, _, wall_ms) = grpc_exec(&h, id, exec_command("sleep 0.1"))
        .await
        .expect("exec");
    assert!(
        wall_ms >= 50,
        "wall_ms ({wall_ms}) should reflect the ~100ms sleep",
    );
}

#[tokio::test]
async fn exec_includes_rusage_on_exit_frame() {
    // The streamed `exit` frame carries the rusage (proto `ExecExit`).
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let frames = h
        .session()
        .exec(req_with_session(exec_command("sleep 0.1"), id))
        .await
        .expect("exec stream")
        .into_inner()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("frames");
    let exit = frames
        .iter()
        .find_map(|f| match &f.event {
            Some(app::exec_output::Event::Exit(e)) => Some(*e),
            _ => None,
        })
        .expect("stream must end with an exit frame");
    let wall_ms = exit
        .rusage
        .as_ref()
        .map(|r| r.wall_ms)
        .expect("exit frame must carry rusage.wall_ms");
    assert!(wall_ms >= 50, "wall_ms ({wall_ms}) should reflect ~100ms");
}

#[tokio::test]
async fn exec_propagates_nonzero_exit_status() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;
    let (_, _, exit, _) = grpc_exec(&h, id, exec_command("exit 7"))
        .await
        .expect("exec");
    assert_eq!(exit, 7);
}

#[tokio::test]
async fn exec_separates_stdout_and_stderr() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;
    // printf avoids shell-dependent trailing-newline differences.
    let (stdout, stderr, _, _) = grpc_exec(&h, id, exec_command("printf out; printf err 1>&2"))
        .await
        .expect("exec");
    assert_eq!(stdout, "out");
    assert_eq!(stderr, "err");
}

#[tokio::test]
async fn exec_with_explicit_argv_skips_shell() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;
    // argv path: no `sh -c` wrapper, so glob/redirect chars are literal.
    let req = app::ExecRequest {
        session_id: String::new(),
        command: None,
        argv: vec!["printf".into(), "lit*ral".into()],
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
    };
    let (stdout, _, _, _) = grpc_exec(&h, id, req).await.expect("exec");
    assert_eq!(stdout, "lit*ral");
}

#[tokio::test]
async fn exec_returns_not_found_for_unknown_session() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = grpc_exec_err(&h, SessionId::new(), exec_command("true")).await;
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn exec_returns_failed_precondition_when_session_has_no_live_sandbox() {
    // A row in `Active` with no sandbox binding models "coord restart
    // lost the routing"; `ensure_active` passes but the live-sandbox
    // lookup yields Conflict → FailedPrecondition (the old 409).
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    seed_to(&store, id, SessionState::Active).await;
    let h = serve_grpc(TestFixture::new(store, InMemorySecretStore::new()).state).await;

    let err = grpc_exec_err(&h, id, exec_command("echo hi")).await;
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message().to_lowercase().contains("live sandbox"),
        "error message should explain why exec can't run: {:?}",
        err.message(),
    );
}

#[tokio::test]
async fn delete_after_create_unbinds_and_destroys_sandbox() {
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store.clone()).await;
    let id = grpc_create_session(&h, "r").await;

    // Confirm exec works while the session is alive.
    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printf alive"))
        .await
        .expect("exec while alive");
    assert_eq!(stdout, "alive");

    // Delete it.
    h.session()
        .delete_session(app::DeleteSessionRequest {
            session_id: id.to_string(),
        })
        .await
        .expect("delete");
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Completed
    );

    // Subsequent exec must fail — the live sandbox is gone (a terminal
    // session fails `ensure_active` → FailedPrecondition).
    let err = grpc_exec_err(&h, id, exec_command("echo nope")).await;
    assert_eq!(
        err.code(),
        Code::FailedPrecondition,
        "exec after delete must surface the missing sandbox: {err:?}",
    );
}

#[tokio::test]
async fn create_session_failure_returns_503_with_no_row() {
    // Build an app whose sandbox backend always errors on create.
    struct AlwaysFailSandbox;
    #[async_trait]
    impl engram_core::traits::SandboxBackend for AlwaysFailSandbox {
        async fn create(
            &self,
            _spec: engram_core::types::sandbox::SandboxSpec,
        ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
            Err(engram_core::SandboxError::LimitExceeded("test".into()))
        }
        async fn exec_stream(
            &self,
            _id: engram_core::SandboxId,
            _cmd: engram_core::types::sandbox::ExecRequest,
        ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError> {
            unreachable!()
        }
        async fn snapshot(
            &self,
            _id: engram_core::SandboxId,
        ) -> Result<engram_core::types::SnapshotMetadata, engram_core::SandboxError> {
            unreachable!()
        }
        fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
            std::path::PathBuf::from("/__test_always_fail_sandbox__")
        }
        async fn restore(
            &self,
            _metadata: engram_core::types::SnapshotMetadata,
        ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
            unreachable!()
        }
        async fn destroy(
            &self,
            _id: engram_core::SandboxId,
        ) -> Result<(), engram_core::SandboxError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
            Ok(vec![])
        }
    }

    let store = MockMetadataStore::arc();
    // Seed a `cortex/api:warm-1` enabled_images row so the create
    // handler clears the image-resolution gate and we get to the
    // sandbox failure the test is exercising.
    seed_enabled(&store, "cortex/api:warm-1", r#"name = "cortex-api""#);
    let services = Services {
        meta: store.clone(),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            AlwaysFailSandbox,
        ))),
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
        default_image_version: "warm-bootstrap".into(),
        app_grpc_tokens: vec![TEST_TOKEN.to_string()],
        ..CoordinatorConfig::default()
    };
    // ADR 0047/0048: placement reads host rows + ADR 0048 queues on no
    // capacity. Seed a schedulable host (and register the always-fail
    // backend under its id) so the create REACHES the restore — exercising
    // the boot-failure → 503 path this test is about, not the no-capacity
    // → queued path.
    let fail_host = engram_core::HostId::new();
    let registry = Arc::new(engram_coordinator::HostRegistry::new(store.clone()));
    registry.register(fail_host, services.host.clone());
    store
        .upsert_host(engram_core::types::host::HostRecord {
            id: fail_host,
            hostname: "always-fail".into(),
            cloud_metadata: Default::default(),
            capacity: engram_core::types::HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 16_384,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: engram_core::types::host::HostUtilization {
                allocatable_mib: 16_384,
                ..Default::default()
            },
            status: engram_core::types::host::HostStatus::Ready,
            last_heartbeat_at: chrono::Utc::now(),
            host_addr: None,
            ready_images: Vec::new(),
            local_snapshots: Vec::new(),
            current_bundles: Vec::new(),
            cordoned: false,
            total_vcpus: 8,
        })
        .await
        .unwrap();
    let state = Arc::new(AppState::new_with_registry(cfg, services, registry));
    let h = serve_grpc(state).await;

    let err = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "cortex/api:warm-1".into(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect_err("boot failure must surface as an error");
    // New contract: scheduling/boot failures bubble as Unavailable (the
    // old 503), no row is ever written. The caller is expected to retry.
    assert_eq!(err.code(), Code::Unavailable, "{err:?}");

    let active = store.list_active_sessions().await.unwrap();
    assert!(active.is_empty());
    let sessions = store.all_sessions();
    assert!(
        sessions.is_empty(),
        "no session row should be persisted when scheduling fails, got: {sessions:?}",
    );
}

#[tokio::test]
async fn unknown_route_returns_404() {
    // Surviving axum surface: an unknown HTTP path is still a 404 (the
    // healthz / host-ingest / forge-seam router rejects everything else).
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ADR 0051: `wrong_method_on_known_route_returns_405` targeted the deleted
// web-facing `/api/v1/sessions` route (PUT → 405). That route is gone from
// the axum surface entirely; method dispatch is now tonic's job on the
// app-gRPC services. Deleted — no surviving HTTP route has a method-mismatch
// contract worth pinning here.

// ---------------------------------------------------------------------
// Image manifest + secret resolution + rootfs materialization
// ---------------------------------------------------------------------

// Phase 2 removed the "no image / empty workdir" fallback path. The
// `session_with_no_image_falls_through_to_empty_workdir` test that
// exercised it lived here; it was deleted alongside the fallback
// because every session now declares an explicit image (verified by
// `create_session_with_unknown_image_returns_400`).

#[tokio::test]
async fn manifest_env_lands_in_sandbox_environment() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new());
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [env]
            PYTHONUNBUFFERED = "1"
            ENGRAM_TEST_MARKER = "from-manifest"
        "#,
    );
    let h = serve_grpc(f.state).await;

    let id = grpc_create_image(&h, "cortex/api:warm-1").await;

    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printf %s \"$ENGRAM_TEST_MARKER\""))
        .await
        .expect("exec");
    assert_eq!(stdout, "from-manifest");
}

// Stage B1 made the coordinator stateless w.r.t. image data — rootfs
// now flows from the registry through the host-agent's content-
// addressable cache, not from a filesystem `Rootfs::Directory`. There's
// no in-process registry in this unit-test harness, so the
// "materialize files into cwd" assertion can't be exercised here. The
// equivalent end-to-end coverage lives in `tests/registry_e2e.rs` (a
// real `registry:2` testcontainer).
#[tokio::test]
#[ignore = "rootfs is registry-pulled post-Stage-B1; covered by registry_e2e.rs"]
async fn rootfs_directory_is_materialized_into_sandbox_cwd() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new());
    f.write_image("cortex/api", "warm-1", r#"name = "cortex-api""#);
    let h = serve_grpc(f.state).await;

    let id = grpc_create_image(&h, "cortex/api:warm-1").await;

    // Files from the image's rootfs must be visible in the sandbox cwd.
    let (stdout, _, _, _) = grpc_exec(
        &h,
        id,
        exec_command("cat README.md && cat scripts/setup.sh | head -1"),
    )
    .await
    .expect("exec");
    assert!(stdout.contains("# starter"));
    assert!(stdout.contains("#!/bin/sh"));
}

#[tokio::test]
async fn required_secret_resolves_into_sandbox_env_in_literal_mode() {
    let store = MockMetadataStore::arc();
    let secrets = InMemorySecretStore::with_secrets([("GITHUB_TOKEN", "ghp_test_value")]);
    let f = TestFixture::new(store, secrets);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            secret_mode = "literal"

            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true
        "#,
    );
    let h = serve_grpc(f.state).await;

    let id = grpc_create_image(&h, "cortex/api:warm-1").await;

    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printf %s \"$GITHUB_TOKEN\""))
        .await
        .expect("exec");
    assert_eq!(stdout, "ghp_test_value");
}

#[tokio::test]
async fn required_secret_missing_in_store_fails_session_create() {
    let store = MockMetadataStore::arc();
    let secrets = InMemorySecretStore::new(); // empty
    let f = TestFixture::new(store, secrets);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true
        "#,
    );
    let h = serve_grpc(f.state).await;

    let err = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "cortex/api:warm-1".into(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect_err("required-but-missing secret must fail create");
    // Required-but-missing secret is an operator-config issue, not a
    // client problem → Internal core → Code::Internal (the old 500). The
    // message must call out the secret name so logs are debuggable.
    assert_eq!(err.code(), Code::Internal, "{err:?}");
    assert!(
        err.message().contains("GITHUB_TOKEN"),
        "missing-secret error must name the secret: {:?}",
        err.message(),
    );
}

#[tokio::test]
async fn optional_secret_absence_is_silently_ok() {
    let store = MockMetadataStore::arc();
    let secrets = InMemorySecretStore::with_secrets([("GITHUB_TOKEN", "real")]);
    let f = TestFixture::new(store, secrets);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true

            [secrets.OPTIONAL_KEY]
            allow_hosts = ["api.example.com"]
            required = false
        "#,
    );
    let h = serve_grpc(f.state).await;

    let id = grpc_create_image(&h, "cortex/api:warm-1").await;

    // Required is present; optional is silently absent (env var unset).
    let (stdout, _, _, _) = grpc_exec(
        &h,
        id,
        exec_command("echo \"required=$GITHUB_TOKEN optional=${OPTIONAL_KEY-MISSING}\""),
    )
    .await
    .expect("exec");
    assert_eq!(stdout, "required=real optional=MISSING\n");
}

#[tokio::test]
async fn broker_mode_emits_placeholders_not_real_values() {
    // Broker mode (production-shape): real values stay in the
    // SecretStore; the sandbox sees a placeholder. This is the
    // microsandbox-inspired security primitive — even with full code
    // execution inside the sandbox, the agent can't exfiltrate the
    // real GITHUB_TOKEN.
    //
    // The proxy that substitutes placeholders on outbound HTTPS
    // isn't wired yet (next round), so calls to api.github.com would
    // fail in this test setup. We just assert the env is a placeholder.
    let store = MockMetadataStore::arc();
    let secrets = InMemorySecretStore::with_secrets([("GITHUB_TOKEN", "ghp_real_secret_value")]);
    let f = TestFixture::new(store, secrets);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            secret_mode = "broker"

            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true
        "#,
    );
    let h = serve_grpc(f.state).await;

    let id = grpc_create_image(&h, "cortex/api:warm-1").await;

    let (observed, _, _, _) = grpc_exec(&h, id, exec_command("printf %s \"$GITHUB_TOKEN\""))
        .await
        .expect("exec");
    assert!(
        observed.starts_with("engram_ph_"),
        "broker mode must emit a placeholder; got {observed:?}",
    );
    assert!(
        !observed.contains("ghp_real_secret_value"),
        "broker mode must NOT leak the real secret value into the sandbox env",
    );
}

#[tokio::test]
async fn manifest_resource_hints_override_defaults() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new());
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [resources]
            suggested_memory_mib = 8192
            vcpus = 4
        "#,
    );
    let h = serve_grpc(f.state).await;

    // The hints flow through to the SandboxSpec the backend gets;
    // ProcessBackend doesn't enforce them, but a future Firecracker
    // backend will. Assert the session creation succeeds and reaches
    // an Active state (the hints are well-formed integers).
    let resp = h
        .session()
        .create_session(app::CreateSessionRequest {
            image_uri: "cortex/api:warm-1".into(),
            mode: "agent".into(),
            prompt: None,
            harness_secret_id: None,
            secrets: HashMap::new(),
        })
        .await
        .expect("create with resource hints")
        .into_inner();
    assert_eq!(resp.status, "active");
}

// ---------------------------------------------------------------------
// Session-id injection
// ---------------------------------------------------------------------

#[tokio::test]
async fn back_to_back_session_creates_succeed() {
    // Smoke test: two consecutive create calls on the same image
    // succeed and produce distinct sessions. Pre-v5 this test
    // exercised the warm-pool checkout/replenish path; with warm
    // pools deleted (ADR 0008) it now just sanity-checks the
    // chunked-OCI cold path doesn't deadlock on a second create.
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store).await;

    let id_a = grpc_create_session(&h, "warm/test").await;
    let id_b = grpc_create_session(&h, "warm/test").await;
    assert_ne!(id_a, id_b);

    // Both sessions should be runnable.
    for sid in [id_a, id_b] {
        let (stdout, _, _, _) = grpc_exec(&h, sid, exec_command("printf alive"))
            .await
            .expect("exec");
        assert_eq!(stdout, "alive");
    }
}

#[tokio::test]
async fn engram_session_id_is_injected_per_exec() {
    // ENGRAM_SESSION_ID must be injected at exec time, not baked in
    // at sandbox creation, so each session sees its own id.
    let store = MockMetadataStore::arc();
    let h = grpc_fixture(store).await;

    let id = grpc_create_session(&h, "warm/test").await;

    let (stdout, _, _, _) = grpc_exec(&h, id, exec_command("printf %s \"$ENGRAM_SESSION_ID\""))
        .await
        .expect("exec");
    assert_eq!(stdout, id.to_string());
}

// ---------------------------------------------------------------------
// Persistent event log + late-join via StreamEvents `since` (gRPC)
//
// The proto `SessionEvent.idx` is the persistent log's monotonic index
// (the old SSE `id:` field). `StreamEvents { since }` replays everything
// with idx > since; `since = None` (proto unset) = from the start.
//
// The Last-Event-ID-header variants of these tests
// (`last_event_id_header_drives_reconnect`,
// `since_query_wins_when_higher_than_last_event_id_header`) tested the
// deleted axum SSE handler's HTTP reconnect-header merge — a transport
// concern with no gRPC analog. The `since` replay semantics they shared
// are covered by `since_*` below; the header merge is dropped with the
// SSE handler.
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_persisted_event_carries_a_monotonic_idx_on_the_wire() {
    // Subscribe + drive a quick exec; each event must carry a monotonic
    // idx, strictly increasing and unique within a session.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    let stream = open_events(&h, id, None).await.expect("open events");
    let collector = tokio::spawn(collect_events(stream, BRIEF));

    tokio::time::sleep(Duration::from_millis(50)).await;
    grpc_exec(&h, id, exec_command("printf via-bus"))
        .await
        .expect("exec");

    let events = collector.await.unwrap();
    // Lag notifications carry no idx (proto optional); filter them out.
    let ids: Vec<i64> = events.iter().filter_map(|e| e.idx).collect();
    assert!(
        !ids.is_empty(),
        "live events should carry idx; got {events:?}"
    );
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "idx must arrive in monotonic order");
    let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "idx must be unique within a session"
    );
}

#[tokio::test]
async fn since_replays_history_from_persistent_log() {
    // Drive some activity with no subscriber, then open StreamEvents with
    // `since = None` (from the start) to replay everything — the live-only
    // bus would have missed it; the persistent log catches it up.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    grpc_snapshot(&h, id).await.expect("snapshot");
    grpc_evict_local(&h, id).await.expect("evict");
    grpc_resume(&h, id).await.expect("resume");

    let stream = open_events(&h, id, None).await.expect("open events");
    let events = collect_events(stream, Duration::from_millis(300)).await;
    let names: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    for needle in ["status_changed", "snapshot_taken", "evicted", "resumed"] {
        assert!(
            names.contains(&needle),
            "since=start must replay {needle}; got {names:?}",
        );
    }
}

#[tokio::test]
async fn since_skips_events_already_seen() {
    // Replay everything once to find a checkpoint idx, drive a second
    // batch, then reconnect with `since = checkpoint` and verify only
    // events strictly after the checkpoint come back.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    grpc_snapshot(&h, id).await.expect("snapshot");

    let baseline = open_events(&h, id, None).await.expect("baseline open");
    let baseline_events = collect_events(baseline, Duration::from_millis(300)).await;
    let checkpoint = baseline_events
        .iter()
        .filter_map(|e| e.idx)
        .max()
        .expect("baseline replay produced at least one event");

    // Second batch.
    grpc_evict_local(&h, id).await.expect("evict");

    let resume = open_events(&h, id, Some(checkpoint))
        .await
        .expect("resume open");
    let after = collect_events(resume, Duration::from_millis(300)).await;
    assert!(
        !after.is_empty(),
        "reconnect at checkpoint {checkpoint} should still replay the second batch",
    );
    for ev in after.iter().filter(|e| e.idx.is_some()) {
        let idx = ev.idx.unwrap();
        assert!(
            idx > checkpoint,
            "since={checkpoint} must skip everything up to and including that idx; saw idx={idx}",
        );
    }
    let names: Vec<&str> = after.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        names.contains(&"evicted"),
        "second-batch event missing: {names:?}"
    );
}

#[tokio::test]
async fn replay_then_live_seam_is_gap_free_and_dup_free() {
    // The hard test: open StreamEvents with `since = None` while live
    // events arrive. The seam between the replayed log and the live tail
    // must not duplicate or skip any idx.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    // Drive some history.
    grpc_snapshot(&h, id).await.expect("snapshot");

    // Subscribe from the start and concurrently drive more activity.
    let stream = open_events(&h, id, None).await.expect("open events");
    let collector = tokio::spawn(collect_events(stream, BRIEF));

    tokio::time::sleep(Duration::from_millis(50)).await;
    grpc_evict_local(&h, id).await.expect("evict");
    grpc_resume(&h, id).await.expect("resume");

    let events = collector.await.unwrap();
    let ids: Vec<i64> = events.iter().filter_map(|e| e.idx).collect();
    assert!(!ids.is_empty());

    // Strict monotonicity (no duplicates, no out-of-order).
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        ids, sorted,
        "idx across the replay→live seam must be strictly increasing with no duplicates",
    );

    // No gaps within the run we observe.
    if let (Some(first), Some(last)) = (ids.first(), ids.last()) {
        let expected_count = (last - first + 1) as usize;
        assert_eq!(
            ids.len(),
            expected_count,
            "no idx gap allowed across the seam — first={first} last={last} got {} events",
            ids.len(),
        );
    }
}

#[tokio::test]
async fn persistent_log_captures_lifecycle_and_exec_kinds() {
    // Defense in depth: drive every kind of event the coordinator emits
    // and verify each appears in the persistent log via a since=start
    // replay. If a future change forgets to call emit() in some core, this
    // test catches it.
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    // exec → exec_started, stdout, stderr, exec_completed
    grpc_exec(&h, id, exec_command("printf hi; printf err 1>&2"))
        .await
        .expect("exec");
    // snapshot → snapshot_taken
    grpc_snapshot(&h, id).await.expect("snapshot");
    // evict → evicted + status_changed
    grpc_evict_local(&h, id).await.expect("evict");
    // resume → resumed + status_changed
    grpc_resume(&h, id).await.expect("resume");

    let log = open_events(&h, id, None).await.expect("open log");
    let events = collect_events(log, Duration::from_millis(300)).await;
    let kinds: std::collections::HashSet<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    for required in [
        "status_changed",
        "exec_started",
        "exec_completed",
        "stdout",
        "stderr",
        "snapshot_taken",
        "evicted",
        "resumed",
    ] {
        assert!(
            kinds.contains(required),
            "persistent log must capture {required}; got {kinds:?}",
        );
    }
}

// ---------------------------------------------------------------------
// GetLog kind validation (gRPC). ADR 0005 retired the checkpoint / fork /
// diff endpoints with the platform's git surface, and ADR 0051 deleted the
// whole web-facing session REST surface — there are no such routes (and no
// gRPC RPCs) to 404-probe anymore. What survives is the `GetLog` kind
// guard: only `conversation` is supported; anything else is rejected.
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_log_rejects_unsupported_kind() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let id = grpc_create_session(&h, "r").await;

    // The old SSE `log?kind=workspace` → 400; now GetLog kind=workspace →
    // BadRequest core → InvalidArgument.
    let err = h
        .session()
        .get_log(app::GetLogRequest {
            session_id: id.to_string(),
            kind: Some("workspace".into()),
            limit: None,
        })
        .await
        .expect_err("unsupported log kind must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");

    // The default kind (`conversation`) is accepted.
    h.session()
        .get_log(app::GetLogRequest {
            session_id: id.to_string(),
            kind: None,
            limit: None,
        })
        .await
        .expect("conversation log must be accepted");
}

// ---------------------------------------------------------------------
// ADR 0051 DELETED the two `admin_reap_materialize_dir_*` tests with no
// replacement: the `POST /api/admin/reap-materialize-dir` route AND its
// handler/core were removed in the gRPC-only cut, and the FleetService
// proto deliberately ships NO RPC for it (fleet.proto: "INTENTIONAL
// DROPS: … POST /api/admin/reap-materialize-dir get NO RPC — neither has
// a web caller today. A later task decides keep-behind-bearer vs
// delete."). With the production capability itself dropped, there is no
// `_core` fn or service method left to exercise; the tests are removed
// rather than migrated. (The underlying host-side reap still exists as
// `HostService::ReapMaterializeDir` and is covered by host-agent tests.)
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// ADR 0016 Phase B: live-manifest publish HTTP round-trip
//
// Exercises the full host→coord boundary for the FlushScheduler's
// outcome: POST /api/hosts/:id/live-manifest with a JSON body
// matching the host-side serialization, decoded by the coord
// handler, dispatched into MetadataStore::update_live_disk_manifest,
// returning the Applied/Stale envelope. Wire format and routing
// regress here if anything diverges. Lives under the same axum
// router production uses (auth, JSON codecs, route table) so a
// missing route or shape mismatch fails loud.
// ---------------------------------------------------------------------

#[tokio::test]
async fn live_manifest_publish_round_trip_applied_and_stale() {
    use std::sync::atomic::Ordering;

    let meta = MockMetadataStore::arc();
    // Seed an Active session with a bound sandbox.
    let session_id = SessionId::new();
    let sandbox_id = engram_core::SandboxId::new();
    {
        let mut sessions = meta.sessions.lock();
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                user_id: None,
                status: SessionState::Active,
                host_id: None,
                sandbox_id: Some(sandbox_id),
                image: "test/repo:live-manifest".into(),
                mode: engram_core::types::session::SessionMode::Agent,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                live_disk_manifest: None,
            },
        );
    }
    let app = build_app(meta.clone());
    let host_id = engram_core::HostId::new();
    let manifest_id = uuid::Uuid::new_v4();

    // -- Applied path: matching sandbox_id, version=7 --
    let resp = post(
        app.clone(),
        &format!("/api/v1/hosts/{host_id}/live-manifest"),
        json!({
            "session_id": session_id,
            "sandbox_id": sandbox_id,
            "manifest_id": manifest_id,
            "manifest_version": 7u64,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["outcome"], "applied");
    // The mock stored the published ref + bumped generation in the
    // same logical TX as the row update.
    let stored = meta
        .live_disk_manifests
        .lock()
        .get(&session_id)
        .copied()
        .expect("live manifest stored");
    assert_eq!(stored.manifest_id, manifest_id);
    assert_eq!(stored.version, 7);
    assert_eq!(meta.chunk_generation.load(Ordering::SeqCst), 1);

    // -- Stale path: wrong sandbox_id (simulates publish-after-rebind) --
    let stale_sandbox = engram_core::SandboxId::new();
    let resp = post(
        app.clone(),
        &format!("/api/v1/hosts/{host_id}/live-manifest"),
        json!({
            "session_id": session_id,
            "sandbox_id": stale_sandbox,
            "manifest_id": manifest_id,
            "manifest_version": 8u64,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["outcome"], "stale");
    // Generation does NOT advance on stale; the prior Applied
    // value (version=7) is untouched.
    assert_eq!(meta.chunk_generation.load(Ordering::SeqCst), 1);
    let still_stored = meta
        .live_disk_manifests
        .lock()
        .get(&session_id)
        .copied()
        .expect("prior manifest still present");
    assert_eq!(still_stored.version, 7);
}

// ---------------------------------------------------------------------
// ADR 0016 Phase B commit 4a: admin flush-now endpoint
//
// 404, 409, and "idle" paths are reachable against the existing
// ProcessBackend wiring (whose flush_sandbox default returns None).
// The "applied" + "stale" outcomes exercise the full FC chunked-disk
// pipeline and live in commit 4b's e2e test.
// ---------------------------------------------------------------------

/// Flush a session over gRPC (the old `POST /admin/sessions/:id/flush-now`,
/// now `FleetService::FlushSession`).
async fn grpc_flush(
    h: &GrpcHarness,
    id: SessionId,
) -> Result<app::FlushSessionResponse, tonic::Status> {
    h.fleet()
        .flush_session(app::FlushSessionRequest {
            session_id: id.to_string(),
        })
        .await
        .map(|r| r.into_inner())
}

#[tokio::test]
async fn flush_session_returns_not_found_when_session_unknown() {
    let h = grpc_fixture(MockMetadataStore::arc()).await;
    let err = grpc_flush(&h, SessionId::new())
        .await
        .expect_err("unknown session must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn flush_session_fails_precondition_when_session_has_no_bound_sandbox() {
    let meta = MockMetadataStore::arc();
    let session_id = SessionId::new();
    {
        let mut sessions = meta.sessions.lock();
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                user_id: None,
                status: SessionState::Idle,
                host_id: None,
                sandbox_id: None,
                image: "test/repo:no-bind".into(),
                mode: engram_core::types::session::SessionMode::Agent,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                live_disk_manifest: None,
            },
        );
    }
    let h = grpc_fixture(meta).await;
    let err = grpc_flush(&h, session_id)
        .await
        .expect_err("no bound sandbox must be a Conflict");
    // ADR 0051: Conflict core → FailedPrecondition (the old 409).
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
}

#[tokio::test]
async fn flush_session_returns_idle_when_host_has_no_dirty_bytes() {
    let meta = MockMetadataStore::arc();
    let session_id = SessionId::new();
    // Bound to a sandbox the local ProcessBackend doesn't know about
    // — `flush_sandbox`'s trait default returns Ok(None), which the
    // core maps to `outcome: idle`.
    let sandbox_id = engram_core::SandboxId::new();
    {
        let mut sessions = meta.sessions.lock();
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                user_id: None,
                status: SessionState::Active,
                host_id: None,
                sandbox_id: Some(sandbox_id),
                image: "test/repo:idle-flush".into(),
                mode: engram_core::types::session::SessionMode::Agent,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                live_disk_manifest: None,
            },
        );
    }
    let h = grpc_fixture(meta.clone()).await;
    let resp = grpc_flush(&h, session_id)
        .await
        .expect("flush must succeed");
    assert_eq!(resp.outcome, "idle");
    // No PG write happened — generation stayed at 0.
    assert!(meta.live_disk_manifests.lock().is_empty());
    assert_eq!(
        meta.chunk_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
    );
}

#[tokio::test]
async fn live_manifest_publish_unbind_clears_and_bumps_generation() {
    use std::sync::atomic::Ordering;

    let meta = MockMetadataStore::arc();
    let session_id = SessionId::new();
    let sandbox_id = engram_core::SandboxId::new();
    {
        let mut sessions = meta.sessions.lock();
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                user_id: None,
                status: SessionState::Active,
                host_id: None,
                sandbox_id: Some(sandbox_id),
                image: "test/repo:unbind".into(),
                mode: engram_core::types::session::SessionMode::Agent,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                live_disk_manifest: None,
            },
        );
    }
    let app = build_app(meta.clone());
    let host_id = engram_core::HostId::new();
    let manifest_id = uuid::Uuid::new_v4();

    // Publish first, so unbinding has something to clear.
    let resp = post(
        app.clone(),
        &format!("/api/v1/hosts/{host_id}/live-manifest"),
        json!({
            "session_id": session_id,
            "sandbox_id": sandbox_id,
            "manifest_id": manifest_id,
            "manifest_version": 3u64,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let gen_after_publish = meta.chunk_generation.load(Ordering::SeqCst);

    // Direct trait call (no API endpoint for assign_session_sandbox).
    // ADR 0016 Phase B: this is the load-bearing eviction-race
    // mitigation — assign_session_sandbox(None) clears the live
    // manifest + bumps chunk_generation in the same logical step
    // the sandbox_id NULLs out.
    engram_core::traits::MetadataStore::assign_session_sandbox(meta.as_ref(), session_id, None)
        .await
        .unwrap();
    assert!(meta.live_disk_manifests.lock().get(&session_id).is_none());
    assert_eq!(
        meta.chunk_generation.load(Ordering::SeqCst),
        gen_after_publish + 1,
    );
}
