//! #1003 ladder 4: the spec-based re-serve pass recovers a
//! never-flushed survivor from host-local durable state alone.
//!
//! What this pins: a survivor whose session never published a flush has
//! NO disk manifest in the coordinator's rehydrate row and NO shutdown
//! spool (a SIGKILL writes none) — before the fix, no pass owned its
//! re-serve and the NBD device stayed RECONNECTABLE forever. The fix's
//! pass 3 reads the lineage from `sandbox.json` (stamped at create),
//! maps the sandbox to its session via the binding record, and
//! re-serves through the real `rehydrate_sandbox` path.
//!
//! Externally constructed post-crash state (ADR 0099 H5): the test
//! writes the successor generation's world — a `sandbox.json` with the
//! stamped lineage, a binding record, chunks in the store — and runs
//! the REAL pass over a REAL `/dev/nbdN` device. Sized to the property:
//! one sandbox, one zeroed chunk, no FC boot.
//!
//! Run on the dev-vm / CI NBD lane:
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_spec_reserve -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::{BlobStorage, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{
    CpuLimit, DiskLimit, ExecRequest, ExecStream, MemoryLimit, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use engram_host_agent::bindings::BindingStore;
use engram_host_agent::disk_daemon::NbdSlotAllocator;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_sandbox_firecracker::sandbox_manifest::{
    self, FirecrackerProcessRecord, ProcessRecord, SandboxManifest,
};
use engram_storage_local::LocalBlobStorage;

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

/// Mock inner backend. Generation A uses `restore` (returns the
/// survivor id; PooledBackend::restore installs the REAL NBD data
/// plane around it). The successor generation's reattach has already
/// re-adopted the VM conceptually, so `list` reports the survivor and
/// `rootfs_device` resolves its device — what the real FC backend
/// exposes after reattach.
struct SurvivorInner {
    survivor: SandboxId,
    rootfs_dev: Option<PathBuf>,
    staging_root: PathBuf,
}

#[async_trait]
impl SandboxBackend for SurvivorInner {
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
        (id == self.survivor)
            .then(|| self.rootfs_dev.clone())
            .flatten()
    }
    async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        Ok(self.survivor)
    }
    async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![self.survivor])
    }
}

