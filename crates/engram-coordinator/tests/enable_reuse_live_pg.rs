//! ADR 0036 P4 / ADR 0080 phase 3b end-to-end (coord-level): enabling
//! the SAME content under two different tags materializes twice but
//! captures exactly ONE base snapshot.
//!
//! Drives the real enable pipeline — `create_or_get_enable_job` →
//! `enable_scanner` → host-side `materialize_image` (the REAL
//! `engram-rootfs-materializer` pipeline: pull → flatten → inject →
//! pack → chunk (ADR 0093 streaming pack end to end, a deterministic
//! streaming pack, pure Rust — no e2fsprogs) → content-keyed capture
//! reuse → `enabled_images` upsert — against a fake in-process docker
//! registry serving a STANDARD OCI image under two tags with shared
//! layers, and a fake host that counts `materialize_image` +
//! `build_base_snapshot` calls. This is the regression guard for the
//! moved-tag scenario: a re-push under a fresh `warm-<sha>` tag with
//! byte-identical content must reuse the existing snapshot (no capture
//! VM, no new lineage), while both enable jobs still reach `ready`.
//!
//! What would catch it failing:
//!   - the materializer's ManifestRef regressing to a random id
//!     (content no longer recognizable → second capture fires),
//!   - `find_enabled_image_by_content` drifting (ditto),
//!   - the scanner not driving jobs to `ready`, or
//!   - the pull/flatten path failing against a spec-shaped registry.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL` (CI's Postgres-gated lane runs it).

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use chrono::Utc;
use engram_chunk_store::{Manifest, ManifestKind};
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
use engram_oci::{AnonymousResolver, OciClient};
use engram_rootfs_materializer::{InitInjection, Materializer, Transport};
use parking_lot::Mutex;
use sha2::Digest as _;
use tokio::sync::oneshot;
use uuid::Uuid;

// ---------------------------------------------------------------
// Fake docker registry (pull verbs only — the fixtures are inserted
// directly). Same shape as the materializer's own integration tests
// (`engram-rootfs-materializer/tests/materialize.rs`) — duplicated
// because test fixtures don't cross crate boundaries.
// ---------------------------------------------------------------

#[derive(Clone, Default)]
struct Registry {
    /// digest → blob bytes (config + layers).
    blobs: Arc<Mutex<HashMap<String, Bytes>>>,
    /// tag → (content_type, manifest bytes).
    manifests: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
}

impl Registry {
    fn add_blob(&self, bytes: Vec<u8>) -> (String, u64) {
        let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes)));
        let size = bytes.len() as u64;
        self.blobs.lock().insert(digest.clone(), Bytes::from(bytes));
        (digest, size)
    }

    fn add_manifest(&self, tag: &str, bytes: Vec<u8>) {
        self.manifests.lock().insert(
            tag.to_string(),
            (
                "application/vnd.oci.image.manifest.v1+json".to_string(),
                Bytes::from(bytes),
            ),
        );
    }
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

async fn get_manifest(
    State(reg): State<Registry>,
    AxPath((_repo, reference)): AxPath<(String, String)>,
) -> Response {
    let Some((content_type, body)) = reg.manifests.lock().get(&reference).cloned() else {
        return (StatusCode::NOT_FOUND, "no such manifest").into_response();
    };
    let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&body)));
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header("Docker-Content-Digest", digest)
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn spawn_registry() -> (SocketAddr, Registry, oneshot::Sender<()>) {
    let reg = Registry::default();
    let app = Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2/{repo}/blobs/{digest}", get(get_blob))
        .route("/v2/{repo}/manifests/{reference}", get(get_manifest))
        .with_state(reg.clone());
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
    (addr, reg, tx)
}

/// Minimal valid ImageConfig for enable jobs (ADR 0080: the full config
/// rides the job; ADR 0048: suggested_vcpus required).
fn test_config() -> engram_core::types::image::ImageConfig {
    toml::from_str("name = \"reuse-fixture\"\n[resources]\nsuggested_vcpus = 2\n").unwrap()
}

