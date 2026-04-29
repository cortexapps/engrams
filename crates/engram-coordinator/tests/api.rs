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
use engram_coordinator::image_registry::ImageRegistry;
use engram_coordinator::{api, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::{
    HostRecord, HostStatus, ImageStatus, ImageVersion, PersistedEvent, Session, SessionSpec,
    SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, ImageVersionId, MetaError, SessionId};
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
    images: Mutex<HashMap<String, Vec<ImageVersion>>>,
    events: Mutex<HashMap<SessionId, Vec<PersistedEvent>>>,
    next_event_idx: Mutex<HashMap<SessionId, i64>>,
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

    fn add_image(&self, repo: &str, tag: &str, status: ImageStatus) {
        self.images
            .lock()
            .entry(repo.to_string())
            .or_default()
            .push(ImageVersion {
                id: ImageVersionId::new(),
                repo: repo.into(),
                tag: tag.into(),
                blob_url: None,
                status,
                created_at: Utc::now(),
            });
    }
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
    async fn create_session(
        &self,
        spec: SessionSpec,
        image_version: String,
    ) -> Result<SessionId, MetaError> {
        let id = SessionId::new();
        let session = Session {
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
        };
        self.sessions.lock().insert(id, session);
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
        Ok(self
            .sessions
            .lock()
            .values()
            .filter(|s| {
                matches!(
                    s.status,
                    SessionStatus::Pending | SessionStatus::Active | SessionStatus::Idle
                )
            })
            .cloned()
            .collect())
    }

    async fn set_session_status(
        &self,
        id: SessionId,
        status: SessionStatus,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.status = status;
        s.last_active_at = Utc::now();
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

    async fn upsert_host(&self, _host: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }

    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }

    async fn set_host_status(&self, _id: HostId, _status: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }

    async fn list_stale_hosts(&self, _threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        // Mock doesn't track heartbeat timestamps; existing tests
        // don't exercise the dead-host detector path.
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

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
        self.snapshots
            .lock()
            .entry(snap.session_id)
            .or_default()
            .push(snap);
        Ok(())
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

    async fn upsert_image_version(&self, version: ImageVersion) -> Result<(), MetaError> {
        self.images
            .lock()
            .entry(version.repo.clone())
            .or_default()
            .push(version);
        Ok(())
    }

    async fn latest_ready_image(&self, repo: &str) -> Result<Option<ImageVersion>, MetaError> {
        Ok(self.images.lock().get(repo).and_then(|v| {
            v.iter()
                .rev()
                .find(|i| matches!(i.status, ImageStatus::Ready))
                .cloned()
        }))
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
}

// ---------------------------------------------------------------------
// Fixture: build a fully-wired axum router against in-memory components.
// ---------------------------------------------------------------------

fn build_app(meta: Arc<MockMetadataStore>) -> axum::Router {
    let f = TestFixture::new(meta, InMemorySecretStore::new(), 0);
    f.app
}

/// Like `build_app` but seeds the bearer-token allow-list. Used by the
/// auth middleware tests; everything else relies on the default empty
/// list (auth-disabled).
fn build_app_with_tokens(meta: Arc<MockMetadataStore>, tokens: Vec<String>) -> axum::Router {
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let images_dir = tempfile::tempdir().expect("images tempdir").keep();
    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: Arc::new(ProcessBackend::new(sandbox_dir)),
        secrets: Arc::new(InMemorySecretStore::new()),
        images: ImageRegistry::new(images_dir),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        default_warm_pool_size: 0,
        auth_tokens: tokens,
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new(cfg, services));
    api::router(state)
}

/// Test fixture exposing the on-disk paths and stores so individual
/// tests can populate images/secrets before exercising the API.
/// The default `build_app` discards these (most tests don't care).
struct TestFixture {
    app: axum::Router,
    images_dir: std::path::PathBuf,
}

impl TestFixture {
    fn new(
        meta: Arc<MockMetadataStore>,
        secrets: InMemorySecretStore,
        warm_pool_size: u32,
    ) -> Self {
        // Separate tempdirs for each on-disk component so they can't
        // accidentally collide. All leak (`keep()`) because the axum
        // router needs to outlive this function — the OS cleans up
        // `/tmp` later.
        let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
        let images_dir = tempfile::tempdir().expect("images tempdir").keep();
        // Match production wiring: the host-side PooledBackend wraps
        // the real backend so warm-pool semantics (checkout / configure /
        // replenish) work the same way the multi-host setup runs them.
        // `warm_pool_size = 0` skips pooling entirely (every session
        // takes the cold path).
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_dir));
        let backend: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(
            engram_host_agent::pooled_backend::PooledBackend::new(raw, warm_pool_size),
        );
        let services = Services {
            meta,
            cloud: Arc::new(MockCloud::new()),
            sandbox: backend,
            secrets: Arc::new(secrets),
            images: ImageRegistry::new(images_dir.clone()),
        };
        let cfg = CoordinatorConfig {
            default_image_version: "warm-bootstrap".into(),
            default_warm_pool_size: warm_pool_size,
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new(cfg, services));
        Self {
            app: api::router(state),
            images_dir,
        }
    }

    /// Materialize an image at `<images_dir>/<repo>/<tag>` with the
    /// given manifest TOML and an optional rootfs directory tree
    /// (each entry is `(relative_path, contents)`).
    fn write_image(&self, repo: &str, tag: &str, manifest_toml: &str, rootfs: &[(&str, &[u8])]) {
        let img_dir = self.images_dir.join(repo).join(tag);
        std::fs::create_dir_all(&img_dir).unwrap();
        std::fs::write(img_dir.join("manifest.toml"), manifest_toml).unwrap();
        if !rootfs.is_empty() {
            let rootfs_dir = img_dir.join("rootfs");
            std::fs::create_dir_all(&rootfs_dir).unwrap();
            for (path, contents) in rootfs {
                let full = rootfs_dir.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(full, contents).unwrap();
            }
        }
    }
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
/// fresh clone of the router for any subsequent request.
async fn api_create_session(app: axum::Router, repo: &str) -> SessionId {
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": repo, "branch": "main"}),
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
        .oneshot(Request::get("/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_rejects_request_without_authorization_header() {
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(Request::get("/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::get("/sessions")
                .header("authorization", "Bearer beta")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "unauthorized");
    assert!(v["message"].as_str().unwrap().contains("invalid"));
}

#[tokio::test]
async fn auth_rejects_non_bearer_scheme() {
    // Other schemes (Basic, Digest, ...) aren't supported. Mismatched
    // scheme is a misconfigured client; surface 401, not a confusing
    // 200 from a permissive header parser.
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::get("/sessions")
                .header("authorization", "Basic YWxwaGE=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_accepts_valid_bearer_token() {
    let app = build_app_with_tokens(
        MockMetadataStore::arc(),
        vec!["alpha".into(), "beta".into()],
    );
    for token in ["alpha", "beta"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get("/sessions")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "token {token} must pass");
    }
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
async fn auth_protects_post_endpoints_too() {
    // The middleware applies to every method on every protected path,
    // not just GETs. POST /sessions without a token must 401 before
    // the body is parsed.
    let app = build_app_with_tokens(MockMetadataStore::arc(), vec!["alpha".into()]);
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "r", "branch": "main"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
async fn create_session_requires_repo_and_branch() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "", "branch": "main"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "bad_request");
    assert!(
        v["message"].as_str().unwrap().contains("repo"),
        "error message must call out the missing field"
    );
}

#[tokio::test]
async fn create_session_uses_default_image_when_none_ready() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "cortex/api", "branch": "main"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["image_version"], "warm-bootstrap");
    // Once sandbox creation is wired, a successful create transitions
    // the session past Pending into Active.
    assert_eq!(v["status"], "active");
    let id_str = v["session_id"].as_str().unwrap();

    // Round-trip GET /sessions/:id should match what we created.
    let id: SessionId = id_str.parse().unwrap();
    let session = store.get_session(id).await.unwrap();
    assert_eq!(session.repo, "cortex/api");
    assert_eq!(session.branch, "main");
    assert_eq!(session.image_version, "warm-bootstrap");
    assert_eq!(session.status, SessionStatus::Active);
}

#[tokio::test]
async fn create_session_prefers_latest_ready_image() {
    let store = MockMetadataStore::arc();
    // A retired older image must NOT be picked up; only Ready counts.
    store.add_image("cortex/api", "warm-old", ImageStatus::Retired);
    store.add_image("cortex/api", "warm-2026-04", ImageStatus::Ready);
    store.add_image("cortex/api", "warm-2026-05", ImageStatus::Ready);
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "cortex/api", "branch": "main"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["image_version"], "warm-2026-05",
        "latest Ready image must win over older Ready and Retired ones"
    );
}

