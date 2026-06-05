//! HTTP smoke test for the Phase 5 registry + harness + encryption
//! surface.
//!
//! Wires the real `axum` router against an in-memory `MetadataStore`
//! and a real [`engram_crypto::CredCipher`], then drives every CLI-
//! reachable verb through `tower::ServiceExt::oneshot`. The wire
//! shape exercised here is exactly what `engram registry add`,
//! `engram registry list`, etc. POST to the coordinator — the CLI is
//! a thin wrapper over `reqwest`, so locking down this contract
//! locks down the CLI.
//!
//! What this catches:
//! - Wire shape regressions on `POST /api/registries`,
//!   `GET /api/registries`, `DELETE /api/registries/:host`
//! - Same for `/api/harnesses`
//! - The polymorphic `auth_kind` dispatch (static vs
//!   gcp_workload_identity)
//! - Encryption-at-rest: a static-credential row's ciphertext is
//!   genuinely sealed (not just b64'd plaintext) — and the list
//!   endpoint never echoes any cipher material back to clients
//! - Validation surface: missing fields, empty strings, conflicting
//!   auth-kind shapes
//!
//! What this does *not* catch (deliberately, to keep the smoke loop
//! fast):
//! - clap argument parsing — covered by clap's derive codegen and
//!   the small number of CLI-side conversion fns
//! - Real GCP token fetch — the metadata server isn't available in
//!   CI; covered by the `engram-oci-auth` unit tests instead
//! - Real Postgres / live registry — see `tests/ha_listener.rs` for
//!   the existing `#[ignore]` pattern when those are wanted

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use engram_cloud_mock::MockCloud;
use engram_coordinator::{api, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::registry::{RegistryAuthSpec, RegistryCredential};
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------
// MockMetadataStore — narrower than `tests/api.rs::MockMetadataStore`,
// which no-ops the registry/harness methods. This one actually tracks
// rows so we can assert add → list → delete round-trips.
// ---------------------------------------------------------------------

#[derive(Default)]
struct MockMetadataStore {
    registries: Mutex<HashMap<String, RegistryCredential>>,
    // ADR 0021 P1.5a: harness_packs field retired with the registry.
    enabled_images: Mutex<HashMap<String, engram_core::types::EnabledImage>>,
}

impl MockMetadataStore {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
    // -- session / host / snapshot / event surface: unreachable in
    // these tests, but the trait demands they exist. Anything non-
    // session-related returns Ok(default).
    async fn create_session(&self, _: SessionSpec) -> Result<SessionId, MetaError> {
        unreachable!("not exercised by registry_smoke")
    }
    async fn create_session_created(
        &self,
        _: SessionId,
        _: SessionSpec,
        _: HostId,
        _: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        unreachable!("not exercised by registry_smoke")
    }
    async fn get_session(&self, _: SessionId) -> Result<Session, MetaError> {
        unreachable!()
    }
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(vec![])
    }
    async fn transition_session(
        &self,
        _: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError> {
        Ok(target)
    }
    async fn assign_session_host(&self, _: SessionId, _: Option<HostId>) -> Result<(), MetaError> {
        Ok(())
    }
    async fn assign_session_sandbox(
        &self,
        _: SessionId,
        _: Option<SandboxId>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(vec![])
    }
    async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }
    async fn touch_host_heartbeat(
        &self,
        _: HostId,
        _: HostStatus,
        _: engram_core::types::HostCapacity,
        _: engram_core::types::HostUtilization,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(vec![])
    }
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        _: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        Ok(vec![])
    }
    async fn record_snapshot(&self, _: SnapshotRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_snapshots_for_session(
        &self,
        _: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(vec![])
    }
    async fn latest_snapshot_for_session(
        &self,
        _: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(None)
    }
    async fn append_session_event(
        &self,
        _: SessionId,
        _: &str,
        _: serde_json::Value,
    ) -> Result<i64, MetaError> {
        Ok(0)
    }
    async fn list_session_events_since(
        &self,
        _: SessionId,
        _: i64,
        _: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(vec![])
    }

    // -- registry credentials: tracked.
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
    async fn upsert_registry_credential(&self, cred: RegistryCredential) -> Result<(), MetaError> {
        self.registries
            .lock()
            .insert(cred.registry_host.clone(), cred);
        Ok(())
    }
    async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError> {
        let mut v: Vec<_> = self.registries.lock().values().cloned().collect();
        v.sort_by(|a, b| a.registry_host.cmp(&b.registry_host));
        Ok(v)
    }
    async fn registry_credential_for_host(
        &self,
        host: &str,
    ) -> Result<Option<RegistryCredential>, MetaError> {
        Ok(self.registries.lock().get(host).cloned())
    }
    async fn delete_registry_credential(&self, host: &str) -> Result<(), MetaError> {
        match self.registries.lock().remove(host) {
            Some(_) => Ok(()),
            None => Err(MetaError::NotFound),
        }
    }

    // -- harness packs: tracked.
    // ADR 0021 P1.5a: harness-pack mock fns retired with the registry.
    async fn upsert_enabled_image(
        &self,
        ei: engram_core::types::EnabledImage,
    ) -> Result<(), MetaError> {
        self.enabled_images.lock().insert(ei.image_uri.clone(), ei);
        Ok(())
    }
    async fn list_enabled_images(
        &self,
    ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
        let mut v: Vec<_> = self.enabled_images.lock().values().cloned().collect();
        v.sort_by(|a, b| a.image_uri.cmp(&b.image_uri));
        Ok(v)
    }
    async fn get_enabled_image(
        &self,
        uri: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(self.enabled_images.lock().get(uri).cloned())
    }
    async fn get_enabled_image_any(
        &self,
        uri: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(self.enabled_images.lock().get(uri).cloned())
    }
    async fn soft_delete_enabled_image(
        &self,
        uri: &str,
    ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
        match self.enabled_images.lock().remove(uri) {
            Some(_) => Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled),
            None => Err(MetaError::NotFound),
        }
    }
    async fn delete_enabled_image(&self, uri: &str) -> Result<(), MetaError> {
        match self.enabled_images.lock().remove(uri) {
            Some(_) => Ok(()),
            None => Err(MetaError::NotFound),
        }
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
// Fixture
// ---------------------------------------------------------------------

/// Build a router wired to an empty `MockMetadataStore` and a real
/// `EnvVarKeyProvider` test KEK. Returns the router + a handle on
/// the store so tests can peek at what landed.
fn build_app() -> (axum::Router, Arc<MockMetadataStore>) {
    let meta = MockMetadataStore::arc();
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta: meta.clone(),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0xab; 32], "test:v1",
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
    let state = Arc::new(AppState::new(cfg, services));
    (api::router(state), meta)
}