/// Gzipped tar layer with `entries` as regular files.
fn gzip_layer(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (path, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        tar.append_data(&mut header, path, *body).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

// ---------------------------------------------------------------
// Fake host: `materialize_image` runs the REAL materializer (fake
// packer) into the SHARED test chunk store; `build_base_snapshot`
// counts calls and returns a fixed synthetic snapshot whose (empty)
// disk manifest is pre-seeded in BlobStorage so HEAD-verify passes.
// ---------------------------------------------------------------

struct FakeCaptureHost {
    materializes: AtomicUsize,
    captures: AtomicUsize,
    /// Shares the coordinator's LocalBlobStorage, exactly like the real
    /// host's write-through chunk store shares BlobStorage (ADR 0078).
    chunk_store: engram_chunk_store::ChunkStore,
    scratch: std::path::PathBuf,
    snapshot_disk_ref: engram_core::types::manifest::ManifestRef,
}

#[async_trait]
impl HostClient for FakeCaptureHost {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn destroy(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![])
    }
    async fn probe_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        unimplemented!()
    }
    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!()
    }
    async fn restore(
        &self,
        _md: SnapshotMetadata,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: engram_core::types::sandbox::AgentSpec,
        _policy: engram_core::types::egress::SessionEgressPolicy,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) {
    }
    async fn unbind_session(&self, _session_id: SessionId) {}
    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
        _mode: Option<String>,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }

    /// ADR 0080 phase 3b / ADR 0093: the real materializer pipeline.
    async fn materialize_image(
        &self,
        image_uri: &str,
        platform_os: &str,
        platform_arch: &str,
        _registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth>,
        min_disk_gib: u32,
        progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
    ) -> Result<engram_core::types::MaterializedImage, SandboxError> {
        self.materializes.fetch_add(1, Ordering::SeqCst);
        assert_eq!(platform_os, "linux");
        // The fixture manifest is single-platform (no index), so the
        // pull resolves regardless of the coordinator's arch; the
        // Platform enum only drives index selection.
        let platform = match platform_arch {
            "arm64" => engram_rootfs_materializer::Platform::LinuxArm64,
            _ => engram_rootfs_materializer::Platform::LinuxAmd64,
        };
        // ADR 0093: no packer seam — the streaming pack runs for real
        // (pure Rust, no mke2fs needed in the test env).
        let materializer = Materializer::new(
            OciClient::new(Arc::new(AnonymousResolver)),
            InitInjection {
                vsock_port: 1024,
                transport: Transport::Vsock,
                init_script: None,
            },
        );
        let out = materializer
            .materialize(
                image_uri,
                platform,
                &self.scratch,
                &self.chunk_store,
                (min_disk_gib as u64) << 30,
                Some(progress),
            )
            .await
            .map_err(|e| {
                SandboxError::MaterializeFailed(engram_core::types::MaterializeFailure {
                    kind: engram_core::types::MaterializeFailureKind::Internal,
                    message: format!("fake host materialize: {e}"),
                })
            })?;
        Ok(engram_core::types::MaterializedImage {
            disk_manifest: out.disk_manifest,
            oci_defaults: out.oci_defaults,
            manifest_digest: out.manifest_digest,
            ext4_size_bytes: out.ext4_size_bytes,
        })
    }

    // ADR 0084 P1b: `HostClient::build_base_snapshot` is deleted — capture
    // is now a durable `capture_jobs` row dispatched over the heartbeat,
    // not a direct RPC this fake would intercept. The test below drives a
    // simulated host loop (`spawn_capture_job_simulator`) that polls
    // `capture_assignments_for_host`, calls the REAL `claim_capture_job`
    // handler in-process (exercising the actual `SandboxSpec`/env/egress
    // assembly), and reports a synthetic `Done` terminal via
    // `record_capture_job_report` — the moral equivalent of what this
    // method used to do, minus an actual capture VM.
}

// ---------------------------------------------------------------
// ADR 0084 P1b: simulated host-side capture-job executor. No real
// host-agent process exists in this test, so instead of a fake
// `HostClient::build_base_snapshot` (deleted) this polls
// `capture_assignments_for_host` and, for each unseen `(job_id, epoch)`,
// drives the REAL `claim_capture_job` handler in-process (exercising the
// actual `SandboxSpec`/env/egress assembly the coordinator builds) and
// reports back a synthetic `Done` terminal via `record_capture_job_report`
// — the same store call the real heartbeat reconcile uses.
// ---------------------------------------------------------------