#[tokio::test]
#[ignore = "requires /dev/nbdN (root); CI NBD lane runs it"]
async fn never_flushed_survivor_is_reserved_from_spec_and_binding() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("engram_host_agent=debug")
        .try_init();
    let Some(nbd_path) = preflight() else { return };
    let work = tempfile::tempdir().expect("tempdir");

    // The store holds the survivor's create-time lineage: one zeroed
    // chunk, exactly what a fresh-create base looks like pre-flush.
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

    // ---- externally constructed post-crash world ----
    let sandbox_id = SandboxId::new();
    let session_id = SessionId::new();
    // sandbox.json: the stamped lineage (what the create path now
    // persists). Process records are irrelevant to this pass.
    let sandbox_dir = work.path().join(sandbox_id.to_string());
    std::fs::create_dir_all(&sandbox_dir).expect("sandbox dir");
    let spec = SandboxSpec {
        image: "spec-reserve-test".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: Some(disk_ref),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
        swap_mib: None,
    };
    sandbox_manifest::write_manifest(
        &sandbox_dir.join("sandbox.json"),
        &SandboxManifest {
            schema_version: sandbox_manifest::SCHEMA_VERSION,
            sandbox_id,
            backend: "firecracker".into(),
            spec,
            firecracker: FirecrackerProcessRecord {
                process: ProcessRecord {
                    pid: 1,
                    start_time_jiffies: 1,
                    comm: "firecracker".into(),
                },
                api_socket: sandbox_dir.join("firecracker.sock"),
                vsock_uds_base: sandbox_dir.join("vsock"),
                vsock_cid: 3,
                rootfs_canonical: sandbox_dir.join("rootfs.ext4"),
                swap_canonical: None,
            },
            network: None,
            netns: None,
            uffd_handler: None,
            migration_role: None,
        },
    )
    .expect("write sandbox.json");
    // The binding record: session ↔ sandbox (written at bind time in
    // prod; survives the roll on disk).
    let bindings = BindingStore::open(work.path().join("bindings")).expect("bindings");
    bindings
        .bind(session_id, sandbox_id, 1)
        .expect("bind survivor");

    // ---- generation A: really serve the device, then "die" ----
    // PooledBackend::restore installs the REAL NBD data plane on
    // /dev/nbdN from metadata.disk_manifest; a guest write dirties it
    // (acked, never flushed). Dropping generation A without destroy is
    // the SIGKILL: the kernel keeps the device bound.
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

    let inner_a: Arc<dyn SandboxBackend> = Arc::new(SurvivorInner {
        survivor: sandbox_id,
        rootfs_dev: None,
        staging_root: staging_root.clone(),
    });
    let pool_a = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);
    let pooled_a = Arc::new(
        PooledBackend::new(inner_a)
            .with_chunk_store(store.clone(), work.path().join("mat"))
            .with_chunk_cache(cache.clone())
            .with_dirty_root(work.path().join("dirty"))
            .with_nbd_pool(pool_a),
    );
    pooled_a.set_self_ref(&pooled_a);
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
        aux_bundles: Default::default(),
        paused_at: None,
        peer_hints: Default::default(),
        working_set_blob_key: None,
    };
    let restored = pooled_a.restore(metadata).await.expect("gen A restore");
    assert_eq!(restored, sandbox_id);
    let marker = vec![0xABu8; 4096];
    let backend_a = pooled_a
        .__test_nbd_backend(sandbox_id)
        .expect("gen A backend");
    backend_a
        .write(0, &marker)
        .await
        .expect("acked dirty write");
    assert!(backend_a.dirty_bytes().await > 0, "write is un-flushed");
    drop(backend_a);
    // The roll. `abandon_for_shutdown` detaches the daemon WITHOUT
    // disconnecting the kernel binding — the state a dead process
    // leaves behind (RECONNECTABLE). A plain drop would run
    // destructors a SIGKILL never runs and disconnect the device.
    // Deliberately NO shutdown flush first: this survivor's writes
    // are acked and never durable anywhere else.
    let abandoned = pooled_a.abandon_nbd_data_planes_for_shutdown().await;
    assert_eq!(abandoned, 1, "gen A abandons its one data plane");
    drop(pooled_a);

    // ---- successor generation ----
    let inner: Arc<dyn SandboxBackend> = Arc::new(SurvivorInner {
        survivor: sandbox_id,
        rootfs_dev: Some(nbd_path.clone()),
        staging_root,
    });
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path]).expect("pool");
    let mut cache_cfg2 = ChunkCacheConfig::new(work.path().join("chunk-cache-b"));
    cache_cfg2.budget_bytes = 64 * 1024 * 1024;
    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(store, work.path().join("mat2"))
            .with_chunk_cache(ChunkCache::new(cache_cfg2))
            .with_dirty_root(work.path().join("dirty"))
            .with_nbd_pool(pool),
    );
    pooled.set_self_ref(&pooled);

    let (rehydrated, failed, unserved) = pooled
        .rehydrate_unserved_from_specs(work.path(), &bindings)
        .await;
    assert_eq!(
        (rehydrated, failed, unserved),
        (1, 0, 0),
        "the spec-based pass must re-serve the never-flushed survivor \
         (rehydrated={rehydrated} failed={failed} unserved={unserved})",
    );

    // The acked, never-flushed write survived the roll: the Recover
    // open re-adopted the dirty overlay and the successor serves it.
    let backend_b = pooled
        .__test_nbd_backend(sandbox_id)
        .expect("successor backend");
    let read_back = backend_b.read(0, 4096).await.expect("read marker");
    assert_eq!(
        &read_back[..],
        &vec![0xABu8; 4096][..],
        "acked write survived"
    );

    // Second run is idempotent: already served, nothing to do.
    let (r2, f2, u2) = pooled
        .rehydrate_unserved_from_specs(work.path(), &bindings)
        .await;
    assert_eq!((r2, f2, u2), (0, 0, 0), "pass must be idempotent");

    pooled.destroy(sandbox_id).await.expect("destroy");
}