/// Issue an HTTP request against the router and return (status, JSON
/// body). The body is parsed leniently — empty bodies (204) yield
/// `Value::Null`. Tests that care about the boundary check status
/// before reading the body.
async fn send(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    let body = match body {
        Some(j) => {
            builder = builder.header("content-type", "application/json");
            Body::from(j.to_string())
        }
        None => Body::empty(),
    };
    let req = builder.body(body).expect("build request");
    let resp = app.clone().oneshot(req).await.expect("router oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    // Successful responses are always JSON. Error responses might be
    // plaintext (axum's default for serde Json-extractor decode
    // failures, for instance) — in that case we wrap the raw body
    // string into a `Value::String` so tests can still log the
    // response without crashing the harness.
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
        }
    };
    (status, json)
}

// ---------------------------------------------------------------------
// /api/registries
// ---------------------------------------------------------------------

#[tokio::test]
async fn add_static_registry_seals_password_and_returns_summary() {
    let (app, meta) = build_app();

    let body = json!({
        "host": "gcr.io",
        "auth": {
            "kind": "static",
            "username": "_json_key",
            "password": "hunter2",
        }
    });
    let (status, resp) = send(&app, Method::POST, "/api/v1/registries", Some(body)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "expected 201, got {status}: {resp}"
    );

    // Response carries identity but not secret material.
    assert_eq!(resp["host"], "gcr.io");
    assert_eq!(resp["auth_kind"], "static");
    assert_eq!(resp["auth_principal"], "_json_key");
    assert!(
        resp.get("password").is_none(),
        "response leaked password field: {resp}"
    );
    assert!(
        resp.as_object()
            .unwrap()
            .keys()
            .all(|k| !k.contains("ciphertext")),
        "response leaked ciphertext-shaped field: {resp}"
    );

    // The persisted row carries the sealed envelope, not the plaintext.
    let stored = meta
        .registry_credential_for_host("gcr.io")
        .await
        .unwrap()
        .expect("upsert should have persisted the row");
    match stored.auth {
        RegistryAuthSpec::Static {
            username,
            ciphertext,
            wrapped_dek,
            nonce,
            key_id,
        } => {
            assert_eq!(username, "_json_key");
            assert_eq!(key_id, "test:v1");
            assert!(!ciphertext.is_empty(), "ciphertext is empty");
            assert!(
                !ciphertext
                    .windows(b"hunter2".len())
                    .any(|w| w == b"hunter2"),
                "ciphertext contains the plaintext password as a substring — sealing failed"
            );
            // Sanity-check envelope shape: 32-byte+ wrapped DEK, 12-byte nonce.
            assert!(wrapped_dek.len() >= 32);
            assert_eq!(nonce.len(), 12);
        }
        other => panic!("expected Static auth, got {other:?}"),
    }
}

