//! ADR 0045 C1 PR5: the TWO-HOST live teleport integration — the real
//! mechanism end to end, across two complete host-agent stacks.
//!
//! Host A and Host B are fully separate PooledBackend stacks (own
//! workdirs, own NVMe chunk caches, own checkpoint dirs, own jails),
//! sharing only the blob store (the GCS stand-in) — exactly the prod
//! topology. Each is served by its own gRPC HostService on loopback;
//! every byte of the move crosses the wire.
//!
//! Arms:
//!   1. The G2 headline: a sentinel dirtied AFTER the last checkpoint on
//!      host A survives the move to host B (snapshot-rehome provably
//!      loses it), with the source frozen during the move and destroyed
//!      at commit. Downtime legs printed for the G1 ledger.
//!   2. Durability catch-up on B: `snapshot_wait` proves chunks +
//!      manifest store-durable; B can then checkpoint the moved VM
//!      (diff, continuing the fork lineage).
//!   3. Kill arm `kill_source_mid_pull`: host A's gRPC server dies
//!      between capture and the dest pull → B's restore fails CLEANLY
//!      (no partial sandbox on B), the parachute posture: recovery
//!      belongs to the coordinator's scanner from the last durable row.
//!
//! Gated like the FC suite; wired into ci.yml's `test-firecracker` job.
//! `ENGRAM_INTEG_TWO_HOSTS=0` skips (defaults on where the suite runs).

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_core::types::snapshot::MigrationSourceInfo;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

struct HostStack {
    pooled: Arc<PooledBackend>,
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

fn build_host(
    label: &str,
    kernel: &Path,
    handler: &Path,
    blob_root: &Path,
    chunk_store: &engram_chunk_store::ChunkStore,
) -> (Arc<PooledBackend>, tempfile::TempDir) {
    let work = tempfile::Builder::new()
        .prefix(&format!("teleport-{label}-"))
        .tempdir()
        .expect("host workdir");
    let mut cfg = FirecrackerConfig::with_kernel(kernel.to_path_buf());
    cfg.net_pool = None;
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
    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
            .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
            .with_checkpoint_dir(work.path().join("checkpoints")),
    );
    (pooled, work)
}

