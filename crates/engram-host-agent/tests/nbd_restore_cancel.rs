//! Issue #223 regression: an RPC cancellation between the NBD attach and
//! the `nbd_sandboxes` insert must NOT drop the daemon under a live FC and
//! free the slot for cross-session reuse.
//!
//! The hazard: on the NBD-attach resume path the sidecar is patched to
//! `/dev/nbdN` and the daemon is serving BEFORE `inner.restore` boots the FC
//! against it; the `NbdSandboxState` (daemon + slot lease) only enters
//! `nbd_sandboxes` AFTER restore returns. The gRPC handler ran this inline in
//! a cancellable request future, so a client deadline / coordinator pod
//! restart in that window dropped the local `pending_nbd_state` under the live
//! FC — `NbdHandle::Drop` netlink-disconnected the device the just-restored
//! guest was reading, and `NbdSlot::Drop` returned the path to the pool. A
//! different session's `create` could then CONNECT its own backend onto a
//! device the orphan FC still held open (prod canaries 5fa742b7/4391e591).
//!
//! The fix runs the restore body (through the map insert) inside a
//! `tokio::spawn`ed task whose JoinHandle the caller awaits: cancelling the
//! caller's future stops the await, not the work, so the daemon always lands
//! in `nbd_sandboxes` and normal `destroy()` teardown owns it.
//!
//! This test drives the REAL `PooledBackend::restore` with a real NBD slot
//! pool over `/dev/nbd0` and a mock inner backend whose `restore` blocks on a
//! barrier the test controls. The test:
//!   1. polls the `restore` future once so the NBD attach happens and the
//!      mock `inner.restore` is in-flight,
//!   2. DROPS the `restore` future (simulating handler-future cancellation),
//!   3. releases the barrier so the (detached) restore body can finish, then
//!   4. asserts the sandbox landed in `nbd_sandboxes` and the slot was NOT
//!      freed for reuse.
//!
//! On the pre-fix code the dropped future drops `pending_nbd_state` mid-flight
//! → the sandbox never registers and the slot frees → both assertions fail.
//!
//! Gating + run (mirrors `nbd_netlink_reconfigure.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_restore_cancel \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
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

/// Mock inner FC backend whose `restore` blocks on a barrier so the test can
/// drop the caller's future while the restore body is in-flight. Records the
/// id it returned so the test can assert it registered in `nbd_sandboxes`.
struct BarrierInner {
    staging_root: PathBuf,
    /// Fires when `restore` has been entered (the NBD attach already ran).
    entered: Arc<Notify>,
    /// `restore` waits on this before returning.
    release: Arc<Notify>,
    /// The id `restore` will return (so the test can look it up).
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
    // Resume serves memory lazily via UFFD; skip the memory.bin
    // materialization so the test needn't stage a memory manifest.
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
async fn cancelled_restore_keeps_the_daemon_and_slot() {
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };

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

    // Stage the snapshot dir the resume path materializes into: a minimal FC
    // sidecar `manifest.json` with a `spec` object (patch_sidecar_rootfs_source
    // rewrites `spec.rootfs_source` to /dev/nbdN).
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
        paused_at: None,
        peer_hints: Vec::new(),
    };

    // Run `restore` in a task and CANCEL it (abort) once the NBD attach has
    // happened and the mock `inner.restore` is blocked on the barrier — the
    // exact bug window. `JoinHandle::abort()` drops the task's future at its
    // next await, which is precisely what a dropped gRPC handler future does.
    let entered_wait = entered.notified();
    let handle = {
        let pooled = pooled.clone();
        tokio::spawn(async move { pooled.restore(metadata).await })
    };
    // Wait until the (fix: detached) inner.restore is in-flight past the NBD
    // attach. Bound it so a hang fails the test rather than wedging CI.
    tokio::time::timeout(Duration::from_secs(30), entered_wait)
        .await
        .expect("inner.restore never entered (NBD attach failed?)");

    // CANCEL: abort the restore task. With the fix the restore BODY (attach →
    // restore → insert) runs in a `tokio::spawn`ed task that the aborted
    // handler future merely awaited, so the body survives this abort. Pre-fix
    // the body ran inline on this future and the abort drops `pending_nbd_state`
    // under the live FC.
    handle.abort();
    let _ = handle.await; // observe the cancellation completes

    // Release the barrier so the detached restore body can finish + insert.
    release.notify_one();

    // The fix guarantees the daemon lands in nbd_sandboxes despite the
    // cancellation. Poll for it (the spawned task races our await).
    let mut registered = false;
    for _ in 0..500 {
        if pooled.__test_nbd_sandbox_registered(restored_id) {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        registered,
        "issue #223: a cancelled restore must still register the NBD daemon \
         (the spawned restore body owns it); pre-fix it would drop under the live FC"
    );

    // The slot must NOT have been freed for cross-session reuse — the only
    // device in this single-slot pool is held by the registered sandbox.
    assert_eq!(
        pool.free_count().await,
        0,
        "issue #223: the slot must stay held by the registered sandbox, not \
         freed back to the pool for a different session to CONNECT onto"
    );
    assert_eq!(
        pool.warm_count().await,
        0,
        "issue #223: a held device must never be re-warmed for reuse"
    );

    // Clean teardown: destroy releases the daemon + slot.
    pooled.destroy(restored_id).await.expect("destroy");
}