#[tokio::test]
async fn add_gcp_workload_identity_registry_persists_no_secret_material() {
    let (app, meta) = build_app();

    let body = json!({
        "host": "us-east1-docker.pkg.dev",
        "auth": {
            "kind": "gcp_workload_identity",
            "impersonate_sa": "engram@my-project.iam.gserviceaccount.com",
        }
    });
    let (status, resp) = send(&app, Method::POST, "/api/v1/registries", Some(body)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "expected 201, got {status}: {resp}"
    );

    assert_eq!(resp["auth_kind"], "gcp_workload_identity");
    // For cloud-IAM kinds, the principal slot carries the impersonation
    // target — the only non-secret identity-shaped value worth surfacing.
    assert_eq!(
        resp["auth_principal"],
        "engram@my-project.iam.gserviceaccount.com",
    );

    let stored = meta
        .registry_credential_for_host("us-east1-docker.pkg.dev")
        .await
        .unwrap()
        .unwrap();
    match stored.auth {
        RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa } => {
            assert_eq!(
                impersonate_sa.as_deref(),
                Some("engram@my-project.iam.gserviceaccount.com"),
            );
        }
        other => panic!("expected GcpWorkloadIdentity, got {other:?}"),
    }
}

#[tokio::test]
async fn add_gcp_wi_without_impersonation_uses_ambient_identity() {
    let (app, meta) = build_app();
    let body = json!({
        "host": "gcr.io",
        "auth": { "kind": "gcp_workload_identity" }
    });
    let (status, _) = send(&app, Method::POST, "/api/v1/registries", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);

    let stored = meta
        .registry_credential_for_host("gcr.io")
        .await
        .unwrap()
        .unwrap();
    match stored.auth {
        RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa } => {
            assert_eq!(impersonate_sa, None);
        }
        other => panic!("expected GcpWorkloadIdentity, got {other:?}"),
    }
}