#[tokio::test]
async fn create_session_honors_explicit_image_version() {
    let store = MockMetadataStore::arc();
    store.add_image("cortex/api", "warm-current", ImageStatus::Ready);
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-pinned"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["image_version"], "warm-pinned",
        "client-supplied image_version overrides the latest_ready lookup"
    );
}

#[tokio::test]
async fn get_session_returns_404_for_unknown_id() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = app
        .oneshot(
            Request::get(format!("/sessions/{unknown}"))
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
            Request::get("/sessions/not-a-uuid")
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
        .oneshot(Request::get("/sessions").body(Body::empty()).unwrap())
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
async fn list_sessions_returns_pending_active_and_idle_only() {
    // The endpoint mirrors `list_active_sessions` semantics: pending /
    // active / idle rows show up; completed / failed / evicted rows
    // are filtered out. Filtering at the surface keeps the default
    // `engram session list` view focused on live work.
    let store = MockMetadataStore::arc();

    async fn mk(store: &MockMetadataStore, repo: &str) -> SessionId {
        store
            .create_session(
                SessionSpec {
                    repo: repo.into(),
                    branch: "main".into(),
                    user_id: None,
                    image_version: None,
                    read_only: false,
                },
                "warm-bootstrap".into(),
            )
            .await
            .unwrap()
    }
    let active_id = mk(&store, "alive").await;
    store
        .set_session_status(active_id, SessionStatus::Active)
        .await
        .unwrap();
    let idle_id = mk(&store, "idle-too").await;
    store
        .set_session_status(idle_id, SessionStatus::Idle)
        .await
        .unwrap();
    let dead_id = mk(&store, "done").await;
    store
        .set_session_status(dead_id, SessionStatus::Completed)
        .await
        .unwrap();

    let app = build_app(store);
    let resp = app
        .oneshot(Request::get("/sessions").body(Body::empty()).unwrap())
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
        .create_session(
            SessionSpec {
                repo: "cortex/api".into(),
                branch: "trunk".into(),
                user_id: Some("user-42".into()),
                image_version: None,
                read_only: false,
            },
            "warm-2026-04-27".into(),
        )
        .await
        .unwrap();

    let app = build_app(store);
    let resp = app
        .oneshot(Request::get("/sessions").body(Body::empty()).unwrap())
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
    assert_eq!(item["repo"], "cortex/api");
    assert_eq!(item["branch"], "trunk");
    assert_eq!(item["user_id"], "user-42");
    assert_eq!(item["image_version"], "warm-2026-04-27");
    assert_eq!(item["status"], SessionStatus::Pending.as_str());
    assert!(item["created_at"].is_string());
    assert!(item["last_active_at"].is_string());
}

#[tokio::test]
async fn delete_session_marks_completed_and_returns_204() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();

    let app = build_app(store.clone());
    let resp = app
        .oneshot(
            Request::delete(format!("/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after = store.get_session(id).await.unwrap();
    assert_eq!(after.status, SessionStatus::Completed);
}

#[tokio::test]
async fn delete_session_404_for_unknown_id() {
    let app = build_app(MockMetadataStore::arc());
    let unknown = SessionId::new();
    let resp = app
        .oneshot(
            Request::delete(format!("/sessions/{unknown}"))
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
        &format!("/sessions/{id}/exec/stream"),
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
        &format!("/sessions/{}/exec/stream", SessionId::new()),
        json!({"command": "true"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_stream_returns_409_when_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    let app = build_app(store);
    let resp = post(
        app,
        &format!("/sessions/{id}/exec/stream"),
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
        &format!("/sessions/{id}/exec/stream"),
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
            Request::get(format!("/sessions/{}/events", SessionId::new()))
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
            Request::get(format!("/sessions/{id}/events"))
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
            Request::get(format!("/sessions/{id}/events"))
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

    let resp = post(app, &format!("/sessions/{id}/snapshot"), json!({})).await;
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
            Request::get(format!("/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let sub_b = app
        .clone()
        .oneshot(
            Request::get(format!("/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let coll_a = tokio::spawn(async move { collect_sse(sub_a.into_body(), BRIEF).await });
    let coll_b = tokio::spawn(async move { collect_sse(sub_b.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(app, &format!("/sessions/{id}/snapshot"), json!({})).await;

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
            Request::get(format!("/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    delete(app.clone(), &format!("/sessions/{id}/local")).await;
    post(app, &format!("/sessions/{id}/resume"), json!({})).await;

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
            Request::get(format!("/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
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

    let resp = post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
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
        SessionStatus::Active,
        "snapshot must NOT change session status — eviction is a separate call",
    );

    // Metadata received the SnapshotRecord we wrote.
    let recorded = store.list_snapshots_for_session(id).await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].local_path.is_some());

    // After snapshot the live sandbox should still respond to exec.
    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
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
        &format!("/sessions/{}/snapshot", SessionId::new()),
        json!({}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn snapshot_returns_409_when_session_has_no_live_sandbox() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    let app = build_app(store);
    let resp = post(app, &format!("/sessions/{id}/snapshot"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn evict_local_requires_a_snapshot_to_exist() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // No snapshot yet: evicting would lose state. Must 409.
    let resp = delete(app, &format!("/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Session must still be Active and registry still bound.
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionStatus::Active,
    );
}

#[tokio::test]
async fn evict_local_after_snapshot_drops_sandbox_and_marks_idle() {
    let store = MockMetadataStore::arc();
    let app = build_app(store.clone());
    let id = api_create_session(app.clone(), "r").await;

    // Snapshot first.
    let resp = post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Evict.
    let resp = delete(app.clone(), &format!("/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionStatus::Idle,
    );

    // Exec must now 409 — the live sandbox is gone.
    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
        json!({"command": "echo nope"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn evict_local_409_when_session_not_active() {
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    // Session is Pending (never created sandbox), can't evict.
    let app = build_app(store);
    let resp = delete(app, &format!("/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_409_when_session_not_idle() {
    let store = MockMetadataStore::arc();
    let app = build_app(store);
    let id = api_create_session(app.clone(), "r").await;
    // Session is Active — resume only valid from Idle.
    let resp = post(app, &format!("/sessions/{id}/resume"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_409_when_no_snapshot_exists() {
    // Set up an Idle session with no SnapshotRecord. resume must 409
    // rather than restore from nothing.
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    store
        .set_session_status(id, SessionStatus::Idle)
        .await
        .unwrap();
    let app = build_app(store);
    let resp = post(app, &format!("/sessions/{id}/resume"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
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
        &format!("/sessions/{id}/exec"),
        json!({"command": "echo persisted > marker"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Snapshot.
    let resp = post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Evict.
    let resp = delete(app.clone(), &format!("/sessions/{id}/local")).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionStatus::Idle,
    );

    // Resume from snapshot.
    let resp = post(app.clone(), &format!("/sessions/{id}/resume"), json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionStatus::Active,
    );

    // The marker file must still be there in the resumed sandbox.
    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
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
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/sessions/{id}/exec"),
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
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    let app = build_app(store);
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/sessions/{id}/exec"),
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
            &format!("/sessions/{id}/exec"),
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
        &format!("/sessions/{id}/exec"),
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
        &format!("/sessions/{id}/exec/stream"),
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
            &format!("/sessions/{id}/exec"),
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
            &format!("/sessions/{id}/exec"),
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
            &format!("/sessions/{id}/exec"),
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
            &format!("/sessions/{unknown}/exec"),
            json!({"command": "true"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_returns_409_when_session_has_no_live_sandbox() {
    // Pre-stage the session row directly in metadata (skipping the
    // create handler) so the registry has no binding. This models
    // either: a coordinator restart that lost the registry, or a
    // session that was snapshotted and not yet resumed.
    let store = MockMetadataStore::arc();
    let id = store
        .create_session(
            SessionSpec {
                repo: "r".into(),
                branch: "main".into(),
                user_id: None,
                image_version: None,
                read_only: false,
            },
            "warm-bootstrap".into(),
        )
        .await
        .unwrap();
    let app = build_app(store);

    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/sessions/{id}/exec"),
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
            &format!("/sessions/{id}/exec"),
            json!({"command": "printf alive"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete it.
    let resp = app
        .clone()
        .oneshot(
            Request::delete(format!("/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        store.get_session(id).await.unwrap().status,
        SessionStatus::Completed
    );

    // Subsequent exec must fail — the live sandbox is gone.
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/sessions/{id}/exec"),
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
async fn create_session_failure_marks_session_failed() {
    // Build an app whose sandbox backend always errors on create.
    use std::path::Path;
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
            _dest: &Path,
        ) -> Result<engram_core::types::SnapshotMetadata, engram_core::SandboxError> {
            unreachable!()
        }
        async fn restore(
            &self,
            _src: std::path::PathBuf,
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
    let images_dir = tempfile::tempdir().unwrap();
    let services = Services {
        meta: store.clone(),
        cloud: Arc::new(MockCloud::new()),
        sandbox: Arc::new(AlwaysFailSandbox),
        secrets: Arc::new(InMemorySecretStore::new()),
        images: ImageRegistry::new(images_dir.keep()),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        ..CoordinatorConfig::default()
    };
    let app = api::router(Arc::new(AppState::new(cfg, services)));

    let resp = app
        .oneshot(json_request(
            Method::POST,
            "/sessions",
            json!({"repo": "cortex/api", "branch": "main"}),
        ))
        .await
        .unwrap();
    // SandboxError::LimitExceeded → ApiError::Internal → 500.
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // The metadata row exists and is marked Failed so operators can
    // see *which* sessions the backend rejected.
    let active = store.list_active_sessions().await.unwrap();
    assert!(
        active.is_empty(),
        "failed sessions must not appear in the active list",
    );
    // Find the failed session by listing all sessions in the mock.
    let sessions = store.all_sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, SessionStatus::Failed);
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
        .oneshot(Request::put("/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------------
// Image manifest + secret resolution + rootfs materialization
// ---------------------------------------------------------------------

#[tokio::test]
async fn session_with_no_image_falls_through_to_empty_workdir() {
    // Bare-bones dev demo: no manifest exists for the requested
    // (repo, tag), session creation must still work.
    let app = build_app(MockMetadataStore::arc());
    let id = api_create_session(app.clone(), "no/image").await;

    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
        json!({"command": "ls -A | wc -l | tr -d ' '"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "0\n", "no rootfs_source → empty cwd");
}

#[tokio::test]
async fn manifest_env_lands_in_sandbox_environment() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new(), 0);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [env]
            PYTHONUNBUFFERED = "1"
            ENGRAM_TEST_MARKER = "from-manifest"
        "#,
        &[],
    );
    let app = f.app;

    let resp = post(
        app.clone(),
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
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
        &format!("/sessions/{id}/exec"),
        json!({"command": "printf %s \"$ENGRAM_TEST_MARKER\""}),
    )
    .await;
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["stdout"], "from-manifest");
}

#[tokio::test]
async fn rootfs_directory_is_materialized_into_sandbox_cwd() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new(), 0);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"name = "cortex-api""#,
        &[
            ("README.md", b"# starter\n"),
            ("scripts/setup.sh", b"#!/bin/sh\necho ok\n"),
        ],
    );
    let app = f.app;

    let resp = post(
        app.clone(),
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
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
        &format!("/sessions/{id}/exec"),
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
    let f = TestFixture::new(store, secrets, 0);
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
        &[],
    );
    let app = f.app;

    let resp = post(
        app.clone(),
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
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
        &format!("/sessions/{id}/exec"),
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
    let f = TestFixture::new(store, secrets, 0);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true
        "#,
        &[],
    );
    let app = f.app;

    let resp = post(
        app,
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
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
    let f = TestFixture::new(store, secrets, 0);
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
        &[],
    );
    let app = f.app;

    let resp = post(
        app.clone(),
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
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
        &format!("/sessions/{id}/exec"),
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
    let f = TestFixture::new(store, secrets, 0);
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
        &[],
    );
    let app = f.app;

    let resp = post(
        app.clone(),
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
    )
    .await;
    let id: SessionId = body_json(resp.into_body()).await["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
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
    let f = TestFixture::new(store, InMemorySecretStore::new(), 0);
    f.write_image(
        "cortex/api",
        "warm-1",
        r#"
            name = "cortex-api"
            [resources]
            suggested_memory_mib = 8192
            suggested_vcpus = 4
        "#,
        &[],
    );
    let app = f.app;

    // The hints flow through to the SandboxSpec the backend gets;
    // ProcessBackend doesn't enforce them, but a future Firecracker
    // backend will. Assert the session creation succeeds and reaches
    // an Active state (the hints are well-formed integers).
    let resp = post(
        app,
        "/sessions",
        json!({"repo": "cortex/api", "branch": "main", "image_version": "warm-1"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["status"], "active");
}

// ---------------------------------------------------------------------
// Warm pool — checkout, replenish, fallback
// ---------------------------------------------------------------------

#[tokio::test]
async fn first_session_uses_cold_path_then_pool_warms() {
    // With warm_pool_size=1 and no pre-warmed sandbox, the first
    // session creates synchronously. After that request returns, a
    // background replenish brings the pool to 1. The second session
    // *should* check out the warm one.
    //
    // We can't observe directly from the API which path was taken —
    // both produce the same SessionStatus::Active response. So we
    // measure latency: warm checkout is sub-ms (pool.checkout +
    // existing sandbox); cold create involves backend.create which
    // even for ProcessBackend is mkdir + rootfs materialization.
    //
    // For ProcessBackend on Mac this is a few hundred microseconds
    // either way, so the timing difference isn't reliable in CI.
    // Instead we inspect Pool state directly via the AppState — but
    // the integration test doesn't have access to it. So this test
    // just asserts that two consecutive create calls succeed and
    // produce distinct sessions. The pool plumbing is exercised; its
    // unit tests in `engram-host-agent::pool` cover the actual
    // warm/cold branching.
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new(), 1);
    let app = f.app;

    let id_a = api_create_session(app.clone(), "warm/test").await;
    // Give the background replenish a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let id_b = api_create_session(app.clone(), "warm/test").await;
    assert_ne!(id_a, id_b);

    // Both sessions should be runnable.
    for sid in [id_a, id_b] {
        let resp = post(
            app.clone(),
            &format!("/sessions/{sid}/exec"),
            json!({"command": "printf alive"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["stdout"], "alive");
    }
}

#[tokio::test]
async fn engram_session_id_is_injected_per_exec_not_baked_into_pool() {
    // The pool serves anonymous sandboxes; ENGRAM_SESSION_ID must be
    // injected at exec time so each session sees its own id even
    // when the sandbox came from the warm pool.
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store, InMemorySecretStore::new(), 1);
    let app = f.app;

    let id = api_create_session(app.clone(), "warm/test").await;

    let resp = post(
        app,
        &format!("/sessions/{id}/exec"),
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
            Request::get(format!("/sessions/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    post(
        app,
        &format!("/sessions/{id}/exec"),
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
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    delete(app.clone(), &format!("/sessions/{id}/local")).await;
    post(app.clone(), &format!("/sessions/{id}/resume"), json!({})).await;

    // Now subscribe with since=-1 (start of log).
    let sub = app
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
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
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;

    // Read everything-so-far to find the checkpoint idx.
    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
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
    delete(app.clone(), &format!("/sessions/{id}/local")).await;

    // Reconnect with since=checkpoint. Should NOT see anything from
    // the first batch.
    let resume = app
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since={checkpoint}"))
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

    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_events = collect_sse(baseline.into_body(), Duration::from_millis(300)).await;
    let checkpoint = baseline_events.iter().filter_map(|e| e.id).max().unwrap();

    delete(app.clone(), &format!("/sessions/{id}/local")).await;

    let resume = app
        .oneshot(
            Request::get(format!("/sessions/{id}/events"))
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
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;

    let baseline = app
        .clone()
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_events = collect_sse(baseline.into_body(), Duration::from_millis(300)).await;
    let high = baseline_events.iter().filter_map(|e| e.id).max().unwrap();

    delete(app.clone(), &format!("/sessions/{id}/local")).await;

    // Stale header (way back), fresh query (current high water).
    let resume = app
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since={high}"))
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
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;

    // Subscribe at since=-1 and concurrently drive more activity.
    let sub = app
        .clone()
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let collector = tokio::spawn(async move { collect_sse(sub.into_body(), BRIEF).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    delete(app.clone(), &format!("/sessions/{id}/local")).await;
    post(app, &format!("/sessions/{id}/resume"), json!({})).await;

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
        &format!("/sessions/{id}/exec"),
        json!({"command": "printf hi; printf err 1>&2"}),
    )
    .await;
    // snapshot → snapshot_taken
    post(app.clone(), &format!("/sessions/{id}/snapshot"), json!({})).await;
    // evict → evicted + status_changed
    delete(app.clone(), &format!("/sessions/{id}/local")).await;
    // resume → resumed + status_changed
    post(app.clone(), &format!("/sessions/{id}/resume"), json!({})).await;

    let log = app
        .oneshot(
            Request::get(format!("/sessions/{id}/events?since=-1"))
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
