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
    /// ADR 0047: placement reads host rows now — the fixture seeds the
    /// test host here and `mark_host_ready_for` mutates its
    /// `ready_images`.
    hosts: Mutex<HashMap<HostId, HostRecord>>,
    /// Issue #213: simulate the per-session lease being held by another
    /// holder (a concurrent eviction / resume / live migration). When
    /// set, `try_acquire_session_lease` returns `false`, so the
    /// lease-acquiring handlers (snapshot, evict_local) must back off and
    /// refuse rather than mutate. Default `false` = the lease is free
    /// (preserves the existing tests' behaviour).
    lease_held: std::sync::atomic::AtomicBool,
    /// Issue #231: simulate `touch_host_heartbeat` failing (a saturated
    /// coord PG pool). When set, the per-heartbeat persist returns an
    /// error so the test can assert the handler now returns 5xx (and
    /// no longer swallows the failure into a 200) — the regression that
    /// staled a live host's `last_heartbeat_at` and orphaned its
    /// sessions. Default `false` = persist succeeds.
    heartbeat_persist_fails: std::sync::atomic::AtomicBool,
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
        if self
            .heartbeat_persist_fails
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // Issue #231: simulate a saturated PG pool dropping the
            // per-tick persist.
            return Err(MetaError::Db(Box::new(std::io::Error::other(
                "simulated pool saturation",
            ))));
        }
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
            // Mirror the PG impl's UPSERT-by-id contract: re-recording the
            // same snapshot id (e.g. issue #213's recoverable=false → flip
            // to true after commit_snapshot) UPDATES the existing row in
            // place rather than appending a duplicate. A blind push here
            // would let the test see two rows for one snapshot and miss the
            // promotion semantics.
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

    // Issue #213: the lease is the serializer the snapshot / evict_local
    // handlers now acquire. `lease_held` lets a test pin it "held by
    // another holder" so those handlers must back off (Conflict) instead
    // of mutating. Default (false) returns `true` like the trait default,
    // so every other test acquires freely.
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
async fn build_forge_app() -> (
    axum::Router,
    SessionId,
    Arc<engram_git_dev::StaticGitHubIntegration>,
) {
    let meta = Arc::new(MockMetadataStore::new());
    let session_id = meta
        .create_session(engram_core::types::session::SessionSpec {
            image: "cortexapps/engrams:warm-bootstrap".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let forge = Arc::new(engram_git_dev::StaticGitHubIntegration::github(
        "ghs_test_xyz",
    ));
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
    app.integrations = engram_coordinator::integrations::IntegrationBroker::with(forge.clone());
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
                wire_version: 0,
                stages_images: false,
            },
        );
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        let fx = Self {
            app: api::router(state),
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
            capture_env: Vec::new(),
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

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Bearer-token middleware
// ---------------------------------------------------------------------

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

// ADR 0021 P1.3 deleted `create_session_unknown_harness_name_is_400`:
// the session no longer selects which harness to attach (that's an
// image-manifest property baked at image-bake time). The equivalent
// post-0021 failure mode is "the image has no [harness] block, so
// even with mode=Agent the session runs as a dev VM" — which is *not*
// an error condition, it's the harness-less template case. The image-
// manifest validation in engram-image-builder catches a malformed
// `[harness]` block at bake; there's no per-session "unknown harness"
// path anymore.

async fn post(app: axum::Router, uri: &str, body: Value) -> axum::http::Response<Body> {
    app.oneshot(json_request(Method::POST, uri, body))
        .await
        .unwrap()
}

// ---------------------------------------------------------------------
// /sessions/:id/events bus
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// End-to-end exec wiring (POST /sessions → registry → sandbox.exec)
//
// The registry is in-process state owned by AppState. Tests that need
// create→exec to share state must reuse the *same* `axum::Router`
// across requests (cloning the Router is fine — it shares the
// Arc<AppState> internally).
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = build_app(MockMetadataStore::arc());
    let resp = app
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------
// Image manifest + secret resolution + rootfs materialization
// ---------------------------------------------------------------------

// Phase 2 removed the "no image / empty workdir" fallback path. The
// `session_with_no_image_falls_through_to_empty_workdir` test that
// exercised it lived here; it was deleted alongside the fallback
// because every session now declares an explicit image (verified by
// `create_session_with_unknown_image_returns_400`).

// ---------------------------------------------------------------------
// Session-id injection
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Persistent event log + late-join via ?since=N / Last-Event-ID
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// ADR 0005 retired the checkpoint / fork / diff / log?kind=workspace
// endpoints along with the platform's git surface. The deleted routes
// now return 404 because they're no longer registered.
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// ADR 0007 — POST /api/admin/reap-materialize-dir
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

/// Issue #231 regression: the heartbeat handler must NOT swallow a
/// `touch_host_heartbeat` persist failure into a 200. If it does, the
/// host's `last_heartbeat_at` row goes stale while the host keeps
/// getting 200-acks (so it never retries/backs off), and a sibling
/// coord pod's dead-host detector then orphans every session on a host
/// that is alive and loaded. The contract: persist failure ⇒ 5xx so
/// the host's loop backs off and the failure is visible.
#[tokio::test]
async fn heartbeat_persist_failure_returns_5xx_not_swallowed_200() {
    let store = MockMetadataStore::arc();
    let f = TestFixture::new(store.clone(), InMemorySecretStore::new());
    let host_id = f.test_host_id;
    let app = f.app;

    let hb_body = json!({
        "capacity": { "total_mib": 16384u64, "used_mib": 0u64, "running_sandboxes": 0u32 },
    });

    // Persist fails (saturated pool) → the handler must return a 5xx,
    // NOT a 200. This is the regression: pre-fix the error was only
    // logged and the handler fell through to a 200 ack.
    store
        .heartbeat_persist_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let resp = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/hosts/{host_id}/heartbeat"),
            hb_body.clone(),
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_server_error(),
        "persist failure must surface as 5xx so the host backs off; got {}",
        resp.status()
    );

    // Control: with the persist healthy, the *same* request acks 200 —
    // proving the 5xx above is caused specifically by the persist
    // failure, not by an unrelated handler error.
    store
        .heartbeat_persist_fails
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/hosts/{host_id}/heartbeat"),
            hb_body,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "healthy persist should ack 200",
    );
}
