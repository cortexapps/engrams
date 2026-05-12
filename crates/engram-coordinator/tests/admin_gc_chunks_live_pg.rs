//! Live-Postgres integration test for ADR 0007's chunk-store GC
//! admin endpoint. Asserts the full pipeline:
//!
//!   POST /api/admin/gc-chunks
//!     → MetadataStore::list_live_disk_manifest_ids (DB SELECT)
//!     → engram_chunk_store::gc::run sweep against the live set
//!     → GcChunksResult JSON
//!
//! Seeds the DB with two snapshot rows — one carrying a
//! disk_manifest, one not — and verifies `live_manifest_count`
//! matches.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test admin_gc_chunks_live_pg -- --ignored
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{api, AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{MetadataStore, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{HarnessSpec, SessionSpec};
use engram_core::types::SnapshotRecord;
use engram_core::{SessionId, SnapshotId};
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn gc_chunks_endpoint_reports_live_manifest_count_from_db() {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: ENGRAM_TEST_DATABASE_URL not set");
            return;
        }
    };

    // Build a real Postgres-backed AppState. Shares one blob root
    // between Services.blob + Services.chunk_store so the GC sweep
    // can see the same keyspace the chunk store would write to.
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    let meta: Arc<dyn MetadataStore> = Arc::new(store);

    // Two seeded snapshot rows: one with a disk_manifest ref
    // (chunked write path), one without (legacy/Process backend).
    // The GC live-set sees ONE manifest_id even though there are
    // two snapshots.
    let session_id_a = create_session(&meta, "gc-test-a").await;
    let session_id_b = create_session(&meta, "gc-test-b").await;

    let live_manifest = ManifestRef {
        manifest_id: Uuid::new_v4(),
        version: 3,
    };
    meta.record_snapshot(snapshot_for(session_id_a, Some(live_manifest)))
        .await
        .expect("seed snapshot with manifest");
    meta.record_snapshot(snapshot_for(session_id_b, None))
        .await
        .expect("seed snapshot without manifest");

    // Stand up the API router. Use a fresh local blob root per
    // test run so we don't trip on cruft from prior runs.
    let tmp = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(tmp.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());

    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta,
        cloud: Arc::new(MockCloud::new()),
        sandbox: Arc::new(ProcessBackend::new(sandbox_dir)) as Arc<dyn SandboxBackend>,
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob,
        chunk_store,
        materialize_dir: None,
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-test".into(),
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new_with_registry(
        cfg,
        services,
        Arc::new(HostRegistry::new()),
    ));
    let app = api::router(state);

    // Fire the GC endpoint.
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/gc-chunks")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .expect("call gc-chunks");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // The DB-wide query `SELECT DISTINCT disk_manifest_id` may
    // pick up leftover rows from prior live-PG test runs. The
    // assertion is "at least our one", not "exactly one" — the
    // dev DB isn't a per-test fresh schema.
    let live_count = v["live_manifest_count"]
        .as_u64()
        .expect("live_manifest_count must be a number");
    assert!(
        live_count >= 1,
        "expected at least our seeded manifest in the live set; got {live_count} \
         (body: {v:?})",
    );
    // No chunks were ever written, so even with a live manifest
    // the sweep deletes nothing.
    assert_eq!(v["chunks_deleted"], 0, "no chunks written → none deleted");
}

async fn create_session(meta: &Arc<dyn MetadataStore>, image: &str) -> SessionId {
    meta.create_session(SessionSpec {
        image: image.into(),
        harness: HarnessSpec::None,
        user_id: None,
    })
    .await
    .expect("create session")
}

fn snapshot_for(session_id: SessionId, disk_manifest: Option<ManifestRef>) -> SnapshotRecord {
    SnapshotRecord {
        id: SnapshotId::new(),
        session_id,
        host_id: None,
        local_path: None,
        image_version: "warm-test".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        blob_present: false,
        replicated_at: None,
        disk_manifest,
    }
}
