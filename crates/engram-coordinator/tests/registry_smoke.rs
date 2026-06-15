//! app-gRPC smoke test for the registry + enabled-images + encryption
//! surface (ADR 0051: the web-facing REST routes were deleted; the same
//! `_core` logic now lives behind `ImageService`).
//!
//! Stands up the real `grpc_app::server` over an in-memory `MetadataStore`
//! and a real [`engram_crypto::CredCipher`], then drives every verb through
//! typed tonic clients — the established pattern from `tests/grpc_app.rs`.
//! The wire shape exercised here is exactly what the orchestrator's
//! `ImageService` calls map to.
//!
//! What this catches:
//! - The `AddRegistry` / `ListRegistries` / `DeleteRegistry` round-trip
//! - The polymorphic `auth` oneof dispatch (static vs
//!   gcp_workload_identity vs anonymous)
//! - Encryption-at-rest: a static-credential row's ciphertext is
//!   genuinely sealed (not just b64'd plaintext) — and the list RPC
//!   never echoes any cipher material back to clients
//! - Validation surface: missing fields, empty strings, conflicting
//!   auth-kind shapes (now `InvalidArgument` in place of 400/422)
//! - `ListEnabledImages` / `DisableImage` / `RefreshImage` validation
//!   (the paths that don't need a live registry)
//!
//! What this does *not* catch (deliberately): real GCP token fetch, real
//! Postgres / live registry, the full `EnableImage` OCI pull (see the
//! testcontainer-backed `registry_e2e.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{grpc_app, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::registry::{RegistryAuthSpec, RegistryCredential};
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use engram_protocol::app;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use parking_lot::Mutex;
use tonic::Code;

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
        _: engram_core::types::host::HostHeartbeat,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn set_host_cordoned(&self, _: HostId, _: bool) -> Result<(), MetaError> {
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

/// Token the gRPC test server is configured with.
const TEST_TOKEN: &str = "test-app-grpc-token";

/// A live app-gRPC server + a connected channel.
struct GrpcHarness {
    channel: tonic::transport::Channel,
    handle: tokio::task::JoinHandle<()>,
}

impl GrpcHarness {
    fn image(
        &self,
    ) -> app::image_service_client::ImageServiceClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl tonic::service::Interceptor + Clone,
        >,
    > {
        app::image_service_client::ImageServiceClient::with_interceptor(
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

/// Build an `AppState` wired to an empty `MockMetadataStore` and a real
/// `EnvVarKeyProvider` test KEK, stand up `grpc_app::server` over it, and
/// dial it. Returns the harness + a handle on the store so tests can peek
/// at what landed.
async fn build_app() -> (GrpcHarness, Arc<MockMetadataStore>) {
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
        app_grpc_tokens: vec![TEST_TOKEN.to_string()],
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new(cfg, services));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming");
    let handle = tokio::spawn(async move {
        let _ = grpc_app::server(state).serve_with_incoming(incoming).await;
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC");
    (GrpcHarness { channel, handle }, meta)
}

/// Build a proto `AddRegistryRequest` for a static credential.
fn static_registry(host: &str, username: &str, password: &str) -> app::AddRegistryRequest {
    app::AddRegistryRequest {
        host: host.to_string(),
        auth: Some(app::add_registry_request::Auth::Static(
            app::StaticRegistryAuth {
                username: username.to_string(),
                password: password.to_string(),
            },
        )),
    }
}

// ---------------------------------------------------------------------
// /api/registries
// ---------------------------------------------------------------------

#[tokio::test]
async fn add_static_registry_seals_password_and_returns_summary() {
    let (h, meta) = build_app().await;

    let resp = h
        .image()
        .add_registry(static_registry("gcr.io", "_json_key", "hunter2"))
        .await
        .expect("AddRegistry must succeed")
        .into_inner();

    // Response carries identity but not secret material (the proto
    // `AddRegistryResponse` has no password/ciphertext field — the type
    // is the redaction guard).
    assert_eq!(resp.host, "gcr.io");
    assert_eq!(resp.auth_kind, "static");
    assert_eq!(resp.auth_principal.as_deref(), Some("_json_key"));

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
    let (h, meta) = build_app().await;

    let resp = h
        .image()
        .add_registry(app::AddRegistryRequest {
            host: "us-east1-docker.pkg.dev".into(),
            auth: Some(app::add_registry_request::Auth::GcpWorkloadIdentity(
                app::GcpWorkloadIdentityRegistryAuth {
                    impersonate_sa: Some("engram@my-project.iam.gserviceaccount.com".into()),
                },
            )),
        })
        .await
        .expect("AddRegistry must succeed")
        .into_inner();

    assert_eq!(resp.auth_kind, "gcp_workload_identity");
    // For cloud-IAM kinds, the principal slot carries the impersonation
    // target — the only non-secret identity-shaped value worth surfacing.
    assert_eq!(
        resp.auth_principal.as_deref(),
        Some("engram@my-project.iam.gserviceaccount.com"),
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
    let (h, meta) = build_app().await;
    h.image()
        .add_registry(app::AddRegistryRequest {
            host: "gcr.io".into(),
            auth: Some(app::add_registry_request::Auth::GcpWorkloadIdentity(
                app::GcpWorkloadIdentityRegistryAuth {
                    impersonate_sa: None,
                },
            )),
        })
        .await
        .expect("AddRegistry must succeed");

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
    let (h, _meta) = build_app().await;
    // Plant one of each kind.
    h.image()
        .add_registry(static_registry("gcr.io", "_json_key", "p1"))
        .await
        .expect("add static");
    h.image()
        .add_registry(app::AddRegistryRequest {
            host: "us-east1-docker.pkg.dev".into(),
            auth: Some(app::add_registry_request::Auth::GcpWorkloadIdentity(
                app::GcpWorkloadIdentityRegistryAuth {
                    impersonate_sa: None,
                },
            )),
        })
        .await
        .expect("add gcp-wi");

    let regs = h
        .image()
        .list_registries(app::ListRegistriesRequest::default())
        .await
        .expect("ListRegistries")
        .into_inner()
        .registries;
    assert_eq!(regs.len(), 2, "got {regs:?}");

    // Every row carries kind + host + (optional) principal, and *nothing*
    // secret-shaped. The proto `RegistryCredentialSummary` has no cipher
    // fields at all (the type is the redaction guard); we additionally
    // assert the plaintext password never appears in the debug form.
    for r in &regs {
        assert!(!r.registry_host.is_empty());
        assert!(!r.auth_kind.is_empty());
        let s = format!("{r:?}");
        for forbidden in ["ciphertext", "wrapped_dek", "nonce", "p1"] {
            assert!(
                !s.contains(forbidden),
                "list response leaked {forbidden:?}: {s}"
            );
        }
    }
}

#[tokio::test]
async fn add_static_rejects_missing_username() {
    let (h, _) = build_app().await;
    // Missing username → proto default "" → `add_registry_core` rejects
    // an empty username as a BadRequest → InvalidArgument (the old
    // 400/422). The request is refused before persistence.
    let err = h
        .image()
        .add_registry(static_registry("gcr.io", "", "x"))
        .await
        .expect_err("missing username must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn add_static_rejects_empty_password() {
    let (h, _) = build_app().await;
    let err = h
        .image()
        .add_registry(static_registry("gcr.io", "u", ""))
        .await
        .expect_err("empty password must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn delete_registry_round_trip() {
    let (h, _) = build_app().await;
    h.image()
        .add_registry(static_registry("gcr.io", "u", "p"))
        .await
        .expect("add");

    // First delete succeeds (the old 204).
    h.image()
        .delete_registry(app::DeleteRegistryRequest {
            host: "gcr.io".into(),
        })
        .await
        .expect("first delete must succeed");

    // Second delete = NotFound (the old 404).
    let err = h
        .image()
        .delete_registry(app::DeleteRegistryRequest {
            host: "gcr.io".into(),
        })
        .await
        .expect_err("second delete must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
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
    let (h, meta) = build_app().await;
    for password in ["wrong", "right"] {
        h.image()
            .add_registry(static_registry("gcr.io", "u", password))
            .await
            .expect("add must succeed");
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
    let (h, meta) = build_app().await;
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

    // `ImageService::ListEnabledImages` returns `EnabledImageSummary`
    // (the EnabledImage → summary lift). The mock sorts by uri, so
    // `cortex/api` (ghcr.io) comes first.
    let images = h
        .image()
        .list_enabled_images(app::ListEnabledImagesRequest::default())
        .await
        .expect("ListEnabledImages")
        .into_inner()
        .images;
    assert_eq!(images.len(), 2);
    // Lift surfaces the parsed manifest name/description. The summary proto
    // has no `manifest_toml` field at all (the type is the redaction guard
    // — the dashboard renders from the lifted fields).
    assert_eq!(images[0].manifest_name.as_deref(), Some("cortex-api"));
    assert_eq!(images[0].manifest_description.as_deref(), Some("hello"));
}

#[tokio::test]
async fn disable_enabled_image_not_found_when_missing() {
    let (h, _) = build_app().await;
    let err = h
        .image()
        .disable_image(app::DisableImageRequest {
            image_uri: "ghcr.io/never/enabled:v1".into(),
        })
        .await
        .expect_err("disable of an unenabled image must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    assert!(
        err.message().contains("is not enabled"),
        "error must call out the missing enable: got {:?}",
        err.message(),
    );
}

#[tokio::test]
async fn disable_enabled_image_succeeds_then_idempotent_not_found() {
    let (h, meta) = build_app().await;
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

    h.image()
        .disable_image(app::DisableImageRequest {
            image_uri: "ghcr.io/cortex/api:warm-1".into(),
        })
        .await
        .expect("first disable must succeed (the old 204)");

    let err = h
        .image()
        .disable_image(app::DisableImageRequest {
            image_uri: "ghcr.io/cortex/api:warm-1".into(),
        })
        .await
        .expect_err("second disable on the same URI must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
}

#[tokio::test]
async fn refresh_not_found_when_image_was_never_enabled() {
    // Refresh is *not* an alias for enable — operators must explicitly opt
    // an image into the catalog before refreshing it.
    let (h, _) = build_app().await;
    let err = h
        .image()
        .refresh_image(app::RefreshImageRequest {
            image_uri: "ghcr.io/cortex/api:never-touched".into(),
        })
        .await
        .expect_err("refresh of an unenabled image must be NotFound");
    assert_eq!(err.code(), Code::NotFound, "{err:?}");
    assert!(
        err.message().contains("is not enabled"),
        "refresh must steer the operator to the enable path: got {:?}",
        err.message(),
    );
}

#[tokio::test]
async fn enable_image_rejects_empty_uri() {
    let (h, _) = build_app().await;
    let err = h
        .image()
        .enable_image(app::EnableImageRequest {
            image_uri: String::new(),
        })
        .await
        .expect_err("empty image_uri must be rejected");
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn anonymous_registry_round_trips() {
    // `Anonymous` is a first-class auth_kind so public registries show up
    // in the catalog without storing fake credentials. Add without
    // password fields, list sees the row, never echoes cipher material
    // (there is none to echo).
    let (h, _) = build_app().await;
    h.image()
        .add_registry(app::AddRegistryRequest {
            host: "ghcr.io".into(),
            auth: Some(app::add_registry_request::Auth::Anonymous(
                app::AnonymousRegistryAuth {},
            )),
        })
        .await
        .expect("add anonymous must succeed");

    let regs = h
        .image()
        .list_registries(app::ListRegistriesRequest::default())
        .await
        .expect("ListRegistries")
        .into_inner()
        .registries;
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].registry_host, "ghcr.io");
    assert_eq!(regs[0].auth_kind, "anonymous");
    assert_eq!(regs[0].auth_principal, None, "anonymous has no principal");
}
