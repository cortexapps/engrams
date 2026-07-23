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
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_core::types::snapshot::MigrationSourceInfo;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

mod common;

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
    bundle_dir: &Path,
    chunk_store: &engram_chunk_store::ChunkStore,
) -> (Arc<PooledBackend>, tempfile::TempDir) {
    build_host_with_nbd(
        label,
        kernel,
        handler,
        blob_root,
        bundle_dir,
        chunk_store,
        None,
    )
}

#[allow(clippy::too_many_arguments)] // cohesive host-fixture inputs
fn build_host_with_nbd(
    label: &str,
    kernel: &Path,
    handler: &Path,
    blob_root: &Path,
    bundle_dir: &Path,
    chunk_store: &engram_chunk_store::ChunkStore,
    nbd_device: Option<&Path>,
) -> (Arc<PooledBackend>, tempfile::TempDir) {
    let work = tempfile::Builder::new()
        .prefix(&format!("teleport-{label}-"))
        .tempdir()
        .expect("host workdir");
    let mut cfg = FirecrackerConfig::with_kernel(kernel.to_path_buf());
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
    let mut pooled = PooledBackend::new(inner)
        .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
        .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
        .with_checkpoint_dir(work.path().join("checkpoints"));
    if let Some(dev) = nbd_device {
        pooled = pooled.with_nbd_pool(
            engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(vec![dev.to_path_buf()])
                .expect("nbd slot pool"),
        );
    }
    (Arc::new(pooled), work)
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

    // Bake once (host A's image; the chunks land in the shared store).
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

    // ADR 0080: one staged agentd bundle dir shared by both hosts.
    let staged = common::stage_agentd_bundle(&shared.path().join("bundles"), &agent);
    let (pooled_a, _work_a) = build_host(
        "a",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
    let (pooled_b, _work_b) = build_host(
        "b",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
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
        aux_ro_drives: vec![staged.agentd_slot()],
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
                // ADR 0073: epoch 1 = the test's sole binding generation.
                binding_epoch: 1,
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "trap '' USR1; echo $$ > /dev/shm/harness-pid; i=0; while :; do i=$((i+1)); \
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
        "harness pid file never appeared on A within 5s"
    );
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
    let cap = client_a
        .migration_capture(vm, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("capture on A");
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
        hot_chunks: vec![],
        post_copy: false,
        peer_addr: None,
        peer_token: None,
        sidecar_json: Vec::new(),
    });

    let t_restore = std::time::Instant::now();
    let moved = client_b
        .restore(metadata, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("restore on B");
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
                // ADR 0073: epoch 1 = the test's sole binding generation.
                binding_epoch: 1,
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "trap '' USR1; echo $$ > /dev/shm/harness-pid; i=0; while :; do i=$((i+1)); \
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
        "the mid-run harness heartbeat never advanced on the destination within 5s"
    );
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
        .migration_commit(
            vm,
            &cap.export_id,
            engram_core::traits::SessionFence::unfenced(),
        )
        .await
        .expect("commit on A");
    assert!(!host_a.pooled.list().await.unwrap().contains(&vm));
    assert!(host_b.pooled.list().await.unwrap().contains(&moved));
    host_b.pooled.destroy(moved).await.expect("destroy moved");
    host_a.server.abort();
    host_b.server.abort();
}

