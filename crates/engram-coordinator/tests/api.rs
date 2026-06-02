//! Integration test for the coordinator HTTP API.
//!
//! Wires the real `axum` router against an in-memory `MetadataStore`,
//! a `MockCloud`, a `LocalStorage` blob backend, and `ProcessBackend`
//! (the dev-loop SandboxBackend that runs commands as host
//! subprocesses), then drives the surface via
//! `tower::ServiceExt::oneshot`. The goal is to lock down the public
//! contract — status codes, JSON error envelope, the routing table —
//! while exercising real exec round-trips end-to-end.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{api, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SessionId};
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
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
            .filter(|s| {
                matches!(
                    s.status,
                    SessionState::Pending | SessionState::Active | SessionState::Idle
                )
            })
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

    async fn upsert_host(&self, _host: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }

    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }

    async fn set_host_status(&self, _id: HostId, _status: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }

    async fn touch_host_heartbeat(
        &self,
        _id: HostId,
        _status: HostStatus,
        _cap: engram_core::types::HostCapacity,
    ) -> Result<(), MetaError> {
        Ok(())
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
    meta: Arc<MockMetadataStore>,
    /// ADR 0015 M5: pinned to the single in-process host so test
    /// helpers can flip its `ready_images` set in lockstep with
    /// `seed_enabled` calls. Production hosts populate this from
    /// the prefetch supervisor + heartbeat, but the in-test ProcessBackend
    /// has no chunks to prefetch — we just declare it ready.
    host_registry: Arc<engram_coordinator::HostRegistry>,
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
            ..CoordinatorConfig::default()
        };
        // ADR 0015 M5: build the registry explicitly so we can keep
        // a handle to it and pin the host id we use to seed
        // `ready_images` from `write_image`.
        let host_registry = Arc::new(engram_coordinator::HostRegistry::new(meta.clone()));
        let test_host_id = engram_core::HostId::new();
        host_registry.register(test_host_id, services.host.clone());
        let state = Arc::new(AppState::new_with_registry(
            cfg,
            services,
            host_registry.clone(),
        ));
        let fx = Self {
            app: api::router(state),
            meta: meta.clone(),
            host_registry,
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
        let prior = self
            .host_registry
            .snapshot_state(self.test_host_id)
            .unwrap_or_default();
        let mut next = prior;
        next.ready_images.insert(digest);
        self.host_registry.update_state(self.test_host_id, next);
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

/// Hit `POST /sessions` against `app` and return the new SessionId.
/// The router consumes itself per request, so callers need to pass a
/// fresh clone of the router for any subsequent request. Sends the
/// new orthogonal wire shape with an `Empty` workspace and `None`
/// harness — any test that needs Git workspaces or an attached
/// harness sends the request directly.
async fn api_create_session(app: axum::Router, repo: &str) -> SessionId {
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": format!("{repo}:warm-bootstrap"),
                "workspace": {"kind":"empty"},
                "harness": {"kind":"none"},
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "session create must succeed in test fixture",
    );
    let v = body_json(resp.into_body()).await;
    v["session_id"].as_str().unwrap().parse().unwrap()
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

#[tokio::test]
async fn auth_disabled_when_token_list_empty() {
    // Default fixture path — no `Authorization` header, but auth is
    // disabled. This is the dev-loop and existing-test default.
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

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
async fn human_routes_are_principal_authed_not_bearer_gated() {
    // ADR 0031: even with deployment tokens set, a human route resolves via
    // the principal layer. With no auth runtime wired (test default), that's
    // the synthetic admin → 200, with or without an Authorization header.
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
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
async fn create_session_requires_image_and_workspace() {
    // Phase 2 wire shape: `image` and `workspace` are mandatory.
    // axum's `Json<T>` extractor surfaces missing required fields
    // as 422 Unprocessable Entity (rather than 400) — body never
    // reaches the handler.
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({"workspace": {"kind":"empty"}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
    let app = f.app;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": "demo/dev-vm-from-harnessed:v1",
                "mode": "dev_vm",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["status"], "active");
    let id: SessionId = v["session_id"].as_str().unwrap().parse().unwrap();
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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({ "image": "demo/env-inject:v1", "mode": "dev_vm" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    let id: SessionId = v["session_id"].as_str().unwrap().parse().unwrap();

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({ "command": "printenv ENGRAM_TEST_IMAGE_VAR" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["stdout"].as_str().unwrap_or_default().trim(),
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
    let app = f.app;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": "cortex/api:warm-pinned",
                "workspace": {"kind":"empty"},
                "harness": {"kind":"none"},
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["image_version"], "warm-pinned");
    assert_eq!(v["status"], "active");
    let id: SessionId = v["session_id"].as_str().unwrap().parse().unwrap();
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
async fn create_session_with_unknown_image_returns_400() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": "never-baked:x",
                "workspace": {"kind":"empty"},
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["message"].as_str().unwrap().contains("not enabled"),
        "error must call out the unenabled image: got {v}",
    );
}

#[tokio::test]
async fn create_session_prompt_with_dev_vm_mode_is_400() {
    // ADR 0021 P1.3: `prompt` requires an agent to receive it. A
    // dev-VM session leaves the image's baked harness undriven, so
    // there's nothing on the other end of `prompt` — reject with 400
    // rather than silently dropping the prompt.
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": "r:warm-bootstrap",
                "mode": "dev_vm",
                "prompt": "do the thing",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
async fn get_session_returns_404_for_unknown_id() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{unknown}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "not_found");
}

#[tokio::test]
async fn get_session_returns_400_for_malformed_id() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions/not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // axum's Path<SessionId> deserialisation fails -> 400.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_sessions_returns_empty_array_when_store_is_empty() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["sessions"]
            .as_array()
            .expect("sessions must be an array")
            .len(),
        0,
    );
}

#[tokio::test]
async fn storage_summary_zeros_on_empty_fleet() {
    // ADR 0029: with no registered hosts and an empty metadata store,
    // the Storage surface's summary returns a well-formed all-zero
    // shape — never `NaN`/null — so the page renders cleanly on a
    // fresh deployment. The mock store's default `snapshot_totals` /
    // `count_gc_candidates` supply the zeros.
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::get("/api/v1/storage/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["snapshots"], 0);
    assert_eq!(v["snapshot_bytes"], 0);
    assert_eq!(v["gc_pending"], 0);
    assert_eq!(v["tracked_sandboxes"], 0);
    assert_eq!(v["dirty_chunks"], 0);
    assert_eq!(v["unflushed_bytes"], 0);
    assert_eq!(v["avg_locality_pct"], 0);
    assert_eq!(
        v["rows"].as_array().expect("rows must be an array").len(),
        0,
    );
}

#[tokio::test]
async fn list_sessions_returns_pending_active_and_idle_only() {
    // The endpoint mirrors `list_active_sessions` semantics: pending /
    // active / idle rows show up; completed / failed / evicted rows
    // are filtered out. Filtering at the surface keeps the default
    // `engram session list` view focused on live work.
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
    let dead_id = mk(&store, "done").await;
    seed_to(&store, dead_id, SessionState::Completed).await;

    let app = build_app(store);
    // ADR 0031: list is owner-scoped; the test caller is the synthetic admin,
    // so `scope=all` returns every session (the pre-scoping behaviour).
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions?scope=all")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let ids: Vec<String> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&active_id.to_string()), "active row missing");
    assert!(ids.contains(&idle_id.to_string()), "idle row missing");
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

    let app = build_app(store);
    let resp = app
        .oneshot(
            Request::get("/api/v1/sessions?scope=all")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let item = &v["sessions"]
        .as_array()
        .expect("sessions must be an array")
        .iter()
        .find(|s| s["id"] == id.to_string())
        .expect("created session must be in the list");
    // Lock the wire shape so a CLI / web client can rely on it.
    // Stage B1: image is a flat OCI URI string. ADR 0005: workspace
    // is gone; the bake image is the whole story.
    assert_eq!(item["image"], "cortex/api:warm-2026-04-27");
    assert!(
        item.get("workspace").is_none(),
        "ADR 0005 retired the workspace field"
    );
    assert!(
        item.get("harness").is_none(),
        "ADR 0021 P1.3 retired the per-session harness field"
    );
    assert_eq!(item["mode"], "dev_vm");
    assert_eq!(item["user_id"], "user-42");
    assert_eq!(item["status"], SessionState::Pending.as_str());
    assert!(item["created_at"].is_string());
    assert!(item["last_active_at"].is_string());
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

    let app = build_app(store.clone());
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after = store.get_session(id).await.unwrap();
    assert_eq!(after.status, SessionState::Completed);
}

#[tokio::test]
async fn delete_session_404_for_unknown_id() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/sessions/{unknown}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

async fn delete(app: axum::Router, uri: &str) -> axum::http::Response<Body> {
    app.oneshot(Request::delete(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// One parsed SSE message: the optional `id:`, the `event:` name, and
/// the JSON `data:` body. `id` carries the persistent log's monotonic
/// idx — used by EventSource clients on reconnect.
#[derive(Debug, Clone)]
struct SseEvent {
    id: Option<i64>,
    name: String,
    data: serde_json::Value,
}

/// Drain `body` as SSE for up to `budget`. Stops early when the body
/// ends (which is what /exec/stream does after `exit`). Used by both
/// streaming-exec tests (where the stream terminates naturally) and
/// /events tests (where it doesn't, and we rely on the budget).
async fn collect_sse(body: Body, budget: Duration) -> Vec<SseEvent> {
    use futures::StreamExt;

    let mut events = Vec::new();
    let mut buf = String::new();
    let mut frame_stream = body.into_data_stream();
    let deadline = tokio::time::Instant::now() + budget;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, frame_stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(idx) = buf.find("\n\n") {
                    let block = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    if let Some(ev) = parse_sse_block(&block) {
                        events.push(ev);
                    }
                }
            }
            // Body finished or errored — done.
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
    events
}

fn parse_sse_block(block: &str) -> Option<SseEvent> {
    let mut id: Option<i64> = None;
    let mut name: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        // `:` is an SSE comment (axum's keep-alive uses these).
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start_matches(' '));
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = rest.trim().parse().ok();
        }
    }
    let name = name?;
    let data = data_lines.join("\n");
    let parsed = serde_json::from_str(&data).unwrap_or(Value::String(data));
    Some(SseEvent {
        id,
        name,
        data: parsed,
    })
}

const BRIEF: Duration = Duration::from_millis(2_000);
const STREAMING_PROOF: Duration = Duration::from_millis(450);

// ---------------------------------------------------------------------
// Streaming exec
// ---------------------------------------------------------------------

#[tokio::test]
async fn exec_stream_emits_stdout_chunks_then_exit() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec/stream"),
        json!({"command": "printf hello"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok()),
        Some("text/event-stream"),
        "streaming endpoint must announce SSE",
    );

    let events = collect_sse(resp.into_body(), BRIEF).await;
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"stdout") && names.contains(&"exit"),
        "stream must include at least one stdout event and a terminal exit; got {names:?}",
    );

    // Reconstruct the stdout payload from the chunks.
    let stdout: String = events
        .iter()
        .filter(|e| e.name == "stdout")
        .map(|e| e.data["chunk"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(stdout, "hello");

    let exit = events.iter().find(|e| e.name == "exit").unwrap();
    assert_eq!(exit.data["exit_status"], 0);
}

#[tokio::test]
async fn exec_stream_returns_404_for_unknown_session() {
    let app = build_app(MockMetadataStore::arc());
    let resp = post(
        app,
        &format!("/api/v1/sessions/{}/exec/stream", SessionId::new()),
        json!({"command": "true"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_stream_returns_409_when_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    let app = build_app(store);
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec/stream"),
        json!({"command": "true"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn exec_stream_chunks_arrive_before_process_exits() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec/stream"),
        // 500ms gap between chunks. If the API buffered to completion
        // we'd never see chunk-1 inside the STREAMING_PROOF budget.
        json!({"command": "printf chunk-1; sleep 0.5; printf chunk-2"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Drain just long enough to catch the first chunk; the test passes
    // if we got a stdout event with chunk-1 before the second printf.
    let early = collect_sse(resp.into_body(), STREAMING_PROOF).await;
    let saw_first_chunk = early
        .iter()
        .any(|e| e.name == "stdout" && e.data["chunk"].as_str() == Some("chunk-1"));
    assert!(
        saw_first_chunk,
        "first chunk must arrive before the second printf — buffering would delay it; got {early:?}",
    );
}

// ---------------------------------------------------------------------
// /sessions/:id/events bus
// ---------------------------------------------------------------------

#[tokio::test]
async fn events_endpoint_returns_404_for_unknown_session() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{}/events", SessionId::new()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_endpoint_announces_sse_content_type() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok()),
        Some("text/event-stream"),
    );
}

#[tokio::test]
async fn events_subscriber_sees_snapshot_lifecycle() {
    // Subscribe first, then trigger snapshot. The subscriber must
    // observe the snapshot_taken event (and not other unrelated events).
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    // Open the SSE subscription. axum's handler runs to completion
    // before returning the Sse, which means by the time we have the
    // body the broadcast subscription is already live — so triggers
    // we send afterwards are observed.
    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub.status(), StatusCode::OK);

    // Drive the subscriber concurrently while we trigger the snapshot.
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    // Tiny pause to make sure the broadcast Receiver is in receive
    // state before we publish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = post(app, &format!("/api/v1/sessions/{id}/snapshot"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let events = collector.await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"snapshot_taken"),
        "subscriber must see snapshot_taken; got {names:?}",
    );
}

#[tokio::test]
async fn events_endpoint_fans_out_to_multiple_subscribers() {
    // Two SSE subscribers on the same session both see an action
    // taken on it.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let sub_a = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let sub_b = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let coll_a = tokio::spawn(async move { collect_sse(sub_a.into_body(), BRIEF).await });
    let coll_b = tokio::spawn(async move { collect_sse(sub_b.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(app, &format!("/api/v1/sessions/{id}/snapshot"), json!({})).await;

    let events_a = coll_a.await.unwrap();
    let events_b = coll_b.await.unwrap();
    for events in [&events_a, &events_b] {
        let saw_snap = events.iter().any(|e| e.name == "snapshot_taken");
        assert!(
            saw_snap,
            "every subscriber must see the snapshot_taken event; got {events:?}",
        );
    }
}

#[tokio::test]
async fn events_subscriber_sees_evict_then_resume_lifecycle() {
    // Drive a full snapshot → evict → resume cycle and observe each
    // lifecycle event in the bus.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    post(app, &format!("/api/v1/sessions/{id}/resume"), json!({})).await;

    let events = collector.await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    // Per design: snapshot keeps Active, evict moves to Idle, resume
    // moves back to Active.
    let expected = ["snapshot_taken", "evicted", "resumed"];
    for needle in expected {
        assert!(
            names.contains(&needle),
            "missing lifecycle event {needle:?} in {names:?}",
        );
    }
    // Order: snapshot before evict before resume.
    let pos = |kind: &str| names.iter().position(|n| *n == kind);
    assert!(pos("snapshot_taken").unwrap() < pos("evicted").unwrap());
    assert!(pos("evicted").unwrap() < pos("resumed").unwrap());
}

#[tokio::test]
async fn exec_via_sync_endpoint_publishes_lifecycle_to_event_bus() {
    // Using the *sync* exec endpoint (POST /sessions/:id/exec) should
    // still route through the event bus so a separate `/events`
    // observer sees the exec lifecycle and stdout chunks.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf wired"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let events = collector.await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"exec_started"), "got {names:?}");
    assert!(names.contains(&"stdout"), "got {names:?}");
    assert!(names.contains(&"exec_completed"), "got {names:?}");

    // The stdout chunk visible to /events matches the command output.
    let stdout: String = events
        .iter()
        .filter(|e| e.name == "stdout")
        .map(|e| e.data["chunk"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(stdout, "wired");
}

#[tokio::test]
async fn snapshot_records_a_snapshot_and_keeps_session_active() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    let resp = post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let snap_id = v["snapshot_id"]
        .as_str()
        .expect("snapshot_id must be present after wiring");
    assert!(!snap_id.is_empty());
    assert!(v["size_bytes"].is_u64());

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
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf still-alive"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "still-alive");
}

#[tokio::test]
async fn snapshot_returns_404_for_unknown_session() {
    let app = build_app(MockMetadataStore::arc());
    let resp = post(
        app,
        &format!("/api/v1/sessions/{}/snapshot", SessionId::new()),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_returns_409_when_session_has_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(SessionSpec {
            image: "r:warm-bootstrap".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .unwrap();
    let app = build_app(store);
    let resp = post(app, &format!("/api/v1/sessions/{id}/snapshot"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn evict_local_requires_a_snapshot_to_exist() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // No snapshot yet: evicting would lose state. Must 409.
    let resp = delete(app, &format!("/api/v1/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Session must still be Active and registry still bound.
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
    );
}

#[tokio::test]
async fn evict_local_after_snapshot_drops_sandbox_and_marks_idle() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // Snapshot first.
    let resp = post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Evict.
    let resp = delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Idle,
    );

    // Phase 4 Track B: an exec on an Idle session transparently
    // auto-resumes from the snapshot before routing the command.
    // The session ends up Active again and the exec succeeds.
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "echo back"}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "auto-resume should bring the Idle session back transparently"
    );
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
        "session is Active after auto-resume",
    );
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "back\n");
}

#[tokio::test]
async fn evict_local_409_when_session_not_active() {
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
    let app = build_app(store);
    let resp = delete(app, &format!("/api/v1/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_409_when_session_not_idle() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;
    // Session is Active — resume only valid from Idle.
    let resp = post(app, &format!("/api/v1/sessions/{id}/resume"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_410_gone_when_no_snapshot_exists() {
    // Set up an Idle session with no SnapshotRecord. resume must
    // return 410 Gone — the session's snapshot is invalidated and
    // engram is a one-shot task runner. The only affordance is
    // `engram session fork <id>` to continue from the workspace.
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
    let app = build_app(store);
    let resp = post(app, &format!("/api/v1/sessions/{id}/resume"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn snapshot_evict_resume_round_trips_workspace_state() {
    // The headline end-to-end test: write a file in a session, snapshot,
    // evict, resume, verify the file is still there in the resumed
    // sandbox. ProcessBackend's tarball path round-trips on-disk state;
    // memory state is by definition not preserved in dev.
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // Write a marker file in the live sandbox.
    let resp = post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "echo persisted > marker"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Snapshot.
    let resp = post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Evict.
    let resp = delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Idle,
    );

    // Resume from snapshot.
    let resp = post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/resume"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Active,
    );

    // The marker file must still be there in the resumed sandbox.
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "cat marker"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["stdout"], "persisted\n",
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
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
    let app = build_app(store);
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"argv": []}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// End-to-end exec wiring (POST /sessions → registry → sandbox.exec)
//
// The registry is in-process state owned by AppState. Tests that need
// create→exec to share state must reuse the *same* `axum::Router`
// across requests (cloning the Router is fine — it shares the
// Arc<AppState> internally).
// ---------------------------------------------------------------------

#[tokio::test]
async fn exec_round_trips_stdout_when_session_has_live_sandbox() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);

    // Hit /sessions then /exec on the *same* router so the registry
    // entry from create is visible to exec. axum's Router clones
    // cheaply — `app.clone()` shares the SharedState (Arc<AppState>),
    // which is exactly what we want here.
    let id = api_create_session(app.clone(), "cortex/api").await;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"command": "printf hello-engram"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["session_id"].as_str().unwrap(), id.to_string());
    assert_eq!(v["exit_status"], 0);
    assert_eq!(v["stdout"], "hello-engram");
    assert_eq!(v["stderr"], "");
}

#[tokio::test]
async fn exec_returns_rusage_with_nonzero_wall_ms_for_slow_command() {
    // The coordinator measures wall-clock around the
    // backend.exec_stream → final Exit window. A `sleep 0.1` is short
    // enough not to slow CI but long enough that wall_ms is reliably
    // ≥ 50ms even on a loaded runner. Backend-supplied fields
    // (peak_rss_kb / *_cpu_ms) stay None for ProcessBackend.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "sleep 0.1"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let wall_ms = v["rusage"]["wall_ms"]
        .as_u64()
        .expect("wall_ms must be present and a u64");
    assert!(
        wall_ms >= 50,
        "wall_ms ({wall_ms}) should reflect the ~100ms sleep",
    );
    // Backend-unsupplied rusage fields are omitted via skip_serializing_if.
    assert!(v["rusage"].get("peak_rss_kb").is_none());
    assert!(v["rusage"].get("user_cpu_ms").is_none());
    assert!(v["rusage"].get("sys_cpu_ms").is_none());
}

#[tokio::test]
async fn exec_stream_includes_rusage_on_exit_event() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec/stream"),
        json!({"command": "sleep 0.1"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let events = collect_sse(resp.into_body(), BRIEF).await;
    let exit = events
        .iter()
        .find(|e| e.name == "exit")
        .expect("stream must end with an `exit` event");
    let wall_ms = exit.data["rusage"]["wall_ms"]
        .as_u64()
        .expect("streaming exit payload must carry rusage.wall_ms");
    assert!(wall_ms >= 50, "wall_ms ({wall_ms}) should reflect ~100ms");
}

#[tokio::test]
async fn exec_propagates_nonzero_exit_status() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"command": "exit 7"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["exit_status"], 7);
}

#[tokio::test]
async fn exec_separates_stdout_and_stderr() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            // Use printf to avoid shells that auto-append trailing
            // newlines differently on macOS vs Linux.
            json!({"command": "printf out; printf err 1>&2"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "out");
    assert_eq!(v["stderr"], "err");
}

#[tokio::test]
async fn exec_with_explicit_argv_skips_shell() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            // argv path: no `sh -c` wrapper, so glob/redirect chars are
            // literal. Verify by passing a string with shell metachars.
            json!({"argv": ["printf", "lit*ral"]}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "lit*ral");
}

#[tokio::test]
async fn exec_returns_404_for_unknown_session() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{unknown}/exec"),
            json!({"command": "true"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_returns_409_when_session_has_no_live_sandbox() {
    // Pre-stage the session row directly in metadata (skipping the
    // create handler) so the registry has no binding. After ADR
    // 0015 M2, a row in `Active` with no sandbox binding models
    // "coord restart lost the in-memory registry"; `ensure_active`
    // passes and the exec handler's own `registry.get` returns 409.
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
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"command": "echo hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("live sandbox"),
        "error message should explain why exec can't run",
    );
}

#[tokio::test]
async fn delete_after_create_unbinds_registry_and_destroys_sandbox() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // Confirm exec works while session is alive.
    let resp = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"command": "printf alive"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete it.
    let resp = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionState::Completed
    );

    // Subsequent exec must fail — the live sandbox is gone.
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/sessions/{id}/exec"),
            json!({"command": "echo nope"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "exec after delete must surface the registry miss",
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
        ..CoordinatorConfig::default()
    };
    let app = api::router(Arc::new(AppState::new(cfg, services)));

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "image": "cortex/api:warm-1",
                "workspace": {"kind":"empty"},
                "harness": {"kind":"none"},
            }),
        ))
        .await
        .unwrap();
    // New contract: scheduling failures bubble as 503, no row is
    // ever written. Caller is expected to retry. (The previous shape
    // marked an orphan Pending→Failed row to leave audit breadcrumbs;
    // we now keep Postgres clean and lean on coord logs for forensics.)
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

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
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_method_on_known_route_returns_405() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(
            Request::put("/api/v1/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf %s \"$ENGRAM_TEST_MARKER\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "from-manifest");
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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Files from the image's rootfs must be visible in the sandbox cwd.
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "cat README.md && cat scripts/setup.sh | head -1"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let stdout = v["stdout"].as_str().unwrap();
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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf %s \"$GITHUB_TOKEN\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "ghp_test_value");
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
    let app = f.app;

    let resp = post(
        app,
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    // Required-but-missing secret is a 500 today (it's an
    // operator-config issue, not a client problem). The error message
    // must call out the secret name so logs are debuggable.
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["message"].as_str().unwrap().contains("GITHUB_TOKEN"),
        "missing-secret error must name the secret",
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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Required is present; optional is silently absent (env var unset).
    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "echo \"required=$GITHUB_TOKEN optional=${OPTIONAL_KEY-MISSING}\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "required=real optional=MISSING\n");
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
    let app = f.app;

    let resp = post(
        app.clone(),
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf %s \"$GITHUB_TOKEN\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    let observed = v["stdout"].as_str().unwrap();
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
            suggested_vcpus = 4
        "#,
    );
    let app = f.app;

    // The hints flow through to the SandboxSpec the backend gets;
    // ProcessBackend doesn't enforce them, but a future Firecracker
    // backend will. Assert the session creation succeeds and reaches
    // an Active state (the hints are well-formed integers).
    let resp = post(
        app,
        "/api/v1/sessions",
        json!({
            "image": "cortex/api:warm-1",
            "workspace": {"kind":"empty"},
            "harness": {"kind":"none"},
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["status"], "active");
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
    let f = TestFixture::new(store, InMemorySecretStore::new());
    let app = f.app;

    let id_a = api_create_session(app.clone(), "warm/test").await;
    let id_b = api_create_session(app.clone(), "warm/test").await;
    assert_ne!(id_a, id_b);

    // Both sessions should be runnable.
    for sid in [id_a, id_b] {
        let resp = post(
            app.clone(),
            &format!("/api/v1/sessions/{sid}/exec"),
            json!({"command": "printf alive"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["stdout"], "alive");
    }
}

#[tokio::test]
async fn engram_session_id_is_injected_per_exec() {
    // ENGRAM_SESSION_ID must be injected at exec time, not baked in
    // at sandbox creation, so each session sees its own id.
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new());
    let app = f.app;

    let id = api_create_session(app.clone(), "warm/test").await;

    let resp = post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf %s \"$ENGRAM_SESSION_ID\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], id.to_string());
}

// ---------------------------------------------------------------------
// Persistent event log + late-join via ?since=N / Last-Event-ID
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_persisted_event_carries_a_monotonic_id_on_the_wire() {
    // Subscribe + drive a quick exec; each SSE message must have an
    // `id:` set to the persistent log's idx, and ids must be strictly
    // increasing within a session.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(
        app,
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf via-bus"}),
    )
    .await;

    let events = collector.await.unwrap();
    let ids: Vec<i64> = events.iter().filter_map(|e| e.id).collect();
    assert!(
        !ids.is_empty(),
        "live events should carry id: fields; got {events:?}",
    );
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "ids must arrive in monotonic order");
    let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "ids must be unique within a session"
    );
}

#[tokio::test]
async fn since_query_replays_history_from_persistent_log() {
    // Drive some activity, then connect to /events?since=-1 to replay
    // everything from the start of the log. The historical events
    // must arrive on the wire even though we connected after they
    // were emitted — the live-only bus would have missed them, the
    // persistent log catches them up.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    // Generate some history while no one is subscribing.
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/resume"),
        json!({}),
    )
    .await;

    // Now subscribe with since=-1 (start of log).
    let sub = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub.status(), StatusCode::OK);

    let events = collect_sse(sub.into_body(), Duration::from_millis(300)).await;
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    // We should see the full lifecycle so far: status_changed (create)
    // + snapshot_taken + evicted + status_changed (Active->Idle) +
    // resumed + status_changed (Idle->Active).
    for needle in ["status_changed", "snapshot_taken", "evicted", "resumed"] {
        assert!(
            names.contains(&needle),
            "?since=-1 must replay {needle}; got {names:?}",
        );
    }
    assert!(
        events.iter().all(|e| e.id.is_some()),
        "every replayed event must carry an id",
    );
}

#[tokio::test]
async fn since_query_skips_events_already_seen() {
    // Hold a checkpoint after some activity. Reconnect with since=cp
    // and verify we see only the events strictly after cp.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    // First batch.
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;

    // Read everything-so-far to find the checkpoint idx.
    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_events = collect_sse(baseline.into_body(), Duration::from_millis(300)).await;
    let checkpoint = baseline_events
        .iter()
        .filter_map(|e| e.id)
        .max()
        .expect("baseline replay produced at least one event");

    // Second batch — this is what we want to see in the next replay.
    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;

    // Reconnect with since=checkpoint. Should NOT see anything from
    // the first batch.
    let resume = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since={checkpoint}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let after = collect_sse(resume.into_body(), Duration::from_millis(300)).await;
    assert!(
        !after.is_empty(),
        "reconnect at checkpoint {checkpoint} should still replay the second batch",
    );
    for ev in &after {
        let id = ev.id.expect("replay events carry id");
        assert!(
            id > checkpoint,
            "since={checkpoint} must skip everything up to and including that idx; saw idx={id}",
        );
    }
    let names: Vec<&str> = after.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"evicted"),
        "second-batch event missing: {names:?}"
    );
}

#[tokio::test]
async fn last_event_id_header_drives_reconnect() {
    // Same as the `since=` test but using the EventSource-style
    // header. Documented spec: Browser EventSource sends the last
    // received id back automatically on reconnect via this header.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_events = collect_sse(baseline.into_body(), Duration::from_millis(300)).await;
    let checkpoint = baseline_events.iter().filter_map(|e| e.id).max().unwrap();

    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;

    let resume = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events"))
                .header("last-event-id", checkpoint.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let after = collect_sse(resume.into_body(), Duration::from_millis(300)).await;
    for ev in &after {
        assert!(ev.id.unwrap() > checkpoint);
    }
}

#[tokio::test]
async fn since_query_wins_when_higher_than_last_event_id_header() {
    // If both ?since= and Last-Event-ID are present, we use the higher
    // one — protects against accidental rewinds across reconnects.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;

    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_events = collect_sse(baseline.into_body(), Duration::from_millis(300)).await;
    let high = baseline_events.iter().filter_map(|e| e.id).max().unwrap();

    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;

    // Stale header (way back), fresh query (current high water).
    let resume = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since={high}"))
                .header("last-event-id", "-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let after = collect_sse(resume.into_body(), Duration::from_millis(300)).await;
    for ev in &after {
        assert!(
            ev.id.unwrap() > high,
            "?since={high} must win over Last-Event-ID=-1",
        );
    }
}

#[tokio::test]
async fn replay_then_live_seam_is_gap_free_and_dup_free() {
    // The hard test: subscribe with ?since=N, while live events are
    // arriving. The seam between the replayed log and the live tail
    // must not duplicate or skip any idx.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    // Drive some history.
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;

    // Subscribe at since=-1 and concurrently drive more activity.
    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    post(app, &format!("/api/v1/sessions/{id}/resume"), json!({})).await;

    let events = collector.await.unwrap();
    let ids: Vec<i64> = events.iter().filter_map(|e| e.id).collect();
    assert!(!ids.is_empty());

    // Strict monotonicity (no duplicates, no out-of-order).
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        ids, sorted,
        "ids across the replay→live seam must be strictly increasing with no duplicates",
    );

    // No gaps within the run we observe (idx may not start at 0
    // because the test fixture's MockMetadataStore allocates idx 0
    // for the create's status_changed event, which we DO see at
    // since=-1).
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
    // Defense in depth: drive every kind of event the coordinator
    // emits and verify each appears in the persistent log via
    // ?since=-1 replay. If a future change forgets to call emit() in
    // some handler, this test catches it.
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    // exec → exec_started, stdout, exec_completed
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/exec"),
        json!({"command": "printf hi; printf err 1>&2"}),
    )
    .await;
    // snapshot → snapshot_taken
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/snapshot"),
        json!({}),
    )
    .await;
    // evict → evicted + status_changed
    delete(app.clone(), &format!("/api/v1/sessions/{id}/local")).await;
    // resume → resumed + status_changed
    post(
        app.clone(),
        &format!("/api/v1/sessions/{id}/resume"),
        json!({}),
    )
    .await;

    let log = app
        .oneshot(
            Request::get(format!("/api/v1/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let events = collect_sse(log.into_body(), Duration::from_millis(300)).await;
    let kinds: std::collections::HashSet<&str> = events.iter().map(|e| e.name.as_str()).collect();
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
// ADR 0005 retired the checkpoint / fork / diff / log?kind=workspace
// endpoints along with the platform's git surface. The deleted routes
// now return 404 because they're no longer registered.
// ---------------------------------------------------------------------

#[tokio::test]
async fn deleted_git_endpoints_return_404() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;

    for path in [
        format!("/api/v1/sessions/{id}/checkpoint"),
        format!("/api/v1/sessions/{id}/fork"),
    ] {
        let resp = post(app.clone(), &path, json!({})).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} must be retired"
        );
    }

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/v1/sessions/{id}/diff"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "diff endpoint must be retired"
    );

    // log?kind=workspace returns 400 (the route still exists, but
    // only kind=conversation is supported now).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/v1/sessions/{id}/log?kind=workspace"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "log?kind=workspace must be rejected"
    );
}

// ---------------------------------------------------------------------
// ADR 0007 — POST /api/admin/reap-materialize-dir
// ---------------------------------------------------------------------

#[tokio::test]
async fn admin_reap_materialize_dir_reports_host_not_in_grpc_pool() {
    // ADR 0013: the fanout dispatches through `state.services.host_pool`.
    // The default TestFixture registers an in-proc ProcessBackend via
    // `register()` but doesn't populate the pool (no HTTP /register
    // call in tests). Hosts not in the pool surface a per-host error
    // explaining the skip — same "graceful skip" contract as the
    // old admin_client path, just keyed on the new pool.
    let store = MockMetadataStore::arc();
    let app = build_app(store);

    let resp = post(app, "/api/v1/admin/reap-materialize-dir", json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["files_deleted"], 0);
    assert_eq!(v["bytes_freed"], 0);
    let per_host = v["per_host"]
        .as_array()
        .expect("per_host array must be present in coordinator mode");
    assert!(!per_host.is_empty(), "test fixture registers one host");
    let entry = &per_host[0];
    assert!(entry["stats"].is_null(), "no stats when host was skipped");
    let err = entry["error"]
        .as_str()
        .expect("not in gRPC pool → per-host error string");
    assert!(
        err.contains("not yet registered") || err.contains("gRPC pool"),
        "error must explain the skip reason, got: {err}",
    );
}

#[tokio::test]
async fn admin_reap_materialize_dir_deletes_orphan_and_keeps_live() {
    // End-to-end: stand up an app with a real `materialize_dir`,
    // plant two `.ext4` files (one for a live manifest, one for
    // an orphan), fire the endpoint, assert the orphan is gone.
    // The `MockMetadataStore` doesn't override
    // `list_live_disk_manifest_ids`, so the default Vec::new()
    // returns the empty set — every file on disk is treated as
    // orphan. That's fine for this test: we just need to prove
    // the endpoint actually scans + deletes + reports stats.
    use std::path::PathBuf;

    let store = MockMetadataStore::arc();
    let materialize_dir: PathBuf = tempfile::tempdir().expect("materialize tempdir").keep();

    // Plant two ext4 files matching the `<uuid>-v<num>.ext4` shape.
    let orphan_id = uuid::Uuid::new_v4();
    let orphan_path = materialize_dir.join(format!("{orphan_id}-v1.ext4"));
    std::fs::write(&orphan_path, b"orphan-bytes").unwrap();
    let live_id = uuid::Uuid::new_v4();
    let live_path = materialize_dir.join(format!("{live_id}-v3.ext4"));
    std::fs::write(&live_path, b"live-bytes-larger-payload").unwrap();

    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta: store,
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
        materialize_dir: Some(materialize_dir.clone()),
    };
    let cfg = engram_coordinator::CoordinatorConfig::default();
    let state = Arc::new(engram_coordinator::AppState::new(cfg, services));
    let app = engram_coordinator::api::router(state);

    // min_age_secs=0 lets the freshly-written files be deletable.
    // Without this override the 1h default would skip them all.
    let resp = post(
        app,
        "/api/v1/admin/reap-materialize-dir?min_age_secs=0",
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let v = body_json(resp.into_body()).await;
    assert_eq!(v["files_scanned"], 2);
    // Empty live set → both files are orphans.
    assert_eq!(v["files_deleted"], 2, "body: {v:?}");
    assert!(v["bytes_freed"].as_u64().unwrap() > 0);
    assert!(!orphan_path.exists());
    assert!(!live_path.exists());
}

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

#[tokio::test]
async fn flush_now_returns_404_when_session_unknown() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = post(
        app,
        &format!("/api/v1/admin/sessions/{unknown}/flush-now"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn flush_now_returns_409_when_session_has_no_bound_sandbox() {
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
    let app = build_app(meta);
    let resp = post(
        app,
        &format!("/api/v1/admin/sessions/{session_id}/flush-now"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn flush_now_returns_idle_when_host_has_no_dirty_bytes() {
    let meta = MockMetadataStore::arc();
    let session_id = SessionId::new();
    // Bound to a sandbox the local ProcessBackend doesn't know about
    // — `flush_sandbox`'s trait default returns Ok(None), which the
    // endpoint maps to `outcome: idle`.
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
    let app = build_app(meta.clone());
    let resp = post(
        app,
        &format!("/api/v1/admin/sessions/{session_id}/flush-now"),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["outcome"], "idle");
    assert!(body.get("manifest_version").is_none() || body["manifest_version"].is_null());
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
