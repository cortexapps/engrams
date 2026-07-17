//! Issue #225 regression: the SIGTERM path must FINAL-FLUSH every surviving
//! NBD data plane's un-flushed dirty/pending writes BEFORE abandoning it.
//!
//! The hazard: NBD WRITEs are acked to the guest the instant the bytes land in
//! the backend's in-RAM `dirty` tier; durability rides the FlushScheduler's
//! ~30 s / 256 MiB cadence. A routine pod roll calls
//! `abandon_nbd_data_planes_for_shutdown` → `abandon_for_shutdown` →
//! `drop(backend)`, discarding the `dirty` (+ `pending_uploads`) tier with NO
//! final flush. The VM keeps running (the K2 contract), but the successor
//! rehydrates from the last *published* `live_disk_manifest`, which predates
//! the discarded writes — a silent rollback of acked guest I/O on a running VM.
//!
//! The fix adds `PooledBackend::flush_nbd_data_planes_for_shutdown`, run in the
//! SIGTERM path BEFORE the abandon sweep: per surviving sandbox, under a hard
//! deadline, it `wait_idle()`s, runs a full `flush()` (drain → GCS upload →
//! manifest rebase), and SYNCHRONOUSLY publishes the new `live_disk_manifest`
//! to coord (the async publisher's drain task is gone on process exit).
//!
//! This test drives the REAL `PooledBackend::restore` over a real NBD slot on
//! `/dev/nbd0` (mirrors `nbd_shutdown_abandon_race.rs`), lets it install the
//! data plane in `nbd_sandboxes`, then:
//!   1. writes a recognizable marker through the live `ChunkedDiskBackend`
//!      (the acked-from-RAM write the bug discards) WITHOUT flushing,
//!   2. stands up a tiny mock coord recording the `live-manifest` publish,
//!   3. calls `flush_nbd_data_planes_for_shutdown`, then asserts:
//!      - the backend's dirty tier is now EMPTY (the write was drained, not
//!        discarded),
//!      - the new manifest_ref's chunks are durably readable from the chunk
//!        store and reconstruct the marker bytes (the acked write SURVIVED),
//!      - coord received exactly one publish for this sandbox carrying the new
//!        manifest version (so the successor rehydrates from the current ref).
//!
//! On the pre-fix code there is no final-flush method at all; the marker would
//! live only in the dropped RAM tier and the published manifest would never
//! advance — the durability + publish assertions would fail.
//!
//! Gating + run (no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_shutdown_final_flush \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, State};
use axum::routing::post;
use axum::{Json, Router};
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::{BlobStorage, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use engram_host_agent::coord_client::CoordClient;
use engram_host_agent::disk_daemon::NbdSlotAllocator;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_storage_local::LocalBlobStorage;
use tokio::sync::mpsc;

/// Containment (2026-06-18): clear any stale binding a prior, possibly-panicked
/// NBD test left on the SHARED `/dev/nbd0`, so its leak can't surface here as
/// "NBD attach failed" (the cascade that turned one flake into a suite wipeout).
/// Idempotent (no-op when unbound). Full rationale in nbd_netlink_reconfigure.rs.
fn clear_stale_nbd_binding(nbd_path: &std::path::Path) {
    if let Some(idx) = nbd_path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse::<u32>().ok())
    {
        let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(idx);
    }
}

fn preflight() -> Option<PathBuf> {
    let nbd_path = PathBuf::from(
        std::env::var("ENGRAM_TEST_NBD_DEVICE").unwrap_or_else(|_| "/dev/nbd0".to_string()),
    );
    if !nbd_path.exists() {
        eprintln!(
            "SKIP: {} not present — run `sudo modprobe nbd nbds_max=4`",
            nbd_path.display()
        );
        return None;
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&nbd_path)
    {
        Ok(_) => {
            clear_stale_nbd_binding(&nbd_path);
            Some(nbd_path)
        }
        Err(e) => {
            eprintln!(
                "SKIP: cannot open {} R/W: {e} — run as root (`sudo -E`)",
                nbd_path.display()
            );
            None
        }
    }
}

