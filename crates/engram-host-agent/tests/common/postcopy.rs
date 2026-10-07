//! Shared post-copy fixtures for the two-host and source lifecycle tests.

use engram_core::traits::{SandboxBackend, SessionFence};
use engram_core::types::snapshot::{
    DrainOutcome, MigrationPresetupOut, MigrationSourceInfo, SnapshotMetadata,
};
use engram_core::{SandboxId, SnapshotId};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_protocol::grpc_client::GrpcHostClient;
use std::sync::Arc;

pub async fn peer(pooled: &Arc<PooledBackend>) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = engram_host_agent::migrate_peer::PeerServer::new(addr.port());
    pooled.set_migrate_peer_server(server.clone());
    let task = tokio::spawn(async move {
        server.serve_on(listener).await.unwrap();
    });
    task
}

pub async fn fresh(
    pooled: &Arc<PooledBackend>,
    spec: engram_core::types::sandbox::SandboxSpec,
) -> SandboxId {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let base = pooled
        .build_base_snapshot(
            engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                spec,
                warm: None,
                capture_env: Default::default(),
                capture_egress: None,
                cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
            },
            tx,
        )
        .await
        .expect("build base");
    let vm = pooled
        .restore_fresh(base.snapshot, Vec::new())
        .await
        .expect("File-mode fresh create");
    assert!(
        pooled.post_copy_source_view(vm).is_some(),
        "source RAM must be file-backed"
    );
    vm
}

pub fn metadata(
    mut row: SnapshotMetadata,
    presetup: &MigrationPresetupOut,
    addr: std::net::SocketAddr,
) -> SnapshotMetadata {
    row.id = SnapshotId::new();
    row.memory_manifest = Some(presetup.memory_manifest_ref);
    row.disk_manifest = presetup.disk_manifest_ref;
    row.source_sandbox_id = None;
    row.state_blob_key = None;
    row.sidecar_blob_key = None;
    row.rootfs_blob_key = None;
    row.working_set_blob_key = None;
    row.paused_at = None;
    row.peer_hints.clear();
    row.migration_source = Some(MigrationSourceInfo {
        export_id: presetup.export_id.clone(),
        source_addr: format!("http://{addr}"),
        memory_manifest_json: presetup.memory_manifest_json.clone(),
        memory_manifest_ref: presetup.memory_manifest_ref,
        hot_chunks: presetup.hot_chunks.clone(),
        post_copy: true,
        peer_addr: Some(format!("{}:{}", addr.ip(), presetup.peer_port)),
        peer_token: Some(presetup.peer_token.clone()),
        sidecar_json: presetup.sidecar_json.clone(),
    });
    row
}

pub async fn move_guest(
    source: &GrpcHostClient,
    dest: &GrpcHostClient,
    vm: SandboxId,
    row: SnapshotMetadata,
    addr: std::net::SocketAddr,
) -> (SandboxId, MigrationPresetupOut) {
    let presetup = source
        .migration_presetup(vm, SessionFence::unfenced())
        .await
        .expect("presetup");
    let row = metadata(row, &presetup, addr);
    let dest = dest.clone();
    let restore = tokio::spawn(async move { dest.restore(row, SessionFence::unfenced()).await });
    tokio::task::yield_now().await;
    source
        .migration_capture_postcopy(vm, &presetup.export_id, SessionFence::unfenced())
        .await
        .expect("post-copy capture");
    let moved = restore
        .await
        .expect("restore task")
        .expect("restore on destination");
    (moved, presetup)
}

pub async fn drain(pooled: &Arc<PooledBackend>, vm: SandboxId) {
    assert!(
        matches!(
            pooled.migration_drain_wait(vm).await.expect("drain"),
            DrainOutcome::Done { .. }
        ),
        "both drains must finish before commit"
    );
}