#[tokio::test]
async fn list_registries_redacts_all_secret_material() {
    let (app, _meta) = build_app();
    // Plant one of each kind.
    let _ = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "gcr.io",
            "auth": { "kind": "static", "username": "_json_key", "password": "p1" }
        })),
    )
    .await;
    let _ = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "us-east1-docker.pkg.dev",
            "auth": { "kind": "gcp_workload_identity" }
        })),
    )
    .await;

    let (status, resp) = send(&app, Method::GET, "/api/v1/registries", None).await;
    assert_eq!(status, StatusCode::OK);
    let regs = resp["registries"].as_array().expect("registries: array");
    assert_eq!(regs.len(), 2, "got {regs:?}");

    // Every row must carry kind + host + (optional) principal, and
    // *nothing* secret-shaped. The redaction is the load-bearing
    // promise of this endpoint.
    for r in regs {
        assert!(r.get("registry_host").is_some());
        assert!(r.get("auth_kind").is_some());
        let s = r.to_string();
        for forbidden in [
            "ciphertext",
            "wrapped_dek",
            "nonce",
            "password",
            "p1",
            "secret",
        ] {
            assert!(
                !s.contains(forbidden),
                "list response leaked {forbidden:?}: {s}"
            );
        }
    }
}

#[tokio::test]
async fn add_static_rejects_missing_fields() {
    let (app, _) = build_app();
    // Missing username.
    let (status, _) = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "gcr.io",
            "auth": { "kind": "static", "password": "x" }
        })),
    )
    .await;
    // axum returns 422 for serde decode failures on the Json extractor;
    // 400 for app-layer validation. Either is fine; we just want the
    // request rejected before persistence.
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
        "expected 400 or 422, got {status}"
    );
}

