//! ADR 0084 §B/§D: `claim_capture_job`'s cold-base plan resolution
//! (`resolve_cold_base_plan`, `pub(crate)` — reached only through the
//! `pub` axum handler, called in-process exactly like
//! `enable_reuse_live_pg.rs`'s capture-job simulator does).
//!
//! Drives the REAL `host_http::claim_capture_job` handler against a
//! minimal `AppState` (no scanner, no materializer, no real host — this
//! suite only cares what `cold_base_plan` the claim resolves to for a
//! given `(disk_manifest, resources, fc_snapshot_version)` /
//! `cold_bases` state, not the surrounding job lifecycle those other
//! suites already cover).
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use chrono::Utc;
use engram_chunk_store::{Manifest, ManifestKind};
use engram_coordinator::config::CoordinatorConfig;
use engram_coordinator::AppState;
use engram_core::traits::MetadataStore;
use engram_core::types::capture_job::{cold_base_content_key, ColdBasePlan, ColdBaseRow};
use engram_core::types::host::{CapStatus, HostCapabilities, HostCapacity, HostMetadata};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::{HostRecord, HostStatus, NewCaptureJob};
use engram_core::HostId;
use engram_oci::AnonymousResolver;
use uuid::Uuid;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
}

fn test_config() -> engram_core::types::image::ImageConfig {
    toml::from_str(
        "name = \"claim-cold-base-fixture\"\n[resources]\nsuggested_vcpus = 2\n\
         suggested_memory_mib = 2048\n",
    )
    .unwrap()
}

/// Build a minimal `AppState` — no real host client is ever dialed by
/// `claim_capture_job` (it only reads/writes PG + BlobStorage), so a
/// `ProcessBackend`-backed `LocalHostClient` is a harmless placeholder.
async fn build_state(
    meta: Arc<dyn MetadataStore>,
) -> (
    Arc<AppState>,
    engram_chunk_store::ChunkStore,
    Arc<dyn engram_core::traits::BlobStorage>,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());
    let services = engram_coordinator::Services {
        meta: meta.clone(),
        cloud: Arc::new(engram_cloud_mock::MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            engram_sandbox_process::ProcessBackend::new(tmp.path().join("sandboxes")),
        ))),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0xcd; 32], "test:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(AnonymousResolver))),
        auth_resolver: Arc::new(AnonymousResolver),
        blob: blob.clone(),
        chunk_store: chunk_store.clone(),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
        clock: Arc::new(engram_core::traits::SystemClock::new()),
        entropy: Arc::new(engram_core::traits::OsEntropy),
    };
    let host_registry = Arc::new(engram_coordinator::host_registry::HostRegistry::new(
        meta.clone(),
    ));
    let state = Arc::new(AppState::new_with_registry(
        CoordinatorConfig::default(),
        services,
        host_registry,
    ));
    std::mem::forget(tmp);
    (state, chunk_store, blob)
}

/// Register an FC-capable host reporting `fc_version` — the exact
/// shape `resolve_cold_base_plan` requires to consider a cold base at
/// all (`capabilities.backend == "firecracker"` AND a reported
/// `fc_snapshot_version`).
async fn seed_fc_host(meta: &Arc<dyn MetadataStore>, fc_version: &str) -> HostId {
    let host_id = HostId::new();
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: format!("cold-base-fixture-{host_id}"),
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
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: HostCapabilities {
            schema: 1,
            backend: "firecracker".to_string(),
            grpc_self_connect: CapStatus::Ok(None),
            base_shm_tmpfs: CapStatus::Ok(None),
            uffd_minor_shmem: CapStatus::Ok(None),
            nbd: CapStatus::Ok(None),
            bundle_stamp: CapStatus::Ok(None),
            fc_snapshot_version: Some(fc_version.to_string()),
            wire_version: engram_protocol::WIRE_VERSION,
        },
    })
    .await
    .expect("upsert host");
    host_id
}