async fn serve(pooled: Arc<PooledBackend>) -> HostStack {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let inner: Arc<dyn engram_core::traits::HostClient> = Arc::new(
        engram_host_agent::host_client::LocalHostClient::with_noop_hub(
            pooled.clone() as Arc<dyn SandboxBackend>
        ),
    );
    let server = tokio::spawn(async move {
        let _ = engram_host_agent::grpc_server::boot(addr, inner, None).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    HostStack {
        pooled,
        addr,
        server,
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

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; boots microVMs on two host stacks"]
async fn two_host_live_teleport_preserves_post_checkpoint_state() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some((kernel, handler, agent)) = gate() else {
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

    // Bake once (host A's image; the chunks land in the shared store).
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-two-host-teleport\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images dir");
    let baker = Builder::new(DockerCli::new(), chunk_store.clone());
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-two-host-teleport".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("bake");

    let (pooled_a, _work_a) = build_host("a", &kernel, &handler, &blob_root, &chunk_store);
    let (pooled_b, _work_b) = build_host("b", &kernel, &handler, &blob_root, &chunk_store);
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    // ---- Session lives on A: create, seed checkpoint, dirty a sentinel ----
    let spec = SandboxSpec {
        image: "engram-two-host-teleport".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let vm = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, vm, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint on A");
    let sentinel = exec(
        &host_a.pooled,
        vm,
        "head -c 4194304 /dev/urandom > /dev/shm/sentinel \
         && sha256sum /dev/shm/sentinel | cut -d' ' -f1",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(sentinel.len(), 64);

    // ---- A LIVE harness, mid-run (ADR 0045 C1: the reattach arm) ----
    // The marquee teleport use-case is a session whose harness is
    // ACTIVELY RUNNING. agentd must REATTACH to the moved harness on
    // the post-move handshake — the old kill-and-respawn destroyed the
    // very process the move preserved. A shell-loop harness writing a
    // monotonic heartbeat stands in for claude.
    host_a
        .pooled
        .start_agent(
            vm,
            engram_core::types::sandbox::AgentSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo $$ > /dev/shm/harness-pid; i=0; while :; do i=$((i+1)); \
                     echo $i > /dev/shm/harness-heartbeat; sleep 0.2; done"
                        .into(),
                ],
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("start the mid-run harness on A");
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let harness_pid_a = exec(&host_a.pooled, vm, "cat /dev/shm/harness-pid")
        .await
        .trim()
        .to_string();
    assert!(!harness_pid_a.is_empty(), "harness running on A");
    let alive = exec(
        &host_a.pooled,
        vm,
        &format!("test -d /proc/{harness_pid_a} && echo alive"),
    )
    .await;
    assert_eq!(alive.trim(), "alive", "harness alive on A");

    // ---- The move, over the wire (the G1 downtime legs) ----
    let t_capture = std::time::Instant::now();
    let cap = client_a.migration_capture(vm).await.expect("capture on A");
    let capture_ms = t_capture.elapsed().as_millis();

    let mut metadata = ckpt.clone();
    metadata.id = cap.snapshot_id;
    metadata.memory_manifest = Some(cap.memory_manifest_ref);
    metadata.disk_manifest = (!cap.disk_manifest_json.is_empty()).then_some(cap.disk_manifest_ref);
    metadata.state_blob_key = None;
    metadata.sidecar_blob_key = None;
    metadata.migration_source = Some(MigrationSourceInfo {
        export_id: cap.export_id.clone(),
        source_addr: format!("http://{}", host_a.addr),
        memory_manifest_json: cap.memory_manifest_json.clone(),
        disk_manifest_json: cap.disk_manifest_json.clone(),
        memory_manifest_ref: cap.memory_manifest_ref,
        disk_manifest_ref: cap.disk_manifest_ref,
        new_memory_chunk_hashes: cap.new_memory_chunk_hashes.clone(),
        new_disk_chunk_hashes: cap.new_disk_chunk_hashes.clone(),
    });

    let t_restore = std::time::Instant::now();
    let moved = client_b.restore(metadata).await.expect("restore on B");
    let restore_ms = t_restore.elapsed().as_millis();
    eprintln!(
        "TELEPORT: capture {capture_ms} ms, dest pull+restore {restore_ms} ms, \
         total {} ms",
        capture_ms + restore_ms
    );

    // G2: the post-checkpoint sentinel survives on B.
    let check = exec(
        &host_b.pooled,
        moved,
        "sha256sum /dev/shm/sentinel | cut -d' ' -f1",
    )
    .await;
    assert_eq!(
        check.trim(),
        sentinel,
        "G2: post-checkpoint state survives the move (snapshot-rehome loses this)"
    );

    // ---- The reattach arm: the post-move handshake must NOT kill
    // the mid-run harness. Same in-guest pid (pid namespaces move with
    // the VM), exactly one instance, heartbeat still advancing.
    host_b
        .pooled
        .start_agent(
            moved,
            engram_core::types::sandbox::AgentSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo $$ > /dev/shm/harness-pid; i=0; while :; do i=$((i+1)); \
                     echo $i > /dev/shm/harness-heartbeat; sleep 0.2; done"
                        .into(),
                ],
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("post-move handshake on B");
    let harness_pid_b = exec(&host_b.pooled, moved, "cat /dev/shm/harness-pid")
        .await
        .trim()
        .to_string();
    let alive_b = exec(
        &host_b.pooled,
        moved,
        &format!("test -d /proc/{harness_pid_b} && echo alive"),
    )
    .await;
    assert_eq!(alive_b.trim(), "alive", "the moved harness is alive on B");
    assert_eq!(
        harness_pid_a, harness_pid_b,
        "the moved harness must be REATTACHED (same in-guest pid), not respawned"
    );
    let n_instances = exec(
        &host_b.pooled,
        moved,
        // `[-]` so the probe's own cmdline doesn't match itself.
        "grep -l 'harness[-]heartbeat' /proc/*/cmdline 2>/dev/null | wc -l",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(
        n_instances, "1",
        "exactly one harness instance after the handshake"
    );
    let hb1 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let hb2 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
    assert_ne!(
        hb1.trim(),
        hb2.trim(),
        "the mid-run harness keeps RUNNING on the destination (heartbeat advances)"
    );

    // Durability catch-up on B, then B checkpoints the moved VM (the
    // lineage continues on the new host).
    let row = host_b.pooled.snapshot_wait(moved).await.expect("catch-up");
    assert_eq!(row.memory_manifest, Some(cap.memory_manifest_ref));
    chunk_store
        .get_manifest(cap.memory_manifest_ref)
        .await
        .expect("v+1 durable after catch-up");
    let next = host_b
        .pooled
        .checkpoint_sandbox(moved)
        .await
        .expect("B checkpoints the moved VM");
    let next_ref = next.memory_manifest.expect("manifest");
    assert_eq!(next_ref.manifest_id, cap.memory_manifest_ref.manifest_id);
    assert_eq!(
        next_ref.version,
        cap.memory_manifest_ref.version + 1,
        "the fork lineage continues on the destination"
    );

    // Commit destroys A's frozen source; the moved VM lives on B.
    client_a
        .migration_commit(vm, &cap.export_id)
        .await
        .expect("commit on A");
    assert!(!host_a.pooled.list().await.unwrap().contains(&vm));
    assert!(host_b.pooled.list().await.unwrap().contains(&moved));
    host_b.pooled.destroy(moved).await.expect("destroy moved");
    host_a.server.abort();
    host_b.server.abort();
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; boots microVMs on two host stacks"]
async fn two_host_kill_source_mid_pull_fails_clean_on_dest() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some((kernel, handler, agent)) = gate() else {
        return;
    };
    // Low-disk hosts (the dev VM at >90% used) trip the cache's
    // free-space floor and evict just-staged migration chunks mid-test.
    // The suite runs --test-threads=1, so the process-global env is safe.
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");
    let shared = tempfile::tempdir().expect("shared dir");
    let blob_root = shared.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-kill-source-test\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images dir");
    let baker = Builder::new(DockerCli::new(), chunk_store.clone());
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-kill-source-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("bake");

    let (pooled_a, _work_a) = build_host("ka", &kernel, &handler, &blob_root, &chunk_store);
    let (pooled_b, _work_b) = build_host("kb", &kernel, &handler, &blob_root, &chunk_store);
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    let spec = SandboxSpec {
        image: "engram-kill-source-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let vm = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, vm, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint");
    let cap = client_a.migration_capture(vm).await.expect("capture");

    // KILL the source's serving side before the dest pulls — the
    // "source died mid-transfer" arm.
    host_a.server.abort();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut metadata = ckpt.clone();
    metadata.id = cap.snapshot_id;
    metadata.memory_manifest = Some(cap.memory_manifest_ref);
    metadata.disk_manifest = (!cap.disk_manifest_json.is_empty()).then_some(cap.disk_manifest_ref);
    metadata.state_blob_key = None;
    metadata.sidecar_blob_key = None;
    metadata.migration_source = Some(MigrationSourceInfo {
        export_id: cap.export_id.clone(),
        source_addr: format!("http://{}", host_a.addr),
        memory_manifest_json: cap.memory_manifest_json.clone(),
        disk_manifest_json: cap.disk_manifest_json.clone(),
        memory_manifest_ref: cap.memory_manifest_ref,
        disk_manifest_ref: cap.disk_manifest_ref,
        new_memory_chunk_hashes: cap.new_memory_chunk_hashes.clone(),
        new_disk_chunk_hashes: cap.new_disk_chunk_hashes.clone(),
    });

    let err = client_b.restore(metadata).await;
    assert!(
        err.is_err(),
        "dest restore must fail when the source is gone"
    );
    assert!(
        host_b.pooled.list().await.unwrap().is_empty(),
        "no partial sandbox may remain on the destination"
    );
    // Recovery posture from here is the coordinator's: the session is
    // Evacuating (the parachute) and the scanner rehomes from the last
    // durable checkpoint — exercised in live_migration's unit arms and
    // admin_evac_live_pg. Host-side: A's frozen VM is the export TTL's
    // problem (its process tree is gone in this test).
    host_b.server.abort();
}

fn gate() -> Option<(PathBuf, PathBuf, PathBuf)> {
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
    for bin in ["firecracker", "docker", "mke2fs"] {
        if std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(true)
        {
            eprintln!("SKIP: {bin} not on PATH");
            return None;
        }
    }
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
    Some((kernel, handler, agent))
}

async fn exec(backend: &Arc<PooledBackend>, id: engram_core::SandboxId, cmd: &str) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(std::time::Duration::from_secs(30)),
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
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