fn nbd_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse().ok())
        .expect("device path must be /dev/nbdN")
}

/// Mock inner FC backend: `restore` returns immediately (we want the data
/// plane INSTALLED, not held in-flight — the opposite of the #224 race test).
struct ImmediateInner {
    staging_root: PathBuf,
    restored_id: SandboxId,
    /// `Some` on the SUCCESSOR side of the spool test:
    /// `rehydrate_sandbox` resolves the survivor's device through
    /// `inner.rootfs_device` (production: the FC sidecar records it).
    rootfs_dev: Option<PathBuf>,
}

#[async_trait]
impl SandboxBackend for ImmediateInner {
    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Err(SandboxError::InvalidSpec("unused".into()))
    }
    async fn exec_stream(&self, _: SandboxId, _: ExecRequest) -> Result<ExecStream, SandboxError> {
        Err(SandboxError::InvalidSpec("unused".into()))
    }
    async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec("unused".into()))
    }
    fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
        self.staging_root.join(id.to_string())
    }
    fn restore_memory_is_lazy_for(&self, _fresh: bool) -> bool {
        true
    }
    fn rootfs_device(&self, id: SandboxId) -> Option<PathBuf> {
        (id == self.restored_id)
            .then(|| self.rootfs_dev.clone())
            .flatten()
    }
    async fn restore(&self, _meta: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        Ok(self.restored_id)
    }
    async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(Vec::new())
    }
}

/// One publish the mock coord recorded.
#[derive(Clone)]
struct RecordedPublish {
    sandbox_id: SandboxId,
    manifest_version: u64,
}

#[derive(serde::Deserialize)]
struct PublishBody {
    sandbox_id: SandboxId,
    manifest_version: u64,
    #[allow(dead_code)]
    manifest_id: uuid::Uuid,
    #[allow(dead_code)]
    session_id: SessionId,
}