/// A faithful `claude`(libuv) stdin reader for the Phase 4 spike: hold a pipe
/// open, register it with EPOLL, then block in `epoll_wait` for the next line,
/// appending each to OUT. The point is to freeze a real `eppoll_entry` on the
/// pipe's wait queue — the exact machinery flagged by the unmerged ADR 0037
/// draft's File-restore wedge finding (recorded in ADR 0052) — NOT just a
/// blocking `read()` (a strictly weaker property). Compiled static
/// in a throwaway gcc stage so the slim runtime needs no toolchain.
/// argv: `<fifo> <out> <pid> <ready>`.
const EPOLL_READER_C: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 5) { fprintf(stderr, "usage: %s fifo out pid ready\n", argv[0]); return 2; }
    const char *fifo = argv[1], *outp = argv[2], *pidp = argv[3], *readyp = argv[4];

    /* The post-move C1 reattach nudge SIGUSR1s us; default action is terminate,
     * so ignore it (this fake doesn't re-dial — the test drives I/O directly). */
    signal(SIGUSR1, SIG_IGN);

    FILE *pf = fopen(pidp, "w");
    if (!pf) { perror("pid"); return 1; }
    fprintf(pf, "%d\n", (int)getpid());
    fclose(pf);

    unlink(fifo);
    if (mkfifo(fifo, 0600) < 0 && errno != EEXIST) { perror("mkfifo"); return 1; }
    /* O_RDWR holds a write-end too (like the harness holding claude's stdin),
     * so the read side never EOFs/HUPs even after a transient writer closes. */
    int fd = open(fifo, O_RDWR);
    if (fd < 0) { perror("open"); return 1; }

    int ep = epoll_create(1); /* size arg ignored since 2.6.8; no feature-test macro needed */
    if (ep < 0) { perror("epoll_create"); return 1; }
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = fd;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) < 0) { perror("epoll_ctl"); return 1; }

    /* Registration is live now — the test snapshots only after this file
     * exists, so the frozen image holds a real eppoll_entry on the pipe. */
    FILE *rf = fopen(readyp, "w");
    if (rf) { fputs("1\n", rf); fclose(rf); }

    for (;;) {
        struct epoll_event out[8];
        int n = epoll_wait(ep, out, 8, -1); /* BLOCK — the frozen mid-turn state */
        if (n < 0) { if (errno == EINTR) continue; perror("epoll_wait"); return 1; }
        for (int i = 0; i < n; i++) {
            if (!(out[i].events & EPOLLIN)) continue;
            char buf[4096];
            ssize_t r = read(fd, buf, sizeof buf);
            if (r > 0) {
                int of = open(outp, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (of >= 0) { ssize_t w = write(of, buf, (size_t)r); (void)w; close(of); }
            }
        }
    }
}
"#;

