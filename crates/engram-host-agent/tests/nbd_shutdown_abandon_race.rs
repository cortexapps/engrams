//! Issue #224 regression: SIGTERM abandon must be a TERMINAL mode, not a
//! one-shot sweep. A registration/rehydrate or in-flight gRPC task whose
//! `nbd_sandboxes.insert` lands AFTER `abandon_nbd_data_planes_for_shutdown`
//! has run must NOT leak a live NBD data plane that then gets
//! netlink-disconnected at process exit.
//!
//! The hazard: `abandon_nbd_data_planes_for_shutdown` originally snapshotted
//! the map's current keys and drained them once. But the restore/create insert
//! sites run inside a `tokio::spawn`ed task (issue #223 cancellation safety)
//! that survives a dropped handler future — and therefore survives a SIGTERM
//! that cancels the handler. Its `inner.restore` window is multi-second
//! (FC restore / GCS manifest fetch / netlink RECONFIGURE). If the abandon
//! sweep ran during that window, the late `insert` put a live
//! `NbdSandboxState` into the map AFTER the sweep had already passed. When
//! `run()` returned and the runtime dropped, that state dropped the NORMAL
//! way: `NbdHandle::Drop` netlink-DISCONNECTED the survivor's live device —
//! exactly the "Disconnected due to user request → successor's RECONFIGURE
//! meets 'not configured'" failure the K2 fix shipped to eliminate (prod
//! 2026-06-11 /dev/nbd4 class).
//!
//! The fix adds an `abandoning: AtomicBool` to `PooledBackend`. The sweep
//! raises it (SeqCst) BEFORE draining; every insert site checks it right
//! before `insert` and `abandon_for_shutdown()`s the state in-place (kernel
//! config left ALIVE for the successor) instead of inserting after the sweep.
//!
//! This test drives the REAL `PooledBackend::restore` with a real NBD slot
//! pool over `/dev/nbd0` and a mock inner backend whose `restore` blocks on a
//! barrier the test controls. The test:
//!   1. polls `restore` until the NBD attach has happened and the mock
//!      `inner.restore` is in-flight (device connected, pid set in /sys),
//!   2. fires `abandon_nbd_data_planes_for_shutdown()` — the SIGTERM sweep,
//!      which raises the terminal flag and finds the map empty of this id,
//!   3. releases the barrier so the (detached) restore body finishes its
//!      flag check, then
//!   4. asserts the late state was ABANDONED, not inserted (so a subsequent
//!      runtime-drop can't disconnect it), AND the device is STILL connected
//!      (`/sys/block/nbdN/pid` present) — i.e. the survivor's data plane was
//!      preserved for the successor, never netlink-disconnected.
//!
//! On the pre-fix code the sweep drains the (empty) map and returns; the late
//! insert lands a live state in `nbd_sandboxes`; when the backend Arc finally
//! drops, `NbdHandle::Drop` disconnects the device (`pid` clears) — the second
//! assertion fails.
//!
//! Gating + run (mirrors `nbd_restore_cancel.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_shutdown_abandon_race \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::{BlobStorage, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId};
use engram_host_agent::disk_daemon::NbdSlotAllocator;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_storage_local::LocalBlobStorage;
use tokio::sync::Notify;

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
        Ok(_) => Some(nbd_path),
        Err(e) => {
            eprintln!(
                "SKIP: cannot open {} R/W: {e} — run as root (`sudo -E`)",
                nbd_path.display()
            );
            None
        }
    }
}

/// The kernel index (`/dev/nbd{N}` → `N`) for the `/sys` pid probe.
fn nbd_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse().ok())
        .expect("device path must be /dev/nbdN")
}

/// Kernel truth: is the NBD device still bound to a serving thread?
/// `/sys/block/nbdN/pid` exists iff the device is CONNECTED. A netlink
/// disconnect clears it. This is the exact signal the slot allocator's
/// free-check uses.
fn device_connected(index: u32) -> bool {
    Path::new(&format!("/sys/block/nbd{index}/pid")).exists()
}

/// Mock inner FC backend whose `restore` blocks on a barrier so the test can
/// fire the SIGTERM abandon sweep while the restore body is in-flight.
struct BarrierInner {
    staging_root: PathBuf,
    /// Fires when `restore` has been entered (the NBD attach already ran).
    entered: Arc<Notify>,
    /// `restore` waits on this before returning.
    release: Arc<Notify>,
    /// The id `restore` will return.
    restored_id: SandboxId,
}

impl BarrierInner {
    fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
        self.staging_root.join(id.to_string())
    }
}