async fn live_manifest_handler(
    State(tx): State<mpsc::UnboundedSender<RecordedPublish>>,
    AxumPath(_host_id): AxumPath<String>,
    Json(body): Json<PublishBody>,
) -> Json<serde_json::Value> {
    let _ = tx.send(RecordedPublish {
        sandbox_id: body.sandbox_id,
        manifest_version: body.manifest_version,
    });
    Json(serde_json::json!({ "outcome": "applied" }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + modprobe nbd + writable /dev/nbd0 (root)"]
async fn sigterm_final_flush_persists_survivors_un_flushed_writes() {
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let index = nbd_index(&nbd_path);

    let work = tempfile::tempdir().expect("tempdir");

    // A zeroed disk image chunked into the store; the resume path attaches
    // `metadata.disk_manifest` over NBD.
    let chunk_size = 4 * 1024 * 1024usize;
    let image = work.path().join("disk.img");
    let zeros = vec![0u8; chunk_size];
    std::fs::write(&image, &zeros).expect("write image");

    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = ChunkStore::new(blob);
    let manifest = store
        .chunk_file(&image, ManifestKind::Disk, None)
        .await
        .expect("chunk image");
    let disk_ref = ManifestRef::new();
    store
        .put_manifest(disk_ref, &manifest)
        .await
        .expect("put manifest");

    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);

    // Stage the snapshot dir the resume path materializes into.
    let snapshot_id = engram_core::SnapshotId::new();
    let staging_root = work.path().join("fc-snaps");
    let snap_dir = staging_root.join(snapshot_id.to_string());
    std::fs::create_dir_all(&snap_dir).expect("snap dir");
    let sidecar = serde_json::json!({
        "sandbox_id": uuid::Uuid::new_v4(),
        "created_at": chrono::Utc::now(),
        "spec": {
            "image": "t", "rootfs_source": null, "image_uri": null,
            "harness_pack_uri": null, "cpu": {"vcpus": 1},
            "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
            "ttl": null, "env": {}, "workdir": null,
            "harness_substrate": null, "network": {}
        },
        "format": "fc"
    });
    std::fs::write(
        snap_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");
    std::fs::write(snap_dir.join("state.bin"), b"state").expect("write state");

    let restored_id = SandboxId::new();
    let inner: Arc<dyn SandboxBackend> = Arc::new(ImmediateInner {
        staging_root,
        restored_id,
        rootfs_dev: None,
    });

    // Tiny mock coord recording the live-manifest publish.
    let (tx, mut rx) = mpsc::unbounded_channel::<RecordedPublish>();
    let app = Router::new()
        .route(
            "/api/v1/hosts/:host_id/live-manifest",
            post(live_manifest_handler),
        )
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock coord");
    let coord_addr = listener.local_addr().expect("addr");
    let coord_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // One-device NBD pool over the real /dev/nbd0.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    assert_eq!(pool.capacity(), 1);

    let host_id = engram_core::HostId::new();
    let coord = CoordClient::new(format!("http://{coord_addr}"), None);

    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(store.clone(), work.path().join("mat"))
            .with_chunk_cache(cache)
            .with_nbd_pool(pool.clone())
            .with_live_manifest_coord_publisher(coord, host_id),
    );

    let metadata = SnapshotMetadata {
        id: snapshot_id,
        size_bytes: zeros.len() as u64,
        created_at: chrono::Utc::now(),
        image_version: "t".into(),
        disk_manifest: Some(disk_ref),
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

    // Resume: installs the NBD data plane in `nbd_sandboxes`.
    let restored = pooled.restore(metadata).await.expect("restore");
    assert_eq!(restored, restored_id);
    assert!(
        pooled.__test_nbd_sandbox_registered(restored_id),
        "precondition: the survivor's NBD data plane must be registered",
    );

    // Bind a session so the shutdown flush can resolve a session id for its
    // synchronous coord publish (production: notify_session_policy does this).
    let session_id = SessionId::new();
    pooled.__test_bind_session(restored_id, session_id);

    // The acked-from-RAM write the bug discards. Write a recognizable marker
    // into the first chunk through the live backend; do NOT flush.
    let backend = pooled
        .__test_nbd_backend(restored_id)
        .expect("live backend for survivor");
    let marker = vec![0xABu8; chunk_size];
    backend.write(0, &marker).await.expect("dirty write");
    assert!(
        backend.dirty_bytes().await > 0,
        "precondition: the write must be buffered dirty (acked from RAM, \
         not yet durable) — this is exactly what abandon would discard",
    );
    let pre_flush_version = backend.manifest_ref().await.version;

    // SIGTERM final-flush pass with a generous budget.
    pooled
        .flush_nbd_data_planes_for_shutdown(Duration::from_secs(30))
        .await;

    // ASSERTION 1: the dirty tier was DRAINED, not discarded.
    assert_eq!(
        backend.dirty_bytes().await,
        0,
        "issue #225: the survivor's un-flushed dirty write was not drained by \
         the SIGTERM final-flush pass — abandon would have discarded it",
    );

    // ASSERTION 2: the manifest advanced and its chunks are DURABLE and
    // reconstruct the marker — the acked write survived the shutdown.
    let new_ref = backend.manifest_ref().await;
    assert!(
        new_ref.version > pre_flush_version,
        "issue #225: the manifest_ref did not advance — no durable flush happened",
    );
    let published = store.get_manifest(new_ref).await.expect("durable manifest");
    let first_chunk = published
        .chunks
        .iter()
        .find(|c| c.offset == 0)
        .expect("chunk at offset 0");
    let bytes = store
        .get_chunk(first_chunk.hash)
        .await
        .expect("durable chunk readable from store");
    assert_eq!(
        &bytes[..chunk_size],
        &marker[..],
        "issue #225: the durable chunk does not carry the acked marker bytes — \
         the successor would rehydrate the OLD (rolled-back) data",
    );

    // ASSERTION 3: coord received the publish for this sandbox at the new
    // version, so the successor's rehydrate picks up the current ref.
    let recorded = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("coord publish never arrived")
        .expect("publish channel closed");
    assert_eq!(
        recorded.sandbox_id, restored_id,
        "the publish must name the survivor sandbox",
    );
    assert_eq!(
        recorded.manifest_version, new_ref.version,
        "issue #225: coord must receive the FRESHLY-FLUSHED manifest version, \
         not the stale pre-shutdown one",
    );

    // Now the normal abandon sweep can run — it leaves the kernel device alive.
    let abandoned = pooled.abandon_nbd_data_planes_for_shutdown().await;
    assert_eq!(
        abandoned, 1,
        "the survivor's data plane is abandoned post-flush"
    );

    drop(pooled);
    coord_server.abort();
    // Operator cleanup: disconnect the left-alive device so the harness doesn't
    // leak a bound /dev/nbdN across runs.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(index);
}

/// 2026-07-16 session-85e0298a corruption regression: when the SIGTERM
/// final flush does NOT complete (deadline overrun / GCS unavailable — the
/// prod incident lost 320 MiB of acked writes exactly this way), the
/// abandon sweep must export the un-uploaded dirty tier to the hostPath
/// shutdown spool, and a SUCCESSOR PooledBackend's `rehydrate_sandbox`
/// must adopt it — the guest's acked bytes survive the pod roll instead
/// of being rolled back to the last published manifest.
///
/// Drives the real device handoff: predecessor abandons `/dev/nbd0` with
/// the kernel config left alive (dead connection), successor RECONFIGUREs
/// a fresh socket onto it, seeded from the spool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + modprobe nbd + writable /dev/nbd0 (root)"]
async fn sigterm_overrun_spools_dirty_writes_and_successor_adopts_them() {
    // Debug-visibility for the rehydrate short-circuit branches (all of
    // them log rather than error). Mirrors nbd_netlink_reconfigure.rs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("engram_host_agent=debug,engram_chunk_store=info")
        .with_test_writer()
        .try_init();
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let index = nbd_index(&nbd_path);

    let work = tempfile::tempdir().expect("tempdir");
    let checkpoints = work.path().join("checkpoints");

    let chunk_size = 4 * 1024 * 1024usize;
    let image = work.path().join("disk.img");
    std::fs::write(&image, vec![0u8; chunk_size]).expect("write image");

    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = ChunkStore::new(blob);
    let manifest = store
        .chunk_file(&image, ManifestKind::Disk, None)
        .await
        .expect("chunk image");
    let disk_ref = ManifestRef::new();
    store
        .put_manifest(disk_ref, &manifest)
        .await
        .expect("put manifest");

    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);

    let snapshot_id = engram_core::SnapshotId::new();
    let staging_root = work.path().join("fc-snaps");
    let snap_dir = staging_root.join(snapshot_id.to_string());
    std::fs::create_dir_all(&snap_dir).expect("snap dir");
    let sidecar = serde_json::json!({
        "sandbox_id": uuid::Uuid::new_v4(),
        "created_at": chrono::Utc::now(),
        "spec": {
            "image": "t", "rootfs_source": null, "image_uri": null,
            "harness_pack_uri": null, "cpu": {"vcpus": 1},
            "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
            "ttl": null, "env": {}, "workdir": null,
            "harness_substrate": null, "network": {}
        },
        "format": "fc"
    });
    std::fs::write(
        snap_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&sidecar).unwrap(),
    )
    .expect("write sidecar");
    std::fs::write(snap_dir.join("state.bin"), b"state").expect("write state");

    let restored_id = SandboxId::new();
    let inner: Arc<dyn SandboxBackend> = Arc::new(ImmediateInner {
        staging_root: staging_root.clone(),
        restored_id,
        rootfs_dev: None,
    });

    // Predecessor generation. NO coord publisher wired — the final flush's
    // publish leg is not what this test exercises; checkpoint_dir IS wired
    // (it hosts the shutdown spool).
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(store.clone(), work.path().join("mat"))
            .with_chunk_cache(cache.clone())
            .with_nbd_pool(pool)
            .with_checkpoint_dir(checkpoints.clone()),
    );

    let metadata = SnapshotMetadata {
        id: snapshot_id,
        size_bytes: chunk_size as u64,
        created_at: chrono::Utc::now(),
        image_version: "t".into(),
        disk_manifest: Some(disk_ref),
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
    let restored = pooled.restore(metadata).await.expect("restore");
    assert_eq!(restored, restored_id);

    let session_id = SessionId::new();
    pooled.__test_bind_session(restored_id, session_id);

    // The acked-from-RAM write the incident rolled back. NO flush runs —
    // this models the deadline-overrun / GCS-down shutdown.
    let backend = pooled
        .__test_nbd_backend(restored_id)
        .expect("live backend for survivor");
    let marker = vec![0xCDu8; chunk_size];
    backend.write(0, &marker).await.expect("dirty write");
    let pre_abandon_ref = backend.manifest_ref().await;

    let abandoned = pooled.abandon_nbd_data_planes_for_shutdown().await;
    assert_eq!(abandoned, 1);

    // The spool must carry the acked bytes and the exact lineage they
    // diverge from.
    let spool_root = checkpoints.join("spool");
    let (meta, spooled) =
        engram_host_agent::disk_daemon::spool::read_spool(&spool_root, restored_id)
            .await
            .expect("spool readable")
            .expect("abandon with un-uploaded dirty bytes must write a spool");
    assert_eq!(meta.manifest_ref(), pre_abandon_ref);
    assert_eq!(spooled.len(), 1, "one dirty chunk");
    assert_eq!(spooled[0].0, 0);
    assert_eq!(&spooled[0].1[..], &marker[..]);

    drop(pooled);

    // Successor generation: same node state (store, cache, checkpoints),
    // fresh slot pool over the SAME still-configured device; the mock
    // inner resolves the survivor's rootfs device like the FC sidecar
    // would.
    let successor_inner: Arc<dyn SandboxBackend> = Arc::new(ImmediateInner {
        staging_root,
        restored_id,
        rootfs_dev: Some(nbd_path.clone()),
    });
    let successor_pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let successor = Arc::new(
        PooledBackend::new(successor_inner)
            .with_chunk_store(store.clone(), work.path().join("mat2"))
            .with_chunk_cache(cache)
            .with_nbd_pool(successor_pool)
            .with_checkpoint_dir(checkpoints.clone()),
    );

    // Coord hands back the last PUBLISHED ref (nothing was flushed, so
    // that's still the restore-time ref — the rollback the spool exists
    // to prevent).
    let rehydrated = successor
        .rehydrate_sandbox(session_id, restored_id, pre_abandon_ref)
        .await
        .expect("rehydrate");
    assert!(rehydrated, "successor must re-serve the survivor's device");

    let successor_backend = successor
        .__test_nbd_backend(restored_id)
        .expect("successor backend");
    // Issue #721: probing `dirty_bytes() > 0` here is racy by design of the
    // adoption path — `adopt_unflushed` pokes the threshold notify, and the
    // flush scheduler `rehydrate_sandbox` installs can upload the adopted
    // tier before this probe runs (that prompt upload IS the intended prod
    // behavior). The invariant is "the acked writes were never rolled
    // back", and at every instant the adopted bytes are either still
    // un-uploaded — `export_unflushed` snapshots the dirty AND pending
    // tiers, ref last, so a chunk absent from both was uploaded and the
    // ref read afterwards covers it — or already published past the
    // rollback point on the same lineage.
    let (adopted_ref, unflushed) = successor_backend.export_unflushed().await;
    assert!(
        !unflushed.is_empty()
            || (adopted_ref.manifest_id == pre_abandon_ref.manifest_id
                && adopted_ref.version > pre_abandon_ref.version),
        "2026-07-16 RCA: the successor must ADOPT the spooled dirty tier — \
         no un-uploaded copy and no advanced manifest on the survivor's \
         lineage means the guest's acked writes were rolled back",
    );
    let bytes = successor_backend
        .read(0, chunk_size as u64)
        .await
        .expect("read through successor");
    assert_eq!(
        &bytes[..],
        &marker[..],
        "the successor must serve the ACKED bytes, not the stale base",
    );

    // Adoption consumes the spool.
    assert!(
        engram_host_agent::disk_daemon::spool::read_spool(&spool_root, restored_id)
            .await
            .expect("spool root readable")
            .is_none(),
        "an adopted spool must be discarded",
    );

    drop(successor);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(index);
}
