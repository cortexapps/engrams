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

pub async fn base(
    pooled: &Arc<PooledBackend>,
    spec: engram_core::types::sandbox::SandboxSpec,
) -> SnapshotMetadata {
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
    base.snapshot
}

pub async fn fresh(pooled: &Arc<PooledBackend>, base: SnapshotMetadata) -> SandboxId {
    let vm = pooled
        .restore_fresh(base, Vec::new())
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
    row.disk_manifest = Some(
        presetup
            .disk_manifest_ref
            .expect("post-copy source must have an NBD disk manifest"),
    );
    row.swap_manifest = presetup.swap_manifest_ref;
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

/// Check the fork and a consecutive set of NBD devices before a KVM test.
/// Each live VM needs its own device, including a paused source.
pub fn nbd_devices(count: usize) -> Option<Vec<std::path::PathBuf>> {
    if std::env::var_os("ENGRAM_FC_FORK_BIN").is_none() {
        eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set");
        return None;
    }
    let Some(first) = std::env::var("ENGRAM_TEST_NBD_DEVICE").ok() else {
        eprintln!("SKIP: ENGRAM_TEST_NBD_DEVICE not set");
        return None;
    };
    let prefix = first.trim_end_matches(|c: char| c.is_ascii_digit());
    let Some(start) = first[prefix.len()..].parse::<usize>().ok() else {
        eprintln!("SKIP: NBD device must end in a device number");
        return None;
    };
    let mut devices = Vec::with_capacity(count);
    for offset in 0..count {
        let number = start.checked_add(offset)?;
        let dev = std::path::PathBuf::from(format!("{prefix}{number}"));
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev)
            .is_err()
        {
            eprintln!("SKIP: cannot open {} R/W", dev.display());
            return None;
        }
        wait_until_free(&dev);
        devices.push(dev);
    }
    Some(devices)
}

/// The previous test in this binary destroyed its VMs, but the kernel
/// clears an NBD device's `pid` and zeroes its `size` only once the
/// daemon's socket is gone, which lags the destroy by a moment. Attaching
/// before that returns `EBUSY`. Wait for the kernel's free signal (the
/// same two signals the slot allocator reads) for a bounded time; a
/// device that stays busy still fails loudly at attach.
/// Wait for every device in `devices` to read as free (see
/// [`wait_until_free`]). A test that re-attaches a device it just
/// released calls this between the destroy and the next attach.
pub fn wait_devices_free(devices: &[std::path::PathBuf]) {
    for dev in devices {
        wait_until_free(dev);
    }
}

fn wait_until_free(dev: &std::path::Path) {
    let Some(name) = dev.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let pid_absent = !std::path::Path::new(&format!("/sys/block/{name}/pid")).exists();
        let size_zero = std::fs::read_to_string(format!("/sys/block/{name}/size"))
            .map(|s| s.trim() == "0")
            .unwrap_or(true);
        if pid_absent && size_zero {
            return;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("{} still busy after 30 s; attaching anyway", dev.display());
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}
