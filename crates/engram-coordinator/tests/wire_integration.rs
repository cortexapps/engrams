//! End-to-end integration test of the Phase 3a wire path:
//! `POST /sessions` → AppState → HostRegistry → RemoteSandboxBackend
//! → mpsc-channel pair → HostSession::serve_with_reader → ProcessBackend.
//!
//! Smaller than `tests/api.rs` (which exercises the in-process backend
//! directly) but proves the wire-routed path works at the AppState
//! integration layer. The existing 69 api.rs tests still cover handler
//! behaviour against the in-process backend.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use engram_cloud_mock::MockCloud;
use engram_coordinator::image_registry::ImageRegistry;
use engram_coordinator::{api, AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{MetadataStore, SandboxBackend};
use engram_core::types::{
    HostRecord, HostStatus, ImageStatus, ImageVersion, PersistedEvent, Session, SessionSpec,
    SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, ImageVersionId, MetaError, SessionId};
use engram_protocol::client::{ConnectedHost, RemoteSandboxBackend};
use engram_protocol::server::HostSession;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::Value;
use tokio_tungstenite::tungstenite::{Error as TungError, Message as TungMessage};
use tower::ServiceExt;

#[derive(Default)]
struct MiniMeta {
    sessions: Mutex<HashMap<SessionId, Session>>,
    images: Mutex<HashMap<String, Vec<ImageVersion>>>,
    events: Mutex<HashMap<SessionId, Vec<PersistedEvent>>>,
    next_idx: Mutex<HashMap<SessionId, i64>>,
    snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
}

#[async_trait]
impl MetadataStore for MiniMeta {
    async fn create_session(
        &self,
        spec: SessionSpec,
        image_version: String,
    ) -> Result<SessionId, MetaError> {
        let id = SessionId::new();
        self.sessions.lock().insert(
            id,
            Session {
                id,
                repo: spec.repo,
                branch: spec.branch,
                user_id: spec.user_id,
                status: SessionStatus::Pending,
                image_version,
                host_id: None,
                sandbox_id: None,
                created_at: Utc::now(),
                session_kind: engram_core::types::session::SessionKind::Local,
                repo_url: None,
                checkpoint_branch: None,
                last_harness_event_at: None,
                last_active_at: Utc::now(),
            },
        );
        Ok(id)
    }
    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        self.sessions
            .lock()
            .get(&id)
            .cloned()
            .ok_or(MetaError::NotFound)
    }
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(self.sessions.lock().values().cloned().collect())
    }
    async fn set_session_status(
        &self,
        id: SessionId,
        status: SessionStatus,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.status = status;
        Ok(())
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
    async fn list_stale_hosts(&self, _threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn mark_host_dead_and_reassign_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SessionId>, MetaError> {
        let mut g = self.sessions.lock();
        let mut affected = Vec::new();
        for s in g.values_mut() {
            if s.host_id == Some(host_id)
                && !matches!(s.status, SessionStatus::Completed | SessionStatus::Failed)
            {
                s.host_id = None;
                s.sandbox_id = None;
                s.status = SessionStatus::PendingReassign;
                s.last_active_at = Utc::now();
                affected.push(s.id);
            }
        }
        Ok(affected)
    }
    async fn record_snapshot(&self, s: SnapshotRecord) -> Result<(), MetaError> {
        self.snapshots
            .lock()
            .entry(s.session_id)
            .or_default()
            .push(s);
        Ok(())
    }
    async fn list_snapshots_for_session(
        &self,
        id: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(self.snapshots.lock().get(&id).cloned().unwrap_or_default())
    }
    async fn latest_snapshot_for_session(
        &self,
        id: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self
            .snapshots
            .lock()
            .get(&id)
            .and_then(|v| v.last().cloned()))
    }
    async fn upsert_image_version(&self, version: ImageVersion) -> Result<(), MetaError> {
        self.images
            .lock()
            .entry(version.repo.clone())
            .or_default()
            .push(version);
        Ok(())
    }
    async fn latest_ready_image(&self, repo: &str) -> Result<Option<ImageVersion>, MetaError> {
        Ok(self
            .images
            .lock()
            .get(repo)
            .and_then(|v| v.iter().rfind(|i| i.status == ImageStatus::Ready).cloned()))
    }
    async fn append_session_event(
        &self,
        sid: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError> {
        let mut idxs = self.next_idx.lock();
        let idx = *idxs.entry(sid).or_insert(0);
        idxs.insert(sid, idx + 1);
        drop(idxs);
        self.events
            .lock()
            .entry(sid)
            .or_default()
            .push(PersistedEvent {
                idx,
                kind: kind.to_string(),
                payload,
                created_at: Utc::now(),
            });
        Ok(idx)
    }
    async fn list_session_events_since(
        &self,
        sid: SessionId,
        since: i64,
        _limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(self
            .events
            .lock()
            .get(&sid)
            .map(|v| v.iter().filter(|e| e.idx > since).cloned().collect())
            .unwrap_or_default())
    }
}

fn ignored_image() -> ImageVersion {
    ImageVersion {
        id: ImageVersionId::new(),
        repo: "demo".into(),
        tag: "warm-test".into(),
        blob_url: None,
        status: ImageStatus::Ready,
        created_at: Utc::now(),
    }
}

/// Build an AppState whose services.sandbox routes through a HostRegistry
/// → wire → ProcessBackend chain. Exercises the same path a real
/// `--mode=coordinator` + `engram-host-agent` deployment uses, just
/// without an actual TCP/WS handshake (mpsc channels carry the frames).
fn build_wired_router() -> (axum::Router, tokio::task::JoinHandle<()>) {
    let sandbox_dir = tempfile::tempdir().expect("host work_dir").keep();
    let images_dir = tempfile::tempdir().expect("images tmp").keep();

    // Wire setup: in-memory mpsc pair stands in for the WS connection.
    let (coord_tx_a, host_rx_a) = futures::channel::mpsc::unbounded::<TungMessage>();
    let (host_tx_b, coord_rx_b) = futures::channel::mpsc::unbounded::<TungMessage>();

    let coord_sink =
        coord_tx_a.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let coord_stream = coord_rx_b.map(Ok::<TungMessage, TungError>);
    let host_sink = host_tx_b.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let host_stream = host_rx_a.map(Ok::<TungMessage, TungError>);

    let (connected, _notify_rx, _demux) =
        ConnectedHost::spawn(Box::pin(coord_sink), Box::pin(coord_stream));
    let session = HostSession::new(Box::pin(host_sink));

    // Host-side: ProcessBackend serves the requests.
    let local_backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_dir));
    let serve_handle = tokio::spawn(async move {
        session
            .serve_with_reader(local_backend, None, Box::pin(host_stream))
            .await;
    });

    // Coordinator-side: register the wire-connected backend in HostRegistry.
    let host_registry = Arc::new(HostRegistry::new());
    let remote: Arc<dyn SandboxBackend> = Arc::new(RemoteSandboxBackend::new(connected));
    host_registry.register(HostId::new(), remote);

    let meta = Arc::new(MiniMeta::default());
    meta.images
        .lock()
        .entry("demo".into())
        .or_default()
        .push(ignored_image());

    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
        secrets: Arc::new(InMemorySecretStore::new()),
        images: ImageRegistry::new(images_dir),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-test".into(),
        default_warm_pool_size: 0,
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
    (api::router(state), serve_handle)
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_then_exec_round_trips_via_wire() {
    let (app, _serve) = build_wired_router();

    // 1. POST /sessions — coordinator's create_session calls
    //    services.sandbox.create() which is HostRegistry → wire →
    //    ProcessBackend.
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"demo","branch":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let body = body_json(create.into_body()).await;
    let session_id = body["session_id"].as_str().expect("session_id present");

    // 2. POST /sessions/:id/exec — same routing chain for exec.
    let exec = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/sessions/{session_id}/exec"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command":"printf hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exec.status(), StatusCode::OK, "exec via wire must succeed");
    let body = body_json(exec.into_body()).await;
    assert_eq!(body["stdout"], "hello", "stdout must round-trip via wire");
    assert_eq!(body["exit_status"], 0);

    // 3. DELETE /sessions/:id — destroy routes through wire too.
    let del = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn migrate_then_resume_round_trips_via_pending_reassign() {
    // Phase 3d: snapshot → migrate (clears host_id, status =
    // pending_reassign) → resume (scheduler picks new host, restores
    // from snapshot, transitions to active). Single-host fixture so
    // "new host" is the same host, but the state-machine + handler
    // path is exercised.
    let (app, _serve) = build_wired_router();

    // 1. Create.
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"demo","branch":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let session_id = body_json(create.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. Snapshot (so /resume has something to restore from).
    let snap = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/sessions/{session_id}/snapshot"))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap.status(), StatusCode::OK, "snapshot must succeed");

    // 3. Migrate. No target host_id → operator just signals "rebalance
    //    on next access."
    let mig = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/sessions/{session_id}/migrate"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mig.status(), StatusCode::OK, "migrate must succeed");
    let body = body_json(mig.into_body()).await;
    assert_eq!(body["status"], "pending_reassign");

    // 4. Read back: session.status = pending_reassign, host_id = null.
    let g = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(g.status(), StatusCode::OK);
    let body = body_json(g.into_body()).await;
    assert_eq!(body["status"], "pending_reassign");
    assert!(
        body["host_id"].is_null(),
        "host_id must be cleared after migrate; got {body:?}"
    );

    // 5. Resume — the cold path picks a new host (here the same one),
    //    restores from snapshot, and transitions back to active.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/sessions/{session_id}/resume"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "resume must succeed");

    let g = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(g.into_body()).await;
    assert_eq!(body["status"], "active");
    assert!(
        !body["host_id"].is_null(),
        "host_id must be re-populated after resume; got {body:?}"
    );
}

#[tokio::test]
async fn migrate_without_snapshot_fails_with_clear_message() {
    // Pre-condition guard: migrate refuses transitions that would
    // strand the session on the next /resume because there's no
    // snapshot to bring it back from.
    let (app, _serve) = build_wired_router();

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"demo","branch":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let session_id = body_json(create.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mig = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/sessions/{session_id}/migrate"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        mig.status(),
        StatusCode::CONFLICT,
        "migrate without prior snapshot must 409"
    );
    let body = body_json(mig.into_body()).await;
    let msg = body["message"].as_str().unwrap_or("");
    assert!(msg.contains("snapshot"), "error must explain why: {msg:?}");
}

#[tokio::test]
async fn create_with_no_hosts_registered_returns_500_with_clear_message() {
    // Build an AppState with an empty HostRegistry. The Phase 3a
    // single-host scheduler's "no host" path must surface as a 500
    // with a meaningful message rather than silently hanging.
    let images_dir = tempfile::tempdir().expect("images tmp").keep();
    let host_registry = Arc::new(HostRegistry::new());
    let meta = Arc::new(MiniMeta::default());
    meta.images
        .lock()
        .entry("demo".into())
        .or_default()
        .push(ignored_image());

    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
        secrets: Arc::new(InMemorySecretStore::new()),
        images: ImageRegistry::new(images_dir),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-test".into(),
        default_warm_pool_size: 0,
        ..CoordinatorConfig::default()
    };
    let app = api::router(Arc::new(AppState::new_with_registry(
        cfg,
        services,
        host_registry,
    )));

    let create = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"demo","branch":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        create.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "no-host path should not silently succeed",
    );
    let body = body_json(create.into_body()).await;
    let msg = body["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no hosts connected") || msg.contains("no host"),
        "error message must explain why: got {msg:?}",
    );
}
