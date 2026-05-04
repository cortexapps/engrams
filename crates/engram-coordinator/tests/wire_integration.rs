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
    enabled: Mutex<HashMap<String, engram_core::types::EnabledImage>>,
    events: Mutex<HashMap<SessionId, Vec<PersistedEvent>>>,
    next_idx: Mutex<HashMap<SessionId, i64>>,
    snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
}

#[async_trait]
impl MetadataStore for MiniMeta {
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        use engram_core::types::session::SessionKind;
        let id = SessionId::new();
        let session_kind = SessionKind::derive(&spec.workspace);
        self.sessions.lock().insert(
            id,
            Session {
                id,
                user_id: spec.user_id,
                status: SessionStatus::Pending,
                host_id: None,
                sandbox_id: None,
                created_at: Utc::now(),
                image: spec.image,
                workspace: spec.workspace,
                harness: spec.harness,
                session_kind,
                checkpoint_branch: None,
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
                s.status = SessionStatus::Dead;
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
    async fn upsert_harness_pack(
        &self,
        _: engram_core::types::HarnessPack,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_harness_packs(&self) -> Result<Vec<engram_core::types::HarnessPack>, MetaError> {
        Ok(Vec::new())
    }
    async fn get_harness_pack(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::HarnessPack>, MetaError> {
        Ok(None)
    }
    async fn delete_harness_pack(&self, _: &str) -> Result<(), MetaError> {
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
    async fn delete_enabled_image(&self, uri: &str) -> Result<(), MetaError> {
        self.enabled.lock().remove(uri);
        Ok(())
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
    // Phase 2 requires every session to declare an explicit image
    // that resolves in the registry. Seed `demo:warm-test` here so
    // the wire round-trip's create_session call can reach the
    // host backend without 400ing on image resolution.
    {
        let img_dir = images_dir.join("demo/warm-test");
        std::fs::create_dir_all(&img_dir).unwrap();
        std::fs::write(img_dir.join("manifest.toml"), r#"name = "demo""#).unwrap();
    }

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
    seed_enabled(&meta, "demo:warm-test", r#"name = "demo""#);

    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        images: ImageRegistry::new(images_dir),
        harnesses: Arc::new(engram_coordinator::harness_registry::HarnessRegistry::empty()),
        harness_substrate: None,
        egress_proxy: None,
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-test".into(),
        default_warm_pool_size: 0,
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
    (api::router(state), serve_handle)
}

/// Seed a `demo:warm-test` row directly so create_session clears the
/// enabled-images gate without going through `/api/enabled-images`.
fn seed_enabled(meta: &MiniMeta, uri: &str, manifest_toml: &str) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut h);
    let now = chrono::Utc::now();
    meta.enabled.lock().insert(
        uri.to_string(),
        engram_core::types::EnabledImage {
            id: uuid::Uuid::new_v4(),
            image_uri: uri.to_string(),
            manifest_toml: manifest_toml.to_string(),
            manifest_digest: format!("sha256:{:08x}", h.finish()),
            last_refreshed_at: now,
            created_at: now,
            updated_at: None,
        },
    );
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
                .body(Body::from(
                    r#"{
                        "image": "demo:warm-test",
                        "workspace":{"kind":"empty"},
                        "harness":{"kind":"none"}
                    }"#,
                ))
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
async fn create_with_no_hosts_registered_returns_500_with_clear_message() {
    // Build an AppState with an empty HostRegistry. The Phase 3a
    // single-host scheduler's "no host" path must surface as a 500
    // with a meaningful message rather than silently hanging.
    let images_dir = tempfile::tempdir().expect("images tmp").keep();
    // Seed `demo:warm-test` on disk so `ImageRegistry::load` resolves;
    // without this the create_session handler returns 400 from the
    // image-not-found arm and the no-host path is never exercised.
    {
        let img_dir = images_dir.join("demo/warm-test");
        std::fs::create_dir_all(&img_dir).unwrap();
        std::fs::write(img_dir.join("manifest.toml"), r#"name = "demo""#).unwrap();
    }
    let host_registry = Arc::new(HostRegistry::new());
    let meta = Arc::new(MiniMeta::default());
    meta.images
        .lock()
        .entry("demo".into())
        .or_default()
        .push(ignored_image());
    seed_enabled(&meta, "demo:warm-test", r#"name = "demo""#);

    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        images: ImageRegistry::new(images_dir),
        harnesses: Arc::new(engram_coordinator::harness_registry::HarnessRegistry::empty()),
        harness_substrate: None,
        egress_proxy: None,
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
                .body(Body::from(
                    r#"{
                        "image": "demo:warm-test",
                        "workspace":{"kind":"empty"},
                        "harness":{"kind":"none"}
                    }"#,
                ))
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
