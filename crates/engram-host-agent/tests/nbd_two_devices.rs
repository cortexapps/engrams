//! Two host-side NBD devices share capture, recovery, and destruction.
#![cfg(target_os = "linux")]
// Tests drive a live system; wall clock and OS entropy are inputs (ADR 0098 D1).
#![allow(clippy::disallowed_methods)]

mod common;

use async_trait::async_trait;
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, Manifest, ManifestKind};
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId, SnapshotId};
use engram_host_agent::disk_daemon::{attach_manifest, DiskRole, HostNbdKernel, NbdSlotAllocator};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_storage_local::LocalBlobStorage;
use std::path::PathBuf;
use std::sync::Arc;

struct Inner {
    id: SandboxId,
    root: PathBuf,
    snapshots: PathBuf,
}

#[async_trait]
impl SandboxBackend for Inner {
    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Ok(self.id)
    }
    async fn exec_stream(&self, _: SandboxId, _: ExecRequest) -> Result<ExecStream, SandboxError> {
        Err(SandboxError::InvalidSpec("no guest".into()))
    }
    async fn pause(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn resume(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let id = SnapshotId::new();
        std::fs::create_dir_all(self.snapshot_path_for(id)).unwrap();
        Ok(SnapshotMetadata {
            id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
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
            peer_hints: vec![],
        })
    }
    fn snapshot_path_for(&self, id: SnapshotId) -> PathBuf {
        self.snapshots.join(id.to_string())
    }
    fn rootfs_device(&self, _: SandboxId) -> Option<PathBuf> {
        Some(self.root.clone())
    }
    async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        Ok(self.id)
    }
    async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![self.id])
    }
}

