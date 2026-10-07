//! Two-host post-copy drain wave on real KVM. Each source guest has
//! post-checkpoint RAM and a running harness. Drain and commit move all
//! guests off host A; each harness must continue on host B.
//!
//! The existing test-firecracker CI target runs this file.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AgentSpec, CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

mod common;

/// How many live sessions to pack off host A. Two is enough to prove the
/// wave drains a host holding MORE than one session to fully empty, while
/// bounding CI cost (2 fresh boots on A + 2 restores on B).
const SESSIONS: usize = 2;

struct HostStack {
    pooled: Arc<PooledBackend>,
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
    peer: tokio::task::JoinHandle<()>,
}

fn build_host(
    label: &str,
    kernel: &Path,
    handler: &Path,
    blob_root: &Path,
    bundle_dir: &Path,
    chunk_store: &engram_chunk_store::ChunkStore,
    devices: Vec<PathBuf>,
) -> (Arc<PooledBackend>, tempfile::TempDir) {
    let work = tempfile::Builder::new()
        .prefix(&format!("drainwave-{label}-"))
        .tempdir()
        .expect("host workdir");
    let mut cfg = FirecrackerConfig::with_kernel(kernel.to_path_buf());
    cfg.firecracker_bin = std::env::var_os("ENGRAM_FC_FORK_BIN")
        .expect("gated fork")
        .into();
    cfg.net_pool = None;
    // ADR 0080: both hosts stage the same agentd bundle dir (the fleet mirror).
    cfg.bundle_dir = bundle_dir.to_path_buf();
    cfg.restore_mode = RestoreMode::File;
    cfg.uffd_handler_bin = handler.to_path_buf();
    cfg.uffd_blob_root = Some(blob_root.to_path_buf());
    cfg.uffd_cache_root = Some(work.path().join("chunk-cache"));
    cfg.track_dirty_pages = true;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let inner = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let mut cache_cfg =
        engram_chunk_store::cache::ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 1024 * 1024 * 1024;
    let pooled = PooledBackend::new(inner)
        .with_nbd_pool(
            engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(devices)
                .expect("nbd slot pool"),
        )
        .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
        .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
        .with_checkpoint_dir(work.path().join("checkpoints"));
    (Arc::new(pooled), work)
}

async fn serve(pooled: Arc<PooledBackend>) -> HostStack {
    let peer = common::postcopy::peer(&pooled).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let inner: Arc<dyn engram_core::traits::HostClient> = Arc::new(
        engram_host_agent::host_client::LocalHostClient::with_noop_hub(
            pooled.clone() as Arc<dyn SandboxBackend>
        ),
    );
    let server = tokio::spawn(async move {
        let _ = engram_host_agent::grpc_server::boot(
            addr,
            inner,
            None,
            engram_host_agent::session_epochs::ephemeral(),
            None,
        )
        .await;
    });
    assert!(
        common::wait_tcp_bound(addr, std::time::Duration::from_secs(5)).await,
        "host gRPC server did not bind {addr} within 5s"
    );
    HostStack {
        pooled,
        addr,
        server,
        peer,
    }
}

async fn dial(addr: std::net::SocketAddr) -> engram_protocol::grpc_client::GrpcHostClient {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("dial host");
    engram_protocol::grpc_client::GrpcHostClient::new(channel)
}

/// The mid-run harness stand-in: a monotonic heartbeat into tmpfs. The
/// teleport must REATTACH to this exact process on the destination
/// handshake (pid namespaces move with the VM), not respawn it — same
/// assertion the single-session teleport test makes, here per drained
/// session.
fn heartbeat_argv() -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "trap '' USR1; echo $$ > /dev/shm/harness-pid; i=0; while :; do i=$((i+1)); \
         echo $i > /dev/shm/harness-heartbeat; sleep 0.2; done"
            .into(),
    ]
}

