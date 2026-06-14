//! ADR 0036 P4 end-to-end (coord-level): enabling the SAME content
//! under two different tags captures exactly ONE base snapshot.
//!
//! Drives the real enable pipeline — `create_or_get_enable_job` →
//! `enable_scanner` → `fetch_and_seal_manifest` → per-chunk
//! materialize → content-keyed capture reuse → `enabled_images`
//! upsert — against a fake in-process OCI registry and a fake capture
//! host that counts `build_base_snapshot` calls. This is the
//! regression guard for the moved-tag scenario: a re-bake pushed
//! under a fresh `warm-<sha>` tag with byte-identical content must
//! reuse the existing snapshot (no capture VM, no new lineage), while
//! both enable jobs still reach `ready`.
//!
//! What would catch it failing:
//!   - the bake's ManifestRef regressing to a random id (content no
//!     longer recognizable → second capture fires),
//!   - `find_enabled_image_by_content` drifting (ditto),
//!   - the scanner not driving jobs to `ready`, or
//!   - the per-chunk materialize failing against a spec-shaped
//!     registry.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL` (CI's Postgres-gated lane runs it).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use bytes::Bytes;
use chrono::Utc;
use engram_chunk_store::{Bootstrap, ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind};
use engram_coordinator::config::CoordinatorConfig;
use engram_coordinator::{enable_scanner, AppState};
use engram_core::error::SandboxError;
use engram_core::traits::{HostClient, MetadataStore};
use engram_core::types::registry::EnableJobState;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::SessionId;
use engram_core::types::{HostCapacity, HostMetadata, HostStatus};
use engram_core::{HostId, SandboxId, SnapshotId};
use engram_oci::{AnonymousResolver, ChunkLayerRef, ChunkedImageLayers, OciClient};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

// ---------------------------------------------------------------
// Fake OCI registry (push + pull verbs). Same shape as
// engram-oci/tests/per_chunk_roundtrip.rs — duplicated because test
// fixtures don't cross crate boundaries.
// ---------------------------------------------------------------

#[derive(Clone, Default)]
struct Registry {
    blobs: Arc<Mutex<HashMap<String, Bytes>>>,
    sessions: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    manifests: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
}

async fn v2_root() -> StatusCode {
    StatusCode::OK
}