/// ADR 0052 Phase 4 (warm mid-turn teleport): the load-bearing unknown the
/// Phase 0 spike had to prove — a process blocked in `epoll_wait` on a held-open
/// pipe (the `claude --input-format stream-json` / libuv shape: stdin held open,
/// an `eppoll_entry` registered on the pipe, the event loop parked waiting for
/// the next user line) survives the live UFFD teleport AND resumes — its epoll
/// fires for a write delivered AFTER the move. This is the EXACT mechanism
/// the unmerged ADR 0037 draft's File-restore wedge finding (recorded in ADR
/// 0052) describes; here we prove it holds across a real two-host UFFD
/// teleport. The sibling reattach arm proves the process
/// survives with its PID; this proves its epoll-registered stdin pipe survives
/// too, so a streaming agent crosses warm mid-turn and keeps consuming input —
/// no turn restart. The harness-engine half (a connection bounce doesn't abort
/// the in-flight turn) is unit-tested in engram-harness-claude's
/// `connection_bounce_mid_turn_preserves_the_run`.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; boots microVMs on two host stacks"]
async fn two_host_live_teleport_held_stdin_pipe_survives() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some((kernel, handler, agent, busybox)) = gate() else {
        return;
    };
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

    let shared = tempfile::tempdir().expect("shared dir");
    let blob_root = shared.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    // ADR 0080 §D docker-free bake: compile the epoll probe statically on the
    // HOST (no gcc:bookworm build stage) and lay it into the busybox rootfs.
    // Gate on a host C compiler.
    let cc = ["cc", "gcc"].into_iter().find_map(|bin| {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(bin))
                .find(|c| c.is_file())
        })
    });
    let Some(cc) = cc else {
        eprintln!("SKIP: no host C compiler (cc/gcc) for the static epoll probe");
        return;
    };
    let scratch = tempfile::tempdir().expect("epoll build scratch");
    let epoll_c = scratch.path().join("epoll_reader.c");
    std::fs::write(&epoll_c, EPOLL_READER_C).unwrap();
    let epoll_bin = scratch.path().join("epoll_reader");
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
        |tree| {
            use std::os::unix::fs::PermissionsExt;
            let status = std::process::Command::new(&cc)
                .args(["-O2", "-static", "-o"])
                .arg(&epoll_bin)
                .arg(&epoll_c)
                .status()?;
            assert!(status.success(), "cc -O2 -static epoll_reader failed");
            let dst = tree.join("usr/local/bin/epoll_reader");
            std::fs::copy(&epoll_bin, &dst)?;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        },
    )
    .await;

    // ADR 0080: one staged agentd bundle dir shared by both hosts.
    let staged = common::stage_agentd_bundle(&shared.path().join("bundles"), &agent);
    let (pooled_a, _work_a) = build_host(
        "pipe-a",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
    let (pooled_b, _work_b) = build_host(
        "pipe-b",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    let spec = SandboxSpec {
        image: "engram-teleport-pipe".into(),
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
        aux_ro_drives: vec![staged.agentd_slot()],
    };
    let vm = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, vm, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint on A");

    // The streaming-stdin claude analog as the harness: the epoll reader holds a
    // FIFO open, registers it with epoll, and parks in `epoll_wait` for the next
    // line. At snapshot time it's frozen in epoll_wait with a live eppoll_entry
    // on the pipe and NOTHING yet consumed — exactly the mid-turn "awaiting the
    // next stdin line" state of a libuv agent.
    const PIPE: &str = "/dev/shm/claude-stdin";
    const OUT: &str = "/dev/shm/claude-out";
    let reader_argv: Vec<String> = vec![
        "/usr/local/bin/epoll_reader".into(),
        PIPE.into(),
        OUT.into(),
        "/dev/shm/reader-pid".into(),
        "/dev/shm/epoll-ready".into(),
    ];
    host_a
        .pooled
        .start_agent(
            vm,
            engram_core::types::sandbox::AgentSpec {
                // ADR 0073: epoch 1 = the test's sole binding generation.
                binding_epoch: 1,
                argv: reader_argv.clone(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("start the epoll stdin reader on A");
    // Wait for the reader to write its pid file rather than blind-sleeping.
    assert!(
        common::poll_until_async(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(50),
            || {
                let pooled = host_a.pooled.clone();
                async move {
                    !exec(&pooled, vm, "cat /dev/shm/reader-pid 2>/dev/null")
                        .await
                        .trim()
                        .is_empty()
                }
            },
        )
        .await,
        "reader pid file never appeared on A within 5s"
    );
    let reader_pid_a = exec(&host_a.pooled, vm, "cat /dev/shm/reader-pid")
        .await
        .trim()
        .to_string();
    assert!(!reader_pid_a.is_empty(), "reader running on A");
    // The epoll registration is live (so the frozen image holds an eppoll_entry
    // on the pipe), and nothing is consumed yet — parked in epoll_wait.
    let ready = exec(&host_a.pooled, vm, "cat /dev/shm/epoll-ready 2>/dev/null")
        .await
        .trim()
        .to_string();
    assert_eq!(
        ready, "1",
        "the reader registered the pipe with epoll before the move"
    );
    let pre = exec(
        &host_a.pooled,
        vm,
        &format!("cat {OUT} 2>/dev/null | wc -c"),
    )
    .await
    .trim()
    .to_string();
    assert_eq!(
        pre, "0",
        "the reader is parked in epoll_wait with an empty output before the move"
    );

    // ---- The move, over the wire ----
    let cap = client_a
        .migration_capture(vm, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("capture on A");
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
        hot_chunks: vec![],
        post_copy: false,
        peer_addr: None,
        peer_token: None,
        sidecar_json: Vec::new(),
    });
    let moved = client_b
        .restore(metadata, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("restore on B");

    // Post-move handshake → the C1 reattach arm (SIGUSR1 ignored by this fake).
    host_b
        .pooled
        .start_agent(
            moved,
            engram_core::types::sandbox::AgentSpec {
                // ADR 0073: epoch 1 = the test's sole binding generation.
                binding_epoch: 1,
                argv: reader_argv.clone(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("post-move handshake on B");
    let reader_pid_b = exec(&host_b.pooled, moved, "cat /dev/shm/reader-pid")
        .await
        .trim()
        .to_string();
    assert_eq!(
        reader_pid_a, reader_pid_b,
        "the epoll reader must be REATTACHED (same in-guest pid), not respawned"
    );

    // THE PROOF: write a line into the held pipe AFTER the move. If the pipe AND
    // the reader's frozen `epoll_wait` (its eppoll_entry on the pipe wait queue)
    // survived the UFFD restore, ep_poll_callback fires, epoll_wait wakes, the
    // reader reads the line and appends the marker — a libuv streaming agent
    // consuming its next stdin line post-teleport, no turn restart.
    const MARKER: &str = "engram-teleport-marker";
    let wrote = exec(
        &host_b.pooled,
        moved,
        &format!("printf '%s\\n' '{MARKER}' > {PIPE} && echo wrote"),
    )
    .await;
    assert_eq!(wrote.trim(), "wrote", "post-move write into the held pipe");
    // Poll for the resumed reader to consume + append the marker rather
    // than blind-sleeping.
    assert!(
        common::poll_until_async(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(50),
            || {
                let pooled = host_b.pooled.clone();
                async move {
                    exec(&pooled, moved, &format!("cat {OUT} 2>/dev/null"))
                        .await
                        .contains(MARKER)
                }
            },
        )
        .await,
        "the resumed reader never appended the post-move marker within 5s"
    );
    let consumed = exec(&host_b.pooled, moved, &format!("cat {OUT}"))
        .await
        .trim()
        .to_string();
    assert_eq!(
        consumed, MARKER,
        "epoll survived the teleport: the reader's frozen epoll_wait woke for a \
         post-move write to the held pipe (the libuv streaming-claude warm-cross \
         guarantee — the unmerged ADR 0037 draft's File-restore wedge finding \
         (recorded in ADR 0052) does NOT occur under UFFD)"
    );

    client_a
        .migration_commit(
            vm,
            &cap.export_id,
            engram_core::traits::SessionFence::unfenced(),
        )
        .await
        .expect("commit on A");
    host_b.pooled.destroy(moved).await.expect("destroy moved");
    host_a.server.abort();
    host_b.server.abort();
}

/// The NBD-rootfs arm (prod canaries 5fa742b7 / 4391e591): production
/// sessions run on a CHUNKED-NBD rootfs, and both prod teleport
/// canaries came out the other side with a corrupt disk — zeros /
/// "Exec format error" on any UNCACHED read — while every memory-side
/// probe passed. The sibling test above boots on a flat ext4 file
/// (`rootfs_source`), so the migration's whole disk leg (pause-window
/// drain, inline-manifest hand-off, dest NBD re-attach, drive
/// re-point) had no real-VM coverage. Worse, on a single machine the
/// frozen source's NBD device keeps serving correct bytes until
/// commit, masking a wrong redirect — so this test COMMITS (source
/// destroyed) BEFORE probing, then drops the guest page cache and
/// re-reads through the destination's NBD. Byte-identical or bust.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + two /dev/nbd devices"]
async fn two_host_teleport_nbd_rootfs_survives_source_destroy() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some((kernel, handler, agent, busybox)) = gate() else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "engram_host_agent=debug,engram_chunk_store=debug,engram_sandbox_firecracker=info",
        )
        .with_test_writer()
        .try_init();
    // One real NBD device per host stack.
    let nbd_a = PathBuf::from("/dev/nbd0");
    let nbd_b = PathBuf::from("/dev/nbd1");
    for dev in [&nbd_a, &nbd_b] {
        if !dev.exists() {
            eprintln!(
                "SKIP: {} not present — run `sudo modprobe nbd nbds_max=4`",
                dev.display()
            );
            return;
        }
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev)
            .is_err()
        {
            eprintln!("SKIP: cannot open {} R/W", dev.display());
            return;
        }
    }
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

    let shared = tempfile::tempdir().expect("shared dir");
    let blob_root = shared.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

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
    let rootfs_manifest = outcome
        .disk_manifest
        .expect("ext4 bake produces a chunked disk manifest");

    // ADR 0080: one staged agentd bundle dir shared by both hosts.
    let staged = common::stage_agentd_bundle(&shared.path().join("bundles"), &agent);
    let (pooled_a, _work_a) = build_host_with_nbd(
        "nbd-a",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
        Some(&nbd_a),
    );
    let (pooled_b, _work_b) = build_host_with_nbd(
        "nbd-b",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
        Some(&nbd_b),
    );
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    // Create on A through the chunked-NBD rootfs path, exactly like a
    // prod session (no flat ext4 file).
    let spec = SandboxSpec {
        image: "engram-teleport-nbd".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: Some(rootfs_manifest),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![staged.agentd_slot()],
    };
    let vm = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, vm, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint on A");

    // Pre-move disk truth: an existing rootfs binary AND a freshly
    // written ROOTFS file (not /dev/shm — it must travel via the disk
    // leg's pause-window drain).
    let bin_hash_a = exec(&host_a.pooled, vm, "sha256sum /bin/ls | cut -d' ' -f1")
        .await
        .trim()
        .to_string();
    assert_eq!(bin_hash_a.len(), 64, "pre-move /bin/ls hash");
    let probe_hash_a = exec(
        &host_a.pooled,
        vm,
        "head -c 8388608 /dev/urandom > /rootfs-probe.bin && sync \
         && sha256sum /rootfs-probe.bin | cut -d' ' -f1",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(probe_hash_a.len(), 64, "pre-move probe hash");

    // ---- The move ----
    let cap = client_a
        .migration_capture(vm, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("capture on A");
    assert!(
        !cap.disk_manifest_json.is_empty(),
        "an NBD-backed source must export its disk manifest"
    );
    let mut metadata = ckpt.clone();
    metadata.id = cap.snapshot_id;
    metadata.memory_manifest = Some(cap.memory_manifest_ref);
    metadata.disk_manifest = Some(cap.disk_manifest_ref);
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
        hot_chunks: vec![],
        post_copy: false,
        peer_addr: None,
        peer_token: None,
        sidecar_json: Vec::new(),
    });
    let moved = client_b
        .restore(metadata, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("restore on B");
    let row = host_b.pooled.snapshot_wait(moved).await.expect("catch-up");
    assert_eq!(row.memory_manifest, Some(cap.memory_manifest_ref));

    // COMMIT FIRST: the frozen source — whose NBD device on a
    // one-machine test would happily keep serving the right bytes —
    // is destroyed before any probe runs.
    client_a
        .migration_commit(
            vm,
            &cap.export_id,
            engram_core::traits::SessionFence::unfenced(),
        )
        .await
        .expect("commit on A");
    assert!(!host_a.pooled.list().await.unwrap().contains(&vm));

    // Post-move disk truth, THROUGH the destination's NBD: drop the
    // guest page cache so nothing is served from the RAM that moved.
    let dropped = exec(
        &host_b.pooled,
        moved,
        "sync && echo 3 > /proc/sys/vm/drop_caches && echo dropped",
    )
    .await;
    assert_eq!(dropped.trim(), "dropped", "page cache dropped on B");
    let bin_hash_b = exec(&host_b.pooled, moved, "sha256sum /bin/ls | cut -d' ' -f1")
        .await
        .trim()
        .to_string();
    assert_eq!(
        bin_hash_b, bin_hash_a,
        "an uncached ROOTFS read on the destination must be \
         byte-identical (prod symptom: zeros / Exec format error)"
    );
    let probe_hash_b = exec(
        &host_b.pooled,
        moved,
        "sha256sum /rootfs-probe.bin | cut -d' ' -f1",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(
        probe_hash_b, probe_hash_a,
        "the session-written rootfs file must survive the move \
         (travels via the pause-window disk drain)"
    );

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
    let Some((kernel, handler, agent, busybox)) = gate() else {
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

    // ADR 0080: one staged agentd bundle dir shared by both hosts.
    let staged = common::stage_agentd_bundle(&shared.path().join("bundles"), &agent);
    let (pooled_a, _work_a) = build_host(
        "ka",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
    let (pooled_b, _work_b) = build_host(
        "kb",
        &kernel,
        &handler,
        &blob_root,
        &staged.bundle_dir,
        &chunk_store,
    );
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
        aux_ro_drives: vec![staged.agentd_slot()],
    };
    let vm = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, vm, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint");
    let cap = client_a
        .migration_capture(vm, engram_core::traits::SessionFence::unfenced())
        .await
        .expect("capture");

    // KILL the source's serving side before the dest pulls — the
    // "source died mid-transfer" arm. Poll until the listener actually
    // stops accepting (connect refused) rather than blind-sleeping, so the
    // dest genuinely pulls against a dead source.
    host_a.server.abort();
    let source_addr = host_a.addr;
    assert!(
        common::poll_until_async(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(20),
            || async move { tokio::net::TcpStream::connect(source_addr).await.is_err() },
        )
        .await,
        "aborted source server still accepted a connection after 5s"
    );

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
        hot_chunks: vec![],
        post_copy: false,
        peer_addr: None,
        peer_token: None,
        sidecar_json: Vec::new(),
    });

    let err = client_b
        .restore(metadata, engram_core::traits::SessionFence::unfenced())
        .await;
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
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