#[tokio::test]
async fn add_static_rejects_empty_password() {
    let (app, _) = build_app();
    let (status, resp) = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "gcr.io",
            "auth": { "kind": "static", "username": "u", "password": "" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {status}: {resp}");
}

#[tokio::test]
async fn delete_registry_round_trip() {
    let (app, _) = build_app();
    let _ = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "gcr.io",
            "auth": { "kind": "static", "username": "u", "password": "p" }
        })),
    )
    .await;

    let (status, _) = send(&app, Method::DELETE, "/api/v1/registries/gcr.io", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Second delete = NotFound → 404.
    let (status, _) = send(&app, Method::DELETE, "/api/v1/registries/gcr.io", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------
// /api/harnesses tests retired with the registry — ADR 0021 P1.5a.
// The four HTTP smoke tests (add/list/reject-empty/delete) tested an
// endpoint that no longer exists; deleted rather than rewritten
// because there is no per-deployment harness registry to exercise.
// The /api/registries tests below stay — registries (Docker creds)
// are orthogonal and unaffected.
// ---------------------------------------------------------------------

#[tokio::test]
async fn upsert_replaces_in_place_for_same_host() {
    // Re-adding the same host UPDATEs rather than 409s. Models the
    // "operator pasted the wrong password, fix it by re-adding"
    // workflow without forcing a delete-then-add ritual.
    let (app, meta) = build_app();
    for password in ["wrong", "right"] {
        let (status, _) = send(
            &app,
            Method::POST,
            "/api/v1/registries",
            Some(json!({
                "host": "gcr.io",
                "auth": { "kind": "static", "username": "u", "password": password }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Exactly one row, with the latest cipher fields. The mock keys
    // by host so a second insert overwrites; the production
    // ON CONFLICT (registry_host) DO UPDATE has the same effect.
    let all = meta.list_registry_credentials().await.unwrap();
    assert_eq!(all.len(), 1);
}

// ---------------------------------------------------------------------
// Stage C: /api/enabled-images
//
// The `enable_image` path makes a real OCI pull, so end-to-end
// coverage of POST lives in `registry_e2e.rs` (testcontainers-backed).
// Here we exercise list/disable, plus the validation surface — the
// paths that don't need a live registry.
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_enabled_images_returns_seeded_rows_sorted() {
    // We bypass POST (which requires a real registry) and seed the
    // store directly via the trait. The list endpoint then exercises
    // the EnabledImage → EnabledImageSummary lift, including manifest
    // parsing for the `manifest_name` / `manifest_description` fields.
    let (app, meta) = build_app();
    let now = chrono::Utc::now();
    for (uri, name) in [
        ("ghcr.io/cortex/api:warm-1", "cortex-api"),
        ("localhost:5001/demo:v3", "demo"),
    ] {
        meta.upsert_enabled_image(engram_core::types::EnabledImage {
            id: uuid::Uuid::new_v4(),
            image_uri: uri.into(),
            manifest_toml: format!("name = \"{name}\"\ndescription = \"hello\"\n"),
            manifest_digest: "sha256:beefcafe".into(),
            disk_manifest: None,
            base_snapshot_id: Some(engram_core::types::SnapshotId::new()),
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
        })
        .await
        .unwrap();
    }

    let (status, body) = send(&app, Method::GET, "/api/v1/enabled-images", None).await;
    assert_eq!(status, StatusCode::OK);
    let images = body["images"].as_array().expect("images array");
    assert_eq!(images.len(), 2);
    // Lift surfaces the parsed manifest name/description.
    assert_eq!(images[0]["manifest_name"], "cortex-api");
    assert_eq!(images[0]["manifest_description"], "hello");
    // The summary must NEVER include the raw manifest_toml — the
    // dashboard renders from the lifted fields.
    assert!(
        images[0].get("manifest_toml").is_none(),
        "summary must drop manifest_toml: {}",
        images[0]
    );
}

#[tokio::test]
async fn disable_enabled_image_404s_when_missing() {
    let (app, _) = build_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/v1/enabled-images/disable",
        Some(json!({ "image_uri": "ghcr.io/never/enabled:v1" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("is not enabled"),
        "error must call out the missing enable: got {body}",
    );
}

#[tokio::test]
async fn disable_enabled_image_204_then_idempotent_404() {
    let (app, meta) = build_app();
    let now = chrono::Utc::now();
    meta.upsert_enabled_image(engram_core::types::EnabledImage {
        id: uuid::Uuid::new_v4(),
        image_uri: "ghcr.io/cortex/api:warm-1".into(),
        manifest_toml: "name = \"cortex-api\"\n".into(),
        manifest_digest: "sha256:abc".into(),
        disk_manifest: None,
        base_snapshot_id: Some(engram_core::types::SnapshotId::new()),
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
    })
    .await
    .unwrap();

    let (status, _) = send(
        &app,
        Method::POST,
        "/api/v1/enabled-images/disable",
        Some(json!({ "image_uri": "ghcr.io/cortex/api:warm-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(
        &app,
        Method::POST,
        "/api/v1/enabled-images/disable",
        Some(json!({ "image_uri": "ghcr.io/cortex/api:warm-1" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "second disable on the same URI must be a clear 404",
    );
}

#[tokio::test]
async fn refresh_404s_when_image_was_never_enabled() {
    // Refresh is *not* an alias for enable — we want operators to
    // explicitly opt an image into the catalog before refreshing it.
    let (app, _) = build_app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/v1/enabled-images/refresh",
        Some(json!({ "image_uri": "ghcr.io/cortex/api:never-touched" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("is not enabled"),
        "refresh must steer the operator to the enable path: got {body}",
    );
}

#[tokio::test]
async fn enable_image_rejects_empty_uri() {
    let (app, _) = build_app();
    let (status, _) = send(
        &app,
        Method::POST,
        "/api/v1/enabled-images",
        Some(json!({ "image_uri": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn anonymous_registry_round_trips() {
    // Stage C added `Anonymous` as a first-class auth_kind so public
    // registries can show up in the dashboard without storing fake
    // credentials. POST without password fields, GET sees the row,
    // never echoes any cipher material (there is none to echo).
    let (app, _) = build_app();
    let (status, _) = send(
        &app,
        Method::POST,
        "/api/v1/registries",
        Some(json!({
            "host": "ghcr.io",
            "auth": { "kind": "anonymous" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(&app, Method::GET, "/api/v1/registries", None).await;
    assert_eq!(status, StatusCode::OK);
    let regs = body["registries"].as_array().unwrap();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0]["registry_host"], "ghcr.io");
    assert_eq!(regs[0]["auth_kind"], "anonymous");
    assert!(
        regs[0]["auth_principal"].is_null(),
        "anonymous has no principal"
    );
}