async fn get_blob(
    State(reg): State<Registry>,
    AxPath((_repo, digest)): AxPath<(String, String)>,
) -> Response {
    let Some(body) = reg.blobs.lock().get(&digest).cloned() else {
        return (StatusCode::NOT_FOUND, "no such blob").into_response();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn begin_upload(State(reg): State<Registry>, AxPath(repo): AxPath<String>) -> Response {
    let id = Uuid::new_v4().to_string();
    reg.sessions.lock().insert(id.clone(), Vec::new());
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            axum::http::header::LOCATION,
            format!("/v2/{repo}/blobs/uploads/{id}"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn patch_upload(
    State(reg): State<Registry>,
    AxPath((repo, id)): AxPath<(String, String)>,
    body: Bytes,
) -> Response {
    let mut sessions = reg.sessions.lock();
    let Some(buf) = sessions.get_mut(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    buf.extend_from_slice(&body);
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            axum::http::header::LOCATION,
            format!("/v2/{repo}/blobs/uploads/{id}"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn put_upload(
    State(reg): State<Registry>,
    AxPath((repo, id)): AxPath<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let Some(mut buf) = reg.sessions.lock().remove(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    buf.extend_from_slice(&body);
    let Some(digest) = q.get("digest").cloned() else {
        return (StatusCode::BAD_REQUEST, "missing digest").into_response();
    };
    reg.blobs.lock().insert(digest.clone(), Bytes::from(buf));
    Response::builder()
        .status(StatusCode::CREATED)
        .header(
            axum::http::header::LOCATION,
            format!("/v2/{repo}/blobs/{digest}"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn put_manifest(
    State(reg): State<Registry>,
    AxPath((repo, tag)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    reg.manifests.lock().insert(tag, (content_type, body));
    Response::builder()
        .status(StatusCode::CREATED)
        .header(
            axum::http::header::LOCATION,
            format!("/v2/{repo}/manifests/x"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn get_manifest(
    State(reg): State<Registry>,
    AxPath((_repo, tag)): AxPath<(String, String)>,
) -> Response {
    let Some((content_type, body)) = reg.manifests.lock().get(&tag).cloned() else {
        return (StatusCode::NOT_FOUND, "no such manifest").into_response();
    };
    let digest = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(&body));
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header("Docker-Content-Digest", digest)
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn spawn_registry() -> (SocketAddr, oneshot::Sender<()>) {
    let reg = Registry::default();
    let app = Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2/:repo/blobs/:digest", get(get_blob))
        .route("/v2/:repo/blobs/uploads/", post(begin_upload))
        .route(
            "/v2/:repo/blobs/uploads/:id",
            axum::routing::patch(patch_upload).put(put_upload),
        )
        .route(
            "/v2/:repo/manifests/:tag",
            put(put_manifest).get(get_manifest),
        )
        .with_state(reg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, tx)
}

// ---------------------------------------------------------------
// Fake capture host: counts `build_base_snapshot` calls, returns a
// fixed synthetic snapshot whose (empty) disk manifest is pre-seeded
// in BlobStorage so HEAD-verify passes.
// ---------------------------------------------------------------

struct FakeCaptureHost {
    captures: AtomicUsize,
    disk_manifest: engram_core::types::manifest::ManifestRef,
}

#[async_trait]
impl HostClient for FakeCaptureHost {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![])
    }
    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!()
    }
    async fn restore(&self, _md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: engram_core::types::sandbox::AgentSpec,
        _policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn apply_egress_policy(
        &self,
        _policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
        None
    }
    async fn bind_session(&self, _session_id: SessionId, _sandbox_id: SandboxId) {}
    async fn unbind_session(&self, _session_id: SessionId) {}
    async fn send_prompt(&self, _sandbox_id: SandboxId, _text: String) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn acquire_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn release_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn build_base_snapshot(
        &self,
        _spec: SandboxSpec,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        Ok(SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes: 4096,
            created_at: Utc::now(),
            image_version: "reuse-fixture".into(),
            disk_manifest: Some(self.disk_manifest),
            memory_manifest: None, // cold-boot shape (VZ-like)
            base_memory_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
        })
    }
}

// ---------------------------------------------------------------
// The test.
// ---------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn second_tag_with_identical_content_reuses_base_snapshot() {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: ENGRAM_TEST_DATABASE_URL not set");
            return;
        }
    };
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    let meta: Arc<dyn MetadataStore> = Arc::new(store);

    // ---- fixture artifact: 3 chunks, content-derived ManifestRef ----
    let chunk_size = 16u64;
    let bodies: [&[u8]; 3] = [b"0123456789abcdef", b"fedcba9876543210", b"tail"];
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    for b in &bodies {
        chunks.push(ChunkRef {
            offset,
            hash: ChunkHash::of(b),
        });
        offset += chunk_size;
    }
    let total_bytes = 2 * chunk_size + bodies[2].len() as u64;
    let image_manifest = Manifest {
        schema_version: 1,
        kind: ManifestKind::Disk,
        total_bytes,
        chunk_size: ChunkSize::bytes(chunk_size),
        chunks,
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    let content_ref = image_manifest.content_ref();
    let bootstrap = Bootstrap::build_per_chunk(&image_manifest);
    assert!(bootstrap.is_per_chunk());

    // Unique manifest.toml per run so reuse can't match residue from
    // prior runs against the shared dev/CI database. ADR 0048: an
    // enabled image must declare `[resources] vcpus`.
    let manifest_toml = format!(
        "name = \"reuse-fixture-{}\"\n[resources]\nvcpus = 2\n",
        Uuid::new_v4()
    );
    let bundle_json = serde_json::json!({
        "schema_version": 2,
        "disk_manifest": content_ref,
        "bootstrap_disk_available": true,
    });

    // ---- push the SAME artifact under two tags ----
    let (addr, _shutdown) = spawn_registry().await;
    let repo = format!("127.0.0.1:{}/reuse-{}", addr.port(), Uuid::new_v4());
    let uri_a = format!("{repo}:warm-aaaaaaa");
    let uri_b = format!("{repo}:warm-bbbbbbb");

    let oci = OciClient::new(Arc::new(AnonymousResolver));
    for uri in [&uri_a, &uri_b] {
        for b in &bodies {
            let digest = format!("sha256:{}", ChunkHash::of(b).to_hex());
            if !oci.blob_exists(uri, &digest).await.expect("HEAD") {
                oci.push_chunk_blob(uri, &digest, b)
                    .await
                    .expect("push chunk");
            }
        }
        let chunk_refs: Vec<ChunkLayerRef> = bootstrap
            .entries
            .iter()
            .map(|e| ChunkLayerRef {
                digest: e.blob_digest.clone().unwrap(),
                size: e.length as u64,
            })
            .collect();
        oci.push_chunked_image_manifest(
            uri,
            ChunkedImageLayers {
                manifest_toml: manifest_toml.clone().into_bytes(),
                config_json: br#"{"kind":"engram-image-v1"}"#.to_vec(),
                bundle_json: serde_json::to_vec(&bundle_json).unwrap(),
                disk_bootstrap_json: serde_json::to_vec(&bootstrap).unwrap(),
            },
            &chunk_refs,
        )
        .await
        .expect("push manifest");
    }

    // ---- coordinator state: shared blob store + fake capture host ----
    let blob_dir = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_dir.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());

    // The fake host's snapshot points at an EMPTY disk manifest,
    // pre-seeded so `verify_snapshot_recoverable`'s HEAD-verify
    // passes without a real capture pipeline.
    let snapshot_disk_ref = engram_core::types::manifest::ManifestRef::new();
    chunk_store
        .put_manifest(snapshot_disk_ref, &Manifest::empty(ManifestKind::Disk, 0))
        .await
        .expect("seed snapshot disk manifest");

    let services = engram_coordinator::Services {
        meta: meta.clone(),
        cloud: Arc::new(engram_cloud_mock::MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            engram_sandbox_process::ProcessBackend::new(blob_dir.path().join("sandboxes")),
        ))),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0xab; 32], "test:v1",
        )),
        oci: Arc::new(OciClient::new(Arc::new(AnonymousResolver))),
        auth_resolver: Arc::new(AnonymousResolver),
        blob: blob.clone(),
        chunk_store,
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
    };
    // `AppState::new` would auto-register `services.host` (the
    // ProcessBackend stub, which can't capture) and `pick_capture_host`
    // could pick it. Build the registry by hand with ONLY the counting
    // fake capture host.
    let host_id = HostId::new();
    let capture_host = Arc::new(FakeCaptureHost {
        captures: AtomicUsize::new(0),
        disk_manifest: snapshot_disk_ref,
    });
    let host_registry = Arc::new(engram_coordinator::host_registry::HostRegistry::new(
        meta.clone(),
    ));
    host_registry.register(host_id, capture_host.clone());
    let state = Arc::new(AppState::new_with_registry(
        CoordinatorConfig::default(),
        services,
        host_registry,
    ));
    meta.upsert_host(engram_core::types::host::HostRecord {
        id: host_id,
        hostname: format!("capture-fixture-{host_id}"),
        cloud_metadata: HostMetadata::default(),
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 0,
            total_mib: 65_536,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: None,
        ready_images: Vec::new(),
        local_snapshots: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
    })
    .await
    .expect("hosts row");

    // ---- run the scanner; enable tag A, then tag B ----
    let _scanner = enable_scanner::spawn(
        enable_scanner::EnableScannerConfig {
            poll_interval: Duration::from_millis(200),
            // Generous claim budget: the shared CI database may hold
            // residual jobs from other runs; ours must not be starved
            // out of a claim batch.
            claim_limit: 16,
            ..Default::default()
        },
        state.clone(),
    );

    let wait_ready = |job_id: Uuid| {
        let meta = meta.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let job = meta
                    .get_enable_job(job_id)
                    .await
                    .expect("get job")
                    .expect("job exists");
                match job.state {
                    EnableJobState::Ready => return job,
                    EnableJobState::Failed => {
                        panic!("enable job failed: {:?}", job.error)
                    }
                    _ => {}
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "enable job {job_id} stuck in {:?}",
                    job.state
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };

    let job_a = meta
        .create_or_get_enable_job(&uri_a, None)
        .await
        .expect("job a");
    wait_ready(job_a.id).await;
    assert_eq!(
        capture_host.captures.load(Ordering::SeqCst),
        1,
        "first enable must capture exactly once"
    );

    let job_b = meta
        .create_or_get_enable_job(&uri_b, None)
        .await
        .expect("job b");
    let job_b = wait_ready(job_b.id).await;
    assert_eq!(
        capture_host.captures.load(Ordering::SeqCst),
        1,
        "second tag with identical content must NOT boot a capture VM"
    );
    assert_eq!(
        job_b.chunks_total,
        Some(bootstrap.entries.len() as u32),
        "progress total must reflect the bootstrap"
    );

    // Both rows exist and share the SAME base snapshot lineage.
    let row_a = meta
        .get_enabled_image(&uri_a)
        .await
        .expect("get a")
        .expect("row a");
    let row_b = meta
        .get_enabled_image(&uri_b)
        .await
        .expect("get b")
        .expect("row b");
    assert_eq!(row_a.disk_manifest, Some(content_ref));
    assert_eq!(row_b.disk_manifest, Some(content_ref));
    assert_eq!(
        row_a.base_snapshot_id, row_b.base_snapshot_id,
        "both tags must restore from the same reused base snapshot"
    );
}