async fn seed_capture_job(
    meta: &Arc<dyn MetadataStore>,
    host_id: HostId,
    disk_manifest: &str,
) -> engram_core::types::capture_job::CaptureJobRow {
    let uri = format!("test-registry.local/claim-cold-base/{}", Uuid::new_v4());
    let enable_job_id = meta
        .create_or_get_enable_job(&uri, None, &test_config())
        .await
        .expect("seed enable job")
        .id;
    let job = meta
        .insert_capture_job(NewCaptureJob {
            enable_job_id,
            image_uri: uri,
            manifest_digest: "sha256:deadbeef".to_string(),
            disk_manifest: disk_manifest.to_string(),
            image_config: test_config(),
            oci_defaults: Default::default(),
            mem_budget_mib: 2_048,
            cpu_budget_vcpus: 2,
        })
        .await
        .expect("insert capture job");
    // ADR 0084 (c): a fresh job inserts WAITING; bind it to `host_id` via
    // the reserving pick so the claim handler (which checks the job is
    // assigned to the claiming host) accepts it.
    meta.place_capture_job(job.id, &[host_id])
        .await
        .expect("place capture job")
        .expect("row present")
}

async fn claim(
    state: &Arc<AppState>,
    host_id: HostId,
    job_id: engram_core::types::CaptureJobId,
    epoch: i64,
) -> engram_core::types::capture_job::CaptureJobSpec {
    engram_coordinator::api::host_http::claim_capture_job(
        State(state.clone()),
        AxPath((host_id, job_id)),
        axum::Json(engram_coordinator::api::host_http::ClaimCaptureJobRequest { epoch }),
    )
    .await
    .expect("claim must succeed")
    .0
}

/// No `cold_bases` row exists at all for this content key (or any
/// other version of this disk_manifest) → `Miss { reason: NoCandidate }`.
#[tokio::test]
#[ignore]
async fn miss_with_no_candidate_at_all() {
    let Some(meta) = connect().await else { return };
    let (state, _cs, _blob) = build_state(meta.clone()).await;
    let host_id = seed_fc_host(&meta, "v10").await;
    let disk_manifest = format!("{}@v1", Uuid::new_v4());
    let row = seed_capture_job(&meta, host_id, &disk_manifest).await;

    let spec = claim(&state, host_id, row.id, row.epoch).await;
    match spec.cold_base_plan {
        ColdBasePlan::Miss { reason, .. } => assert_eq!(
            reason,
            engram_core::types::capture_job::ColdBaseMissReason::NoCandidate
        ),
        other => panic!("expected Miss{{NoCandidate}}, got {other:?}"),
    }
}

/// A `cold_bases` row exists for this `disk_manifest` but under a
/// DIFFERENT `fc_snapshot_version` → `Miss { reason: FcVersionChanged }`.
#[tokio::test]
#[ignore]
async fn miss_with_fc_version_changed() {
    let Some(meta) = connect().await else { return };
    let (state, _cs, _blob) = build_state(meta.clone()).await;
    let host_id = seed_fc_host(&meta, "v10").await;
    let disk_manifest = format!("{}@v1", Uuid::new_v4());
    let row = seed_capture_job(&meta, host_id, &disk_manifest).await;

    // A row under v9 for the SAME disk_manifest (content_key differs
    // because it embeds the version, so this is deliberately NOT the
    // exact key `claim` will look up).
    meta.upsert_cold_base(ColdBaseRow {
        content_key: format!("stale-key-{}", Uuid::new_v4()),
        snapshot_id: engram_core::SnapshotId::new(),
        disk_manifest: disk_manifest.clone(),
        memory_manifest: format!("{}@v1", Uuid::new_v4()),
        fc_snapshot_version: "v9".to_string(),
        captured_at: Utc::now(),
        snapshot_bincode: vec![1],
    })
    .await
    .expect("seed v9 cold base");

    let spec = claim(&state, host_id, row.id, row.epoch).await;
    match spec.cold_base_plan {
        ColdBasePlan::Miss { reason, .. } => assert_eq!(
            reason,
            engram_core::types::capture_job::ColdBaseMissReason::FcVersionChanged
        ),
        other => panic!("expected Miss{{FcVersionChanged}}, got {other:?}"),
    }
}

