//! ADR 0045 C2 PR 10: the POST-COPY two-host teleport e2e — the real
//! mechanism end to end, across two complete host-agent stacks, driving
//! the coordinator's exact sequence over the wire.
//!
//! Host A and Host B are fully separate PooledBackend stacks (own
//! workdirs, own NVMe chunk caches, own checkpoint dirs, own jails,
//! own migrate-peer page servers on loopback), sharing only the blob
//! store (the GCS stand-in) — the prod topology. The drive sequence is
//! `live_migration.rs`'s, byte for byte where it matters:
//!
//!   presetup on A (pre-pause; export identity + restore package) →
//!   SPAWN the dest restore on B (its handler parks at A's page server
//!   for the SEAL; its FC load gates on `state.bin`) → capture on A
//!   (THE BLACKOUT: pause → NBD drain → fork-v3 vmstate-only →
//!   pagemap seal) → await the restore → drain_wait on B (every sealed
//!   chunk lands) → commit destroys A's frozen source.
//!
//! Headline arms, all on a CHUNKED-NBD rootfs (the corruption-sensitive
//! production shape — both historical prod disk corruptions were
//! invisible on flat-ext4 fixtures):
//!
//!   1. G2: a tmpfs sentinel dirtied AFTER the last checkpoint
//!      demand-faults across the P2P wire (snapshot-rehome provably
//!      loses it) — and the drain stats PROVE pages crossed peer-to-peer
//!      (`pulled > 0`), not merely that the guest survived.
//!   2. Disk: a rootfs file written after the checkpoint AND an
//!      existing binary read back byte-identical on B through B's NBD,
//!      AFTER the source is destroyed and the guest page cache dropped
//!      — nothing can be served from moved RAM or the frozen source.
//!   3. The mid-run harness reattaches (same in-guest pid, exactly one
//!      instance, heartbeat advances).
//!   4. The guest-observed blackout: an in-guest C spinner samples
//!      CLOCK_MONOTONIC + CLOCK_REALTIME and records the max gap of
//!      each. Monotonic freezes across an FC restore (kvmclock is
//!      restored as-saved), so its gap measures in-VM stall only;
//!      REALTIME is stepped by agentd's PTP steering on resume, so its
//!      gap bounds the human-visible wall blackout from above. Both are
//!      PRINTED for the ledger (R6: measure, never quote); the CI
//!      assertion is a generous ceiling on the REALTIME gap — Blacksmith
//!      nested-virt clocks flake, so the honest numbers come from the
//!      dev-vm/prod runs.
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

/// The in-guest blackout spinner. Compiled ON THE TEST HOST (same
/// arch as the guest) with `cc -static` and COPY'd into the image —
/// no gcc layer in the bake, no dynamic-glibc skew against the
/// debian-slim rootfs. Samples both clocks every ~200µs; whenever
/// either max-gap grows, rewrites `/dev/shm/blackout` ("mono_ns
/// realtime_ns") via tmp+rename so readers never see a torn line.
const SPINNER_C: &str = r#"
#include <stdio.h>
#include <time.h>
static long ns(struct timespec a, struct timespec b) {
    return (b.tv_sec - a.tv_sec) * 1000000000L + (b.tv_nsec - a.tv_nsec);
}
int main(void) {
    struct timespec pm, pr, nm, nr, nap = {0, 200000};
    long max_m = 0, max_r = 0;
    clock_gettime(CLOCK_MONOTONIC, &pm);
    clock_gettime(CLOCK_REALTIME, &pr);
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &nm);
        clock_gettime(CLOCK_REALTIME, &nr);
        long gm = ns(pm, nm), gr = ns(pr, nr);
        int grew = 0;
        if (gm > max_m) { max_m = gm; grew = 1; }
        if (gr > max_r) { max_r = gr; grew = 1; }
        if (grew) {
            FILE *f = fopen("/dev/shm/blackout.tmp", "w");
            if (f) {
                fprintf(f, "%ld %ld\n", max_m, max_r);
                fclose(f);
                rename("/dev/shm/blackout.tmp", "/dev/shm/blackout");
            }
        }
        pm = nm;
        pr = nr;
        nanosleep(&nap, 0);
    }
}
"#;