#[tokio::test]
#[ignore = "requires Linux + KVM + root + ENGRAM_FC_FORK_BIN + four NBD devices"]
async fn drain_wave_teleports_every_session_off_host_a() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some(devices) = common::postcopy::nbd_devices(2 * SESSIONS) else {
        return;
    };
    let Some((kernel, handler, agent, busybox)) = gate() else {
        return;
    };
    // Low-disk hosts (the dev VM at >90% used) trip the cache's
    // free-space floor and evict just-staged migration chunks mid-test.
    // The suite runs --test-threads=1, so the process-global env is safe.
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

    // Shared blob store = the GCS stand-in. Everything else per-host.
    let shared = tempfile::tempdir().expect("shared dir");
    let blob_root = shared.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    // Bake once; every session boots the same image (the chunks land in
    // the shared store, so B can restore without A's local cache).
    // Docker-free (ADR 0080 §D).
    let images = tempfile::tempdir().expect("images dir");
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &chunk_store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |_tree| Ok(()),
    )
    .await;

    // ADR 0080: one staged agentd bundle dir shared by both hosts (the
    // fleet stages identical generations).
    let staged = common::stage_agentd_bundle(&shared.path().join("bundles"), &agent);
    let (pooled_a, _work_a) = build_host(
        "a",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
        devices.iter().step_by(2).cloned().collect(),
    );
    let (pooled_b, _work_b) = build_host(
        "b",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
        devices.iter().skip(1).step_by(2).cloned().collect(),
    );
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    // ---- Stand up SESSIONS live sessions on host A ----
    // Each: boot, seed a durable checkpoint, dirty a unique
    // post-checkpoint sentinel (snapshot-rehome would lose it — proves a
    // genuine live move), and start the mid-run heartbeat harness.
    struct Live {
        vm: engram_core::SandboxId,
        sentinel: String,
        harness_pid: String,
        ckpt: engram_core::types::snapshot::SnapshotMetadata,
    }
    let mut live = Vec::with_capacity(SESSIONS);
    let spec = SandboxSpec {
        image: "engram-drain-wave".into(),
        rootfs_source: None,
        image_uri: None,
        // The chunked disk manifest: an NBD-served root is what presetup
        // requires (the disk half of post-copy is the NBD seal).
        rootfs_manifest: Some(
            outcome
                .disk_manifest
                .expect("ext4 bake produces a chunked disk manifest"),
        ),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![staged.agentd_slot()],
        swap_mib: None,
        swap_source: None,
        swap_manifest: None,
    };
    let base = common::postcopy::base(&host_a.pooled, spec).await;
    for i in 0..SESSIONS {
        let vm = common::postcopy::fresh(&host_a.pooled, base.clone()).await;
        let _ = exec(&host_a.pooled, vm, "true").await;
        let ckpt = host_a
            .pooled
            .checkpoint_sandbox(vm)
            .await
            .expect("seed checkpoint on A");
        let sentinel = exec(
            &host_a.pooled,
            vm,
            "head -c 1048576 /dev/urandom > /dev/shm/sentinel \
             && sha256sum /dev/shm/sentinel | cut -d' ' -f1",
        )
        .await
        .trim()
        .to_string();
        assert_eq!(sentinel.len(), 64, "session {i}: sentinel hashed");
        host_a
            .pooled
            .start_agent(
                vm,
                AgentSpec {
                    // ADR 0073: epoch 1 = the test's sole binding generation.
                    binding_epoch: 1,
                    argv: heartbeat_argv(),
                    env: HashMap::new(),
                    session_env: HashMap::new(),
                    host_ca_pem: None,
                },
            )
            .await
            .expect("start mid-run harness on A");
        // Wait for the harness to write its pid file rather than blind-sleeping.
        assert!(
            common::poll_until_async(
                std::time::Duration::from_secs(5),
                std::time::Duration::from_millis(50),
                || {
                    let pooled = host_a.pooled.clone();
                    async move {
                        !exec(&pooled, vm, "cat /dev/shm/harness-pid 2>/dev/null")
                            .await
                            .trim()
                            .is_empty()
                    }
                },
            )
            .await,
            "session {i}: harness pid file never appeared on A within 5s"
        );
        let harness_pid = exec(&host_a.pooled, vm, "cat /dev/shm/harness-pid")
            .await
            .trim()
            .to_string();
        assert!(!harness_pid.is_empty(), "session {i}: harness running on A");
        live.push(Live {
            vm,
            sentinel,
            harness_pid,
            ckpt,
        });
    }
    assert_eq!(
        host_a.pooled.list().await.unwrap().len(),
        SESSIONS,
        "host A holds every session before the wave"
    );

    // ---- The drain wave: teleport each session A → B ----
    // Each source remains alive until both destination drains complete.
    let mut moved_vms = Vec::with_capacity(SESSIONS);
    for (i, s) in live.iter().enumerate() {
        let (moved, cap) =
            common::postcopy::move_guest(&client_a, &client_b, s.vm, s.ckpt.clone(), host_a.addr)
                .await;
        common::postcopy::drain(&host_b.pooled, moved).await;

        // The post-checkpoint sentinel survived the move (a genuine live
        // teleport, not a cold rehome from the checkpoint).
        let check = exec(
            &host_b.pooled,
            moved,
            "sha256sum /dev/shm/sentinel | cut -d' ' -f1",
        )
        .await;
        assert_eq!(
            check.trim(),
            s.sentinel,
            "session {i}: post-checkpoint state survives the move to B"
        );

        // Destination handshake REATTACHES the mid-run harness (same
        // in-guest pid), not respawns it.
        host_b
            .pooled
            .start_agent(
                moved,
                AgentSpec {
                    // ADR 0073: epoch 1 = the test's sole binding generation.
                    binding_epoch: 1,
                    argv: heartbeat_argv(),
                    env: HashMap::new(),
                    session_env: HashMap::new(),
                    host_ca_pem: None,
                },
            )
            .await
            .expect("post-move handshake on B");
        let pid_b = exec(&host_b.pooled, moved, "cat /dev/shm/harness-pid")
            .await
            .trim()
            .to_string();
        assert_eq!(
            s.harness_pid, pid_b,
            "session {i}: the moved harness is REATTACHED (same pid), not respawned"
        );
        let hb1 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
        // Poll for the heartbeat to advance past hb1 instead of blind-sleeping.
        let hb1_val = hb1.trim().to_string();
        assert!(
            common::poll_until_async(
                std::time::Duration::from_secs(5),
                std::time::Duration::from_millis(50),
                || {
                    let pooled = host_b.pooled.clone();
                    let hb1_val = hb1_val.clone();
                    async move {
                        exec(&pooled, moved, "cat /dev/shm/harness-heartbeat")
                            .await
                            .trim()
                            != hb1_val.as_str()
                    }
                },
            )
            .await,
            "session {i}: the harness heartbeat never advanced on B within 5s"
        );
        let hb2 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
        assert_ne!(
            hb1.trim(),
            hb2.trim(),
            "session {i}: the harness keeps RUNNING on B (heartbeat advances)"
        );

        // Commit on A: the frozen source is destroyed. This is what
        // empties the victim host one session at a time.
        client_a
            .migration_commit(
                s.vm,
                &cap.export_id,
                engram_core::traits::SessionFence::unfenced(),
            )
            .await
            .expect("commit on A");
        moved_vms.push(moved);
    }

    // ---- The wave's precondition for node removal ----
    // Host A holds ZERO sandboxes; every session lives on B. Only now is
    // it safe for the operator to remove_node + DELETE the host row.
    let on_a = host_a.pooled.list().await.unwrap();
    assert!(
        on_a.is_empty(),
        "DRAIN COMPLETE: host A must hold zero sandboxes (got {on_a:?})"
    );
    let on_b = host_b.pooled.list().await.unwrap();
    for (i, moved) in moved_vms.iter().enumerate() {
        assert!(
            on_b.contains(moved),
            "session {i} must be alive on the destination after the wave"
        );
    }
    assert_eq!(
        on_b.len(),
        SESSIONS,
        "host B holds every drained session after the wave"
    );

    for moved in moved_vms {
        host_b.pooled.destroy(moved).await.expect("destroy moved");
    }
    host_a.server.abort();
    host_a.peer.abort();
    host_b.server.abort();
    host_b.peer.abort();
}

fn gate() -> Option<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    for bin in ["firecracker", "mksquashfs"] {
        if std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(true)
        {
            eprintln!("SKIP: {bin} not on PATH");
            return None;
        }
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return None;
    };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agent = Path::new(&manifest_dir)
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agent.exists() {
        eprintln!("SKIP: musl engram-agentd not built at {}", agent.display());
        return None;
    }
    let handler = Path::new(&manifest_dir).join("../../target/debug/engram-uffd-handler");
    if !handler.exists() {
        eprintln!("SKIP: engram-uffd-handler not built");
        return None;
    }
    Some((kernel, handler, agent, busybox))
}

async fn exec(backend: &Arc<PooledBackend>, id: engram_core::SandboxId, cmd: &str) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(std::time::Duration::from_secs(30)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => panic!("agent never came up: {e:?}"),
        }
    };
    let mut events = stream.events;
    let mut out = Vec::new();
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => out.extend_from_slice(&b),
            ExecEvent::Exit(_) => break,
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
            ExecEvent::Stderr(_) => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