/// A verified-present `cold_bases` row exists at the EXACT content key
/// → `Hit`, carrying the candidate's own `SnapshotMetadata`.
#[tokio::test]
#[ignore]
async fn hit_with_a_verified_present_candidate() {
    let Some(meta) = connect().await else { return };
    let (state, chunk_store, blob) = build_state(meta.clone()).await;
    let host_id = seed_fc_host(&meta, "v10").await;
    let disk_manifest_ref = ManifestRef::new();
    let mem_manifest_ref = ManifestRef::new();

    // Empty-but-real manifests: `Manifest::empty` has zero chunks, so
    // the chunk-presence self-heal HEAD-checks trivially pass (nothing
    // to check) while still exercising the real `get_manifest` +
    // presence-verify path.
    chunk_store
        .put_manifest(disk_manifest_ref, &Manifest::empty(ManifestKind::Disk, 0))
        .await
        .expect("seed disk manifest");
    chunk_store
        .put_manifest(mem_manifest_ref, &Manifest::empty(ManifestKind::Memory, 0))
        .await
        .expect("seed memory manifest");
    let _ = &blob;

    let row = seed_capture_job(&meta, host_id, &disk_manifest_ref.to_string()).await;
    let config = test_config();
    let content_key = cold_base_content_key(
        &disk_manifest_ref.to_string(),
        &config.resources,
        Some("v10"),
        "firecracker",
    );

    let candidate_snapshot = engram_core::types::snapshot::SnapshotMetadata {
        id: engram_core::SnapshotId::new(),
        size_bytes: 0,
        created_at: Utc::now(),
        image_version: "cold-base-fixture:1".into(),
        disk_manifest: Some(disk_manifest_ref),
        memory_manifest: Some(mem_manifest_ref),
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
    meta.upsert_cold_base(ColdBaseRow {
        content_key: content_key.clone(),
        snapshot_id: candidate_snapshot.id,
        disk_manifest: disk_manifest_ref.to_string(),
        memory_manifest: mem_manifest_ref.to_string(),
        fc_snapshot_version: "v10".to_string(),
        captured_at: Utc::now(),
        snapshot_bincode: bincode::serialize(&candidate_snapshot).unwrap(),
    })
    .await
    .expect("seed hit candidate");

    let spec = claim(&state, host_id, row.id, row.epoch).await;
    match spec.cold_base_plan {
        ColdBasePlan::Hit {
            content_key: got_key,
            snapshot,
        } => {
            assert_eq!(got_key, content_key);
            assert_eq!(snapshot.id, candidate_snapshot.id);
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

/// A `cold_bases` row exists at the exact content key, but its chunks
/// are MISSING from BlobStorage (never populated) → `Miss { reason:
/// ChunksMissing }`, not a silent Hit on a dead pointer.
#[tokio::test]
#[ignore]
async fn miss_when_candidate_chunks_are_missing() {
    let Some(meta) = connect().await else { return };
    let (state, _cs, _blob) = build_state(meta.clone()).await;
    let host_id = seed_fc_host(&meta, "v10").await;
    // A manifest ref that was NEVER `put_manifest`'d — `get_manifest`
    // fails, which `resolve_cold_base_plan`/`reuse_candidate_chunks_present`
    // must treat as "not reusable", not propagate as an error.
    let disk_manifest_ref = ManifestRef::new();
    let row = seed_capture_job(&meta, host_id, &disk_manifest_ref.to_string()).await;
    let config = test_config();
    let content_key = cold_base_content_key(
        &disk_manifest_ref.to_string(),
        &config.resources,
        Some("v10"),
        "firecracker",
    );
    meta.upsert_cold_base(ColdBaseRow {
        content_key,
        snapshot_id: engram_core::SnapshotId::new(),
        disk_manifest: disk_manifest_ref.to_string(),
        memory_manifest: ManifestRef::new().to_string(),
        fc_snapshot_version: "v10".to_string(),
        captured_at: Utc::now(),
        snapshot_bincode: vec![1],
    })
    .await
    .expect("seed dangling cold base");

    let spec = claim(&state, host_id, row.id, row.epoch).await;
    match spec.cold_base_plan {
        ColdBasePlan::Miss { reason, .. } => assert_eq!(
            reason,
            engram_core::types::capture_job::ColdBaseMissReason::ChunksMissing
        ),
        other => panic!("expected Miss{{ChunksMissing}}, got {other:?}"),
    }
}