#[async_trait]
impl SandboxBackend for BarrierInner {
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
        self.dir_for(id)
    }
    fn restore_memory_is_lazy_for(&self, _fresh: bool) -> bool {
        true
    }
    async fn restore(&self, _meta: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // The NBD daemon is already serving /dev/nbdN and the sidecar is
        // patched to it by now — exactly the window the bug lives in.
        self.entered.notify_one();
        self.release.notified().await;
        Ok(self.restored_id)
    }
    async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + modprobe nbd + writable /dev/nbd0 (root)"]
async fn abandon_during_in_flight_restore_does_not_disconnect_survivor() {
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let index = nbd_index(&nbd_path);

    let work = tempfile::tempdir().expect("tempdir");

    // A small recognizable disk image chunked into the store; the resume
    // path attaches `metadata.disk_manifest` over NBD.
    let image = work.path().join("disk.img");
    let bytes = vec![7u8; 4 * 1024 * 1024];
    std::fs::write(&image, &bytes).expect("write image");

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

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let restored_id = SandboxId::new();
    let inner = Arc::new(BarrierInner {
        staging_root,
        entered: entered.clone(),
        release: release.clone(),
        restored_id,
    });
    let inner_dyn: Arc<dyn SandboxBackend> = inner;

    // One-device NBD pool over the real /dev/nbd0.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    assert_eq!(pool.capacity(), 1);

    let pooled = Arc::new(
        PooledBackend::new(inner_dyn)
            .with_chunk_store(store.clone(), work.path().join("mat"))
            .with_chunk_cache(cache)
            .with_nbd_pool(pool.clone()),
    );

    let metadata = SnapshotMetadata {
        id: snapshot_id,
        size_bytes: bytes.len() as u64,
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
    };

    // Drive `restore` in a task. It will MOVE its NBD state into a detached
    // spawn (issue #223) and block in `inner.restore` on the barrier.
    let entered_wait = entered.notified();
    let handle = {
        let pooled = pooled.clone();
        tokio::spawn(async move { pooled.restore(metadata).await })
    };
    tokio::time::timeout(Duration::from_secs(30), entered_wait)
        .await
        .expect("inner.restore never entered (NBD attach failed?)");

    // Sanity: the NBD attach happened, so the device must be connected now.
    assert!(
        device_connected(index),
        "precondition: NBD device must be connected after the attach"
    );

    // SIGTERM: the abandon sweep runs while the restore body is in-flight.
    // The state has NOT been inserted yet (the barrier holds inner.restore),
    // so the sweep drains 0 entries — but it MUST raise the terminal flag.
    let abandoned = pooled.abandon_nbd_data_planes_for_shutdown();
    assert_eq!(
        abandoned, 0,
        "the in-flight state is not in the map yet; the sweep drains nothing"
    );

    // Release the barrier so the detached restore body finishes. With the fix
    // it observes the terminal flag at its pre-insert check and
    // `abandon_for_shutdown()`s the state in-place. Pre-fix it inserts a live
    // state into `nbd_sandboxes` after the sweep.
    release.notify_one();
    let restored = tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("restore task hung")
        .expect("restore task panicked")
        .expect("restore returned an error");
    assert_eq!(restored, restored_id, "restore must still return the id");

    // Give the detached body a beat to run its flag check + abandon/insert.
    // The state must NOT be registered: it was abandoned in-place, not
    // inserted after the sweep.
    let mut still_registered = false;
    for _ in 0..50 {
        if pooled.__test_nbd_sandbox_registered(restored_id) {
            still_registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !still_registered,
        "issue #224: a restore completing during SIGTERM abandon must be \
         abandoned in-place, NOT inserted into nbd_sandboxes after the sweep \
         (a post-sweep insert leaks a live data plane that runtime-drop would \
         netlink-disconnect)"
    );

    // THE CORE ASSERTION: the survivor's device must still be CONNECTED. The
    // fix's in-place `abandon_for_shutdown` aborts only the serve task and
    // `mem::forget`s the slot — it never netlink-disconnects, so the kernel
    // config persists for the successor's RECONFIGURE. Pre-fix the late insert
    // would, on the backend Arc's eventual drop, run `NbdHandle::Drop` →
    // disconnect, clearing the pid.
    //
    // Drop our remaining handle to the pooled backend to force any straggler
    // NbdSandboxState to drop the NORMAL way (the bug's exit path). The device
    // must SURVIVE this — that's the whole K2 contract.
    drop(pooled);
    // Settle any detached drop work.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        device_connected(index),
        "issue #224: the survivor's NBD device was netlink-DISCONNECTED at \
         shutdown — the late-completing restore leaked a live data plane past \
         the abandon sweep and runtime-drop tore it down, exactly the \
         'Disconnected due to user request' K2 regression"
    );

    // Operator cleanup: the device is intentionally left connected for the
    // successor; disconnect it so the test harness doesn't leak a bound
    // /dev/nbdN across test runs.
    let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(index);
}