fn spawn_capture_job_simulator(
    state: Arc<engram_coordinator::AppState>,
    meta: Arc<dyn MetadataStore>,
    host_id: HostId,
    capture_host: Arc<FakeCaptureHost>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use engram_core::types::{CaptureJobReport, CaptureJobStage, CaptureTerminalReport};
        use std::collections::HashMap as StdHashMap;
        let mut seen: StdHashMap<Uuid, i64> = StdHashMap::new();
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let Ok(assignments) = meta.capture_assignments_for_host(host_id).await else {
                continue;
            };
            for assignment in assignments {
                let job_uuid = assignment.job_id.as_uuid();
                if seen.get(&job_uuid) == Some(&assignment.epoch) {
                    continue;
                }
                let claimed = engram_coordinator::api::host_http::claim_capture_job(
                    State(state.clone()),
                    AxPath((host_id, assignment.job_id)),
                    axum::Json(engram_coordinator::api::host_http::ClaimCaptureJobRequest {
                        epoch: assignment.epoch,
                    }),
                )
                .await;
                let spec = match claimed {
                    Ok(axum::Json(spec)) => spec,
                    Err(e) => {
                        eprintln!("capture job simulator: claim failed: {e:?}");
                        continue;
                    }
                };
                assert!(
                    spec.spec.rootfs_manifest.is_some(),
                    "capture spec must carry the materialized rootfs_manifest"
                );
                capture_host.captures.fetch_add(1, Ordering::SeqCst);
                let snapshot_meta = SnapshotMetadata {
                    id: SnapshotId::new(),
                    size_bytes: 4096,
                    created_at: Utc::now(),
                    image_version: "reuse-fixture".into(),
                    disk_manifest: Some(capture_host.snapshot_disk_ref),
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                };
                // ADR 0084 P3: `result_bincode` now encodes a
                // `CaptureJobResult` (artifact + optional cold-base
                // outcome), not a bare `SnapshotMetadata`. This
                // simulator's `capture_host` is non-FC-shaped (no
                // `ColdBasePlan` ever resolves to `Hit`/`Miss` for it —
                // `resolve_cold_base_plan` requires `hosts.capabilities.
                // backend == "firecracker"`, which this fixture's `hosts`
                // row never sets), so `cold_base` is always `None` here.
                let result = engram_core::types::capture_job::CaptureJobResult {
                    snapshot: snapshot_meta,
                    cold_base: None,
                };
                let result_bincode =
                    bincode::serialize(&result).expect("encode synthetic CaptureJobResult");
                let report = CaptureJobReport {
                    job_id: assignment.job_id,
                    epoch: assignment.epoch,
                    stage: CaptureJobStage::Done,
                    progress: None,
                    fc_snapshot_version: None,
                    terminal: Some(CaptureTerminalReport::Done { result_bincode }),
                };
                if let Err(e) = meta.record_capture_job_report(&report).await {
                    eprintln!("capture job simulator: record_capture_job_report failed: {e}");
                    continue;
                }
                seen.insert(job_uuid, assignment.epoch);
            }
        }
    })
}