// The kernel bindings remain real. Model the predecessor as a dead process
// so classification must use its durable owner records, not SelfPid.
struct PriorGenerationKernel;
#[async_trait]
impl engram_host_core::NbdKernel for PriorGenerationKernel {
    async fn connect(&self, req: engram_host_core::NbdConnectRequest<'_>) -> std::io::Result<()> {
        HostNbdKernel.connect(req).await
    }
    async fn reconfigure(
        &self,
        req: engram_host_core::NbdReconfigureRequest<'_>,
    ) -> std::io::Result<()> {
        HostNbdKernel.reconfigure(req).await
    }
    async fn disconnect(&self, device: &std::path::Path) -> std::io::Result<()> {
        HostNbdKernel.disconnect(device).await
    }
    fn backend_identifier(&self, device: &std::path::Path) -> Option<String> {
        HostNbdKernel.backend_identifier(device)
    }
    fn connected_devices(&self) -> Vec<engram_host_core::ConnectedDevice> {
        HostNbdKernel
            .connected_devices()
            .into_iter()
            .map(|mut device| {
                device.owner_pid = i32::MAX;
                device
            })
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and two NBD devices"]
async fn two_devices_capture_recover_and_destroy() {
    exercise_two_devices(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and two NBD devices"]
async fn two_devices_shutdown_final_flush() {
    exercise_two_devices(true).await;
}

async fn exercise_two_devices(final_flush: bool) {
    let Some(devices) = common::postcopy::nbd_devices(2) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let id = SandboxId::new();
    let session = SessionId::new();
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blob"))));
    let cache = ChunkCache::new(ChunkCacheConfig::new(dir.path().join("cache")));
    let dirty = dir.path().join("dirty");
    let owners = dir.path().join("owners");
    let checkpoints = dir.path().join("checkpoints");
    let inner = Arc::new(Inner {
        id,
        root: devices[0].clone(),
        snapshots: dir.path().join("snapshots"),
    });
    let (tx, mut publishes) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/api/v1/hosts/{host_id}/live-manifest",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let tx = tx.clone();
            async move {
                tx.send(body).unwrap();
                axum::Json(serde_json::json!({"outcome": "applied"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coord = Arc::new(engram_host_agent::coord_client::HttpCoordClient::new(
        format!("http://{}", listener.local_addr().unwrap()),
        None,
    ));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let host_id = engram_core::HostId::new();
    let build = |pool| {
        PooledBackend::new(inner.clone())
            .with_chunk_store(store.clone(), dir.path().join("materialized"))
            .with_chunk_cache(cache.clone())
            .with_nbd_pool(pool)
            .with_dirty_root(dirty.clone())
            .with_nbd_owner_dir(owners.clone())
            .with_checkpoint_dir(checkpoints.clone())
            .with_live_manifest_coord_publisher(coord.clone(), host_id)
    };
    let pool = NbdSlotAllocator::from_paths(devices.clone()).unwrap();
    let host = build(pool.clone());
    let mut backends = Vec::new();
    for role in DiskRole::ALL {
        let disk_ref = engram_core::ManifestRef::new();
        store
            .put_manifest(disk_ref, &Manifest::empty(ManifestKind::Disk, 1024 * 1024))
            .await
            .unwrap();
        let state = attach_manifest(
            disk_ref,
            cache.clone(),
            Arc::new(store.clone()),
            &pool,
            u64::MAX,
            false,
        )
        .await
        .unwrap();
        backends.push(state.backend.clone());
        host.attach_disk(id, role, state).await.unwrap();
    }
    host.bind_session(session, id);
    for (i, backend) in backends.iter().enumerate() {
        backend.write(0, &[i as u8 + 1; 4096]).await.unwrap();
    }
    let captured = host.snapshot(id).await.unwrap();
    assert_eq!(
        captured.disk_manifest,
        Some(backends[0].manifest_ref().await)
    );
    for backend in &backends {
        assert_eq!(backend.unflushed_bytes().await, 0);
    }
    // New writes must survive the roll without a capture or a flush.
    for (i, backend) in backends.iter().enumerate() {
        backend.write(4096, &[i as u8 + 3; 4096]).await.unwrap();
    }
    if final_flush {
        // Buffered device writes exercise the shutdown per-device sync, too.
        let paths = devices.clone();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::FileExt;
            for (i, path) in paths.iter().enumerate() {
                let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                file.write_all_at(&[i as u8 + 5; 4096], 8192).unwrap();
            }
        })
        .await
        .unwrap();
        host.flush_nbd_data_planes_for_shutdown(std::time::Duration::from_secs(10))
            .await;
        for (i, backend) in backends.iter().enumerate() {
            let manifest = store
                .get_manifest(backend.manifest_ref().await)
                .await
                .unwrap();
            let chunk = manifest
                .chunks
                .iter()
                .find(|chunk| chunk.offset == 0)
                .unwrap();
            let bytes = store.get_chunk(chunk.hash).await.unwrap();
            assert_eq!(&bytes[8192..12288], &[i as u8 + 5; 4096]);
            assert_eq!(backend.unflushed_bytes().await, 0);
        }
        let published = publishes.try_recv().unwrap();
        let root = backends[0].manifest_ref().await;
        assert_eq!(published["manifest_id"], root.manifest_id.to_string());
        assert_eq!(published["manifest_version"], root.version);
        assert!(
            publishes.try_recv().is_err(),
            "swap must not reach the coordinator"
        );
    }
    let root_ref = backends[0].manifest_ref().await;
    assert_eq!(host.abandon_nbd_data_planes_for_shutdown().await, 2);
    drop(backends);
    drop(host);
    let successor = build(NbdSlotAllocator::from_paths(devices.clone()).unwrap());
    let classification = successor
        .classify_startup_slots(&PriorGenerationKernel, &[])
        .await;
    assert!(devices
        .iter()
        .all(|device| classification.reconnect.contains(device)));
    assert!(
        classification.reap.into_devices().is_empty(),
        "owned devices must not be reaped"
    );
    // Root re-serves, but swap has neither a readable ref nor a readable spool.
    // Restore the records and use the production retry worker to finish recovery.
    let swap_ref_path = dirty
        .join(DiskRole::Swap.dirty_file_name(id))
        .with_extension("ref");
    let saved_ref = std::fs::read(&swap_ref_path).unwrap();
    std::fs::write(&swap_ref_path, b"{").unwrap();
    let spool_root = checkpoints.join("spool");
    let swap_spool = spool_root.join(format!("{id}.swap"));
    let saved_spool = spool_root.join("held-swap");
    std::fs::rename(&swap_spool, &saved_spool).unwrap();
    assert!(successor
        .rehydrate_sandbox(session, id, root_ref)
        .await
        .is_err());
    assert_eq!(successor.quarantined_survivors().len(), 1);
    assert!(successor.snapshot(id).await.is_err());
    std::fs::write(swap_ref_path, saved_ref).unwrap();
    std::fs::rename(saved_spool, swap_spool).unwrap();
    assert_eq!(successor.retry_quarantined_rehydrates_once().await, 1);
    assert!(successor.quarantined_survivors().is_empty());
    for (i, device) in devices.iter().enumerate() {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::open(device).unwrap();
        let mut bytes = [0u8; 8192];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes[..4096], &[i as u8 + 1; 4096]);
        assert_eq!(&bytes[4096..], &[i as u8 + 3; 4096]);
    }
    successor.destroy(id).await.unwrap();
    server.abort();
    for role in DiskRole::ALL {
        assert!(!dirty.join(role.dirty_file_name(id)).exists());
    }
}