struct HostStack {
    pooled: Arc<PooledBackend>,
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

/// Two-stack builder: PooledBackend + chunk machinery + a live
/// migrate-peer page server on an ephemeral loopback port (prod runs
/// it on 9102; `presetup` advertises whatever port the server holds).
#[allow(clippy::too_many_arguments)]
fn build_host_with_nbd(
    label: &str,
    kernel: &Path,
    fork_bin: &Path,
    handler: &Path,
    blob_root: &Path,
    chunk_store: &engram_chunk_store::ChunkStore,
    nbd_devices: &[&Path],
) -> (Arc<PooledBackend>, tempfile::TempDir) {
    let work = tempfile::Builder::new()
        .prefix(&format!("postcopy-{label}-"))
        .tempdir()
        .expect("host workdir");
    let mut cfg = FirecrackerConfig::with_kernel(kernel.to_path_buf());
    cfg.net_pool = None;
    // Post-copy is substrate-only, and substrate (uffd_base_file) is a
    // FORK-only load param — the system firecracker rejects it. Both
    // host stacks run the forked binary.
    cfg.firecracker_bin = fork_bin.to_path_buf();
    cfg.restore_mode = RestoreMode::Uffd;
    cfg.uffd_handler_bin = handler.to_path_buf();
    cfg.uffd_blob_root = Some(blob_root.to_path_buf());
    cfg.uffd_cache_root = Some(work.path().join("chunk-cache"));
    // SUBSTRATE restores (ADR 0045 v2b): guest RAM as MAP_PRIVATE of a
    // per-manifest base shm under this dir — the post-copy
    // prerequisite (`post_copy_source_view` reads the FC's base
    // mapping; without this the presetup answers "not a substrate
    // sandbox"). The dir MUST be tmpfs/shmem: UFFD MINOR mode
    // (`UFFDIO_CONTINUE`, the base-share fault) only registers against
    // shmem, so a tempdir on /tmp (ext4) fails FC's snapshot/load with
    // "Failed to register memory address range with the userfaultfd
    // object". Prod uses /var/lib/engram/shm (tmpfs); the test uses
    // /dev/shm. Unique per stack + jitter so parallel/rerun stacks
    // don't collide; the work tempdir's Drop doesn't reach here, so a
    // best-effort rmdir rides the process (tmpfs is reclaimed on
    // reboot regardless).
    let shm_dir = PathBuf::from(format!(
        "/dev/shm/engram-postcopy-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&shm_dir);
    std::fs::create_dir_all(&shm_dir).expect("create base dir on /dev/shm (tmpfs)");
    cfg.uffd_base_dir = Some(shm_dir);
    cfg.track_dirty_pages = true;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let inner = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let mut cache_cfg =
        engram_chunk_store::cache::ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 1024 * 1024 * 1024;
    let cache = engram_chunk_store::ChunkCache::new(cache_cfg);
    let mut pooled = PooledBackend::new(inner)
        .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
        .with_chunk_cache(cache.clone())
        .with_checkpoint_dir(work.path().join("checkpoints"));
    if !nbd_devices.is_empty() {
        pooled = pooled.with_nbd_pool(
            engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(
                nbd_devices.iter().map(|d| d.to_path_buf()).collect(),
            )
            .expect("nbd slot pool"),
        );
    }
    let pooled = Arc::new(pooled);

    // The page server, exactly as lib.rs wires it (PeerServer::new +
    // set on the pooled backend + serve), but on an ephemeral port.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("peer listener");
    std_listener
        .set_nonblocking(true)
        .expect("peer listener nonblocking");
    let peer_port = std_listener.local_addr().expect("peer addr").port();
    let peer = engram_host_agent::migrate_peer::PeerServer::new(
        peer_port,
        Some(cache),
        Some(chunk_store.clone()),
    );
    pooled.set_migrate_peer_server(peer.clone());
    tokio::spawn(async move {
        let listener =
            tokio::net::TcpListener::from_std(std_listener).expect("tokio peer listener");
        let _ = peer.serve_on(listener).await;
    });

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + cc + two /dev/nbd devices"]
async fn migrate_post_checkpoint_dirty_page_demand_faults_across_p2p() {
    if std::env::var("ENGRAM_INTEG_TWO_HOSTS")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        eprintln!("SKIP: ENGRAM_INTEG_TWO_HOSTS=0");
        return;
    }
    let Some((kernel, fork_bin, handler, agent)) = gate() else {
        return;
    };
    // Without a subscriber, RUST_LOG produces nothing — and a hang in
    // the move (capture / dest restore gate / drain) is then invisible.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "engram_host_agent=debug,engram_sandbox_firecracker=debug,engram_uffd_handler=info",
        )
        .with_test_writer()
        .try_init();
    // TWO devices per host. Host A boots the cold seed VM, checkpoints,
    // DESTROYS it, then restores the substrate VM — and NBD teardown is
    // async (the detached netlink-disconnect; /sys/block/nbdN/pid lingers
    // after destroy returns), so a single-device pool would deadlock the
    // restore's slot acquire against the cold VM's lagging teardown. A
    // and B get disjoint device pairs (nbds_max=4 covers nbd0-3).
    let nbd_a: Vec<PathBuf> = vec!["/dev/nbd0".into(), "/dev/nbd2".into()];
    let nbd_b: Vec<PathBuf> = vec!["/dev/nbd1".into(), "/dev/nbd3".into()];
    for dev in nbd_a.iter().chain(nbd_b.iter()) {
        if !dev.exists()
            || std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dev)
                .is_err()
        {
            eprintln!(
                "SKIP: {} missing or unwritable — run `sudo modprobe nbd nbds_max=4` \
                 + chmod 666 /dev/nbd0..3",
                dev.display()
            );
            return;
        }
    }
    // Low-disk hosts trip the cache's free-space floor and evict
    // just-staged migration chunks mid-test. --test-threads=1 makes
    // the process-global env safe.
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

    // Shared blob store = the GCS stand-in. Everything else per-host.
    let shared = tempfile::tempdir().expect("shared dir");
    let blob_root = shared.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    // ---- Bake: debian-slim + the statically-linked blackout spinner ----
    let src = tempfile::tempdir().expect("source dir");
    let spinner_c = src.path().join("spinner.c");
    std::fs::write(&spinner_c, SPINNER_C).unwrap();
    let cc = std::process::Command::new("cc")
        .args(["-O2", "-static", "-o"])
        .arg(src.path().join("blackout-spinner"))
        .arg(&spinner_c)
        .output()
        .expect("cc spawn (build-essential present wherever cargo builds)");
    assert!(
        cc.status.success(),
        "static spinner build failed (need libc.a / libc6-dev): {}",
        String::from_utf8_lossy(&cc.stderr),
    );
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\nCOPY blackout-spinner /usr/local/bin/blackout-spinner\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-postcopy-teleport\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images dir");
    let baker = Builder::new(DockerCli::new(), chunk_store.clone());
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-postcopy-teleport".into(),
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
    let rootfs_manifest = outcome
        .disk_manifest
        .expect("ext4 bake produces a chunked disk manifest");

    let a_devs: Vec<&Path> = nbd_a.iter().map(|p| p.as_path()).collect();
    let b_devs: Vec<&Path> = nbd_b.iter().map(|p| p.as_path()).collect();
    let (pooled_a, _work_a) = build_host_with_nbd(
        "a",
        &kernel,
        &fork_bin,
        &handler,
        &blob_root,
        &chunk_store,
        &a_devs,
    );
    let (pooled_b, _work_b) = build_host_with_nbd(
        "b",
        &kernel,
        &fork_bin,
        &handler,
        &blob_root,
        &chunk_store,
        &b_devs,
    );
    let host_a = serve(pooled_a).await;
    let host_b = serve(pooled_b).await;
    let client_a = dial(host_a.addr).await;
    let client_b = dial(host_b.addr).await;

    // ---- Session on A: chunked-NBD rootfs, like prod ----
    let spec = SandboxSpec {
        image: "engram-postcopy-teleport".into(),
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
        aux_ro_drives: Vec::new(),
    };
    // Cold-create → checkpoint → destroy → UFFD-RESTORE. Post-copy
    // requires a SUBSTRATE sandbox (guest RAM as MAP_PRIVATE of a base
    // shm — that's what the pagemap seal scan and the page server read
    // against). Prod sessions are always substrate-backed because every
    // create is a warm restore from a base snapshot; this is the test's
    // equivalent. The restore also seeds the checkpoint chain
    // (presetup's prerequisite), exactly like a prod resume.
    let cold = host_a.pooled.create(spec).await.expect("create on A");
    let _ = exec(&host_a.pooled, cold, "true").await;
    let ckpt = host_a
        .pooled
        .checkpoint_sandbox(cold)
        .await
        .expect("seed checkpoint on A");
    host_a.pooled.destroy(cold).await.expect("destroy cold VM");
    let vm = host_a
        .pooled
        .restore(ckpt.clone())
        .await
        .expect("substrate restore on A");
    let _ = exec(&host_a.pooled, vm, "true").await;

    // ---- Post-checkpoint state the move must carry ----
    // Memory: a 4 MiB tmpfs sentinel (sealed pages → P2P).
    let sentinel = exec(
        &host_a.pooled,
        vm,
        "head -c 4194304 /dev/urandom > /dev/shm/sentinel \
         && sha256sum /dev/shm/sentinel | cut -d' ' -f1",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(sentinel.len(), 64, "pre-move sentinel hash");
    // Disk: a fresh ROOTFS file (travels the disk leg) + an existing
    // binary (uncached base read on B).
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
    let bin_hash_a = exec(&host_a.pooled, vm, "sha256sum /bin/ls | cut -d' ' -f1")
        .await
        .trim()
        .to_string();
    assert_eq!(bin_hash_a.len(), 64, "pre-move /bin/ls hash");

    // ---- The mid-run harness (the marquee use-case) ----
    host_a
        .pooled
        .start_agent(
            vm,
            engram_core::types::sandbox::AgentSpec {
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
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let harness_pid_a = exec(&host_a.pooled, vm, "cat /dev/shm/harness-pid")
        .await
        .trim()
        .to_string();
    assert!(!harness_pid_a.is_empty(), "harness running on A");

    // ---- The blackout spinner ----
    let armed = exec(
        &host_a.pooled,
        vm,
        "rm -f /dev/shm/blackout; nohup /usr/local/bin/blackout-spinner >/dev/null 2>&1 & \
         sleep 1; test -s /dev/shm/blackout && echo armed",
    )
    .await;
    assert_eq!(armed.trim(), "armed", "spinner armed on A");

    // ==== THE MOVE: the coordinator's exact C2 sequence ====
    // 1. presetup on A (pre-pause).
    let pre = client_a.migration_presetup(vm).await.expect("presetup");
    let peer_addr = format!("127.0.0.1:{}", pre.peer_port);

    // 2. Spawn the dest restore (concurrent with the blackout; the
    //    handler parks at A's page server, FC gates on state.bin).
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: engram_core::types::SnapshotId::new(),
        size_bytes: ckpt.size_bytes,
        created_at: chrono::Utc::now(),
        image_version: ckpt.image_version.clone(),
        base_memory_manifest: None,
        migration_source: Some(MigrationSourceInfo {
            export_id: pre.export_id.clone(),
            source_addr: format!("http://{}", host_a.addr),
            memory_manifest_json: pre.memory_manifest_json.clone(),
            disk_manifest_json: Vec::new(),
            memory_manifest_ref: pre.memory_manifest_ref,
            disk_manifest_ref: pre
                .disk_manifest_ref
                .unwrap_or_else(engram_core::types::manifest::ManifestRef::new),
            new_memory_chunk_hashes: Vec::new(),
            new_disk_chunk_hashes: Vec::new(),
            hot_chunks: pre.hot_chunks.clone(),
            post_copy: true,
            peer_addr: Some(peer_addr),
            peer_token: Some(pre.peer_token.clone()),
            sidecar_json: pre.sidecar_json.clone(),
        }),
        disk_manifest: pre.disk_manifest_ref,
        memory_manifest: Some(pre.memory_manifest_ref),
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: ckpt.aux_bundles.clone(),
    };
    let restore_task = {
        let client_b = client_b.clone();
        tokio::spawn(async move { client_b.restore(metadata).await })
    };

    // 3. THE BLACKOUT: capture on A.
    let t_blackout = std::time::Instant::now();
    let cap = client_a
        .migration_capture_postcopy(vm, &pre.export_id)
        .await
        .expect("post-copy capture on A");
    assert!(cap.sealed_chunks > 0, "a dirtied guest must seal chunks");
    let capture_ms = t_blackout.elapsed().as_millis();

    // 4. Await the restore.
    let moved = restore_task
        .await
        .expect("restore task join")
        .expect("post-copy restore on B");
    let blackout_wall_ms = t_blackout.elapsed().as_millis();
    eprintln!(
        "POSTCOPY: capture {capture_ms} ms (pause {} / disk {} / vmstate {} / scan {}), \
         blackout wall {blackout_wall_ms} ms, sealed {}/{}",
        cap.pause_ms,
        cap.disk_drain_ms,
        cap.vmstate_ms,
        cap.scan_ms,
        cap.sealed_chunks,
        cap.total_chunks,
    );

    // ---- G2: the sentinel demand-faults across P2P ----
    let check = exec(
        &host_b.pooled,
        moved,
        "sha256sum /dev/shm/sentinel | cut -d' ' -f1",
    )
    .await;
    assert_eq!(
        check.trim(),
        sentinel,
        "G2: post-checkpoint memory survives the move (snapshot-rehome loses this)"
    );

    // ---- Drain: every sealed chunk lands; stats PROVE P2P transfer ----
    let drain = client_b
        .migration_drain_wait(moved)
        .await
        .expect("drain_wait on B");
    let engram_core::types::snapshot::DrainOutcome::Done {
        pulled,
        alt_sourced,
        zero_chunks,
        ms,
    } = drain
    else {
        panic!("drain must complete Done, got {drain:?}");
    };
    eprintln!("DRAIN: pulled {pulled} alt_sourced {alt_sourced} zero {zero_chunks} in {ms} ms");
    assert!(
        pulled > 0,
        "sealed pages must cross PEER-TO-PEER (pulled=0 means the demand-fault \
         path silently fell back to something else)"
    );

    // ---- Harness reattach: same pid, one instance, still running ----
    host_b
        .pooled
        .start_agent(
            moved,
            engram_core::types::sandbox::AgentSpec {
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
    assert_eq!(
        harness_pid_a, harness_pid_b,
        "the moved harness must be REATTACHED (same in-guest pid), not respawned"
    );
    let n_instances = exec(
        &host_b.pooled,
        moved,
        "grep -l 'harness[-]heartbeat' /proc/*/cmdline 2>/dev/null | wc -l",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(n_instances, "1", "exactly one harness instance");
    let hb1 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let hb2 = exec(&host_b.pooled, moved, "cat /dev/shm/harness-heartbeat").await;
    assert_ne!(
        hb1.trim(),
        hb2.trim(),
        "the mid-run harness keeps RUNNING on the destination"
    );

    // ---- COMMIT FIRST, then the disk truth ----
    // The frozen source — whose NBD device on a one-machine test would
    // happily keep serving the right bytes — is destroyed before any
    // disk probe runs.
    client_a
        .migration_commit(vm, &pre.export_id)
        .await
        .expect("commit on A");
    assert!(
        !host_a.pooled.list().await.unwrap().contains(&vm),
        "committed source must be destroyed"
    );

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
        "an uncached ROOTFS read on the destination must be byte-identical \
         (prod symptom: zeros / Exec format error)"
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
        "the post-checkpoint rootfs write must survive the move (the disk leg)"
    );

    // ---- The guest-observed blackout ----
    let gaps = exec(&host_b.pooled, moved, "cat /dev/shm/blackout").await;
    let mut parts = gaps.split_whitespace();
    let mono_ns: i64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let real_ns: i64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    eprintln!(
        "GUEST-OBSERVED BLACKOUT: monotonic max-gap {} ms, realtime max-gap {} ms \
         (realtime bounds the human-visible wall; monotonic freezes across the restore)",
        mono_ns / 1_000_000,
        real_ns / 1_000_000,
    );
    assert!(
        real_ns > 0,
        "the spinner must have recorded gaps post-move (did it survive the move?)"
    );
    // CI ceiling, deliberately generous (D13: Blacksmith nested-virt
    // clocks flake; honest numbers come from dev-vm/prod ledgers). The
    // realtime gap includes the blackout + agentd's PTP step lag.
    let ceiling_ms: i64 = std::env::var("ENGRAM_TEST_BLACKOUT_CEILING_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    assert!(
        real_ns / 1_000_000 < ceiling_ms,
        "guest-observed realtime blackout {} ms exceeded the {} ms ceiling",
        real_ns / 1_000_000,
        ceiling_ms,
    );

    host_b.pooled.destroy(moved).await.expect("destroy moved");
    host_a.server.abort();
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
    // Post-copy is substrate-only; substrate (uffd_base_file) is a
    // fork-only FC load param. CI stages the fork at ENGRAM_FC_FORK_BIN
    // (ci.yml "Stage stock + forked firecracker"); the dev-vm runbook
    // exports it. The system firecracker can't run this test.
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set (post-copy substrate is fork-only)");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    for bin in ["firecracker", "docker", "mke2fs", "cc"] {
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
    Some((kernel, fork_bin, handler, agent))
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