// ---------------------------------------------------------------
// The test.
// ---------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn second_tag_with_identical_content_reuses_base_snapshot() {
    // ADR 0047: placement (the capture-host pick) reads the GLOBAL hosts
    // table, so this test cannot share a database with concurrent/previous
    // runs — a residual `Ready` host row from another run is a legal pick,
    // and since that run's fake capture host is gone the enable job sticks
    // (poll timeout) or fails `HostUnreachable`. ADR 0099 H1: clone a
    // private database from the migrated template.
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let meta: Arc<dyn MetadataStore> = Arc::new(db.store);

    // ---- fixture: a STANDARD docker image under two tags, shared layers ----
    let (addr, reg, _shutdown) = spawn_registry().await;
    let repo = format!("127.0.0.1:{}/reuse-{}", addr.port(), Uuid::new_v4());
    let uri_a = format!("{repo}:warm-aaaaaaa");
    let uri_b = format!("{repo}:warm-bbbbbbb");

    let (cfg_digest, cfg_size) = reg.add_blob(
        br#"{"config":{"Env":["FIXTURE=1","PATH=/usr/bin"],"WorkingDir":"/w"}}"#.to_vec(),
    );
    let layer1 = gzip_layer(&[
        ("bin/tool", b"#!/bin/sh\necho hi\n".as_slice()),
        ("etc/base", b"base-layer".as_slice()),
    ]);
    let layer2 = gzip_layer(&[("etc/upper", b"upper-layer".as_slice())]);
    let (l1_digest, l1_size) = reg.add_blob(layer1);
    let (l2_digest, l2_size) = reg.add_blob(layer2);
    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": cfg_digest,
            "size": cfg_size,
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": l1_digest,
                "size": l1_size,
            },
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": l2_digest,
                "size": l2_size,
            },
        ],
    });
    let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();
    // Two TAGS, same layers (the moved-tag re-push shape).
    reg.add_manifest("warm-aaaaaaa", manifest_bytes.clone());
    reg.add_manifest("warm-bbbbbbb", manifest_bytes);

    // ---- coordinator state: shared blob store + fake host ----
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
        chunk_store: chunk_store.clone(),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
        clock: Arc::new(engram_core::traits::SystemClock::new()),
        entropy: Arc::new(engram_core::traits::OsEntropy),
    };
    // `AppState::new` would auto-register `services.host` (the
    // ProcessBackend stub, which can't materialize or capture) and
    // `pick_capture_host` could pick it. Build the registry by hand
    // with ONLY the counting fake host.
    let host_id = HostId::new();
    let scratch_dir = tempfile::tempdir().expect("scratch");
    let capture_host = Arc::new(FakeCaptureHost {
        materializes: AtomicUsize::new(0),
        captures: AtomicUsize::new(0),
        chunk_store: chunk_store.clone(),
        scratch: scratch_dir.path().to_path_buf(),
        snapshot_disk_ref,
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
        // Unmeasured disk → the ADR 0078 disk floor is soft (no veto);
        // the fake host stays pickable.
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: None,
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    })
    .await
    .expect("hosts row");

    // ADR 0084 P1b: the simulated host-side capture-job executor (no
    // real host-agent process in this test).
    let _capture_sim =
        spawn_capture_job_simulator(state.clone(), meta.clone(), host_id, capture_host.clone());

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
        .create_or_get_enable_job(&uri_a, None, &test_config())
        .await
        .expect("job a");
    wait_ready(job_a.id).await;
    assert_eq!(
        capture_host.materializes.load(Ordering::SeqCst),
        1,
        "first enable materializes once"
    );
    assert_eq!(
        capture_host.captures.load(Ordering::SeqCst),
        1,
        "first enable must capture exactly once"
    );

    let job_b = meta
        .create_or_get_enable_job(&uri_b, None, &test_config())
        .await
        .expect("job b");
    wait_ready(job_b.id).await;
    assert_eq!(
        capture_host.materializes.load(Ordering::SeqCst),
        2,
        "the second tag still materializes (its own pull), producing the same content ref"
    );
    assert_eq!(
        capture_host.captures.load(Ordering::SeqCst),
        1,
        "second tag with identical content must NOT boot a capture VM"
    );

    // Both rows exist, share the SAME content-derived disk manifest
    // (the deterministic double-materialize) and the SAME base
    // snapshot lineage, and carry the Dockerfile-derived defaults.
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
    assert!(row_a.disk_manifest.is_some(), "row a carries a manifest");
    assert_eq!(
        row_a.disk_manifest, row_b.disk_manifest,
        "identical content must reproduce the same content-derived ManifestRef"
    );
    assert_eq!(
        row_a.base_snapshot_id, row_b.base_snapshot_id,
        "both tags must restore from the same reused base snapshot"
    );
    assert!(
        row_a.manifest_digest.starts_with("sha256:"),
        "manifest_digest stamped from the docker platform manifest: {}",
        row_a.manifest_digest
    );
    assert_eq!(
        row_a.oci_defaults.env.get("FIXTURE").map(String::as_str),
        Some("1"),
        "Dockerfile ENV must ride oci_defaults onto the row"
    );
    assert_eq!(row_a.oci_defaults.workdir.as_deref(), Some("/w"));

    // The materialized manifest is durable and resolvable through the
    // coordinator's chunk store (the write-through seam the real host
    // provides via BlobStorage).
    chunk_store
        .get_manifest(row_a.disk_manifest.unwrap())
        .await
        .expect("materialized manifest resolvable from BlobStorage");
}
