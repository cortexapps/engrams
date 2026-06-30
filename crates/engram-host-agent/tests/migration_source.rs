//! ADR 0045 C1: real-FC integration for the live-teleport SOURCE side.
//!
//! Proves on a live microVM:
//!   1. `migration_capture` freezes the guest, mints the next fork-chain
//!      memory manifest WITHOUT publishing it, and lands every transfer
//!      chunk in the host-local NVMe cache (no blob-store PUT).
//!   2. `migration_abort` resumes the guest in place with ZERO loss —
//!      markers written before the capture survive, the guest accepts
//!      new exec, and the next checkpoint diffs on the intact chain
//!      (the aborted capture's unpublished v+1 ref is safely re-minted).
//!   3. The FULL same-host teleport loop over real gRPC (ADR 0045 C1
//!      PR3): capture VM2 -> destination-style restore with a
//!      `migration_source` rider pulls the export over a live tonic
//!      HostService on loopback -> the moved VM carries the
//!      post-checkpoint marker -> `snapshot_wait` drives the durability
//!      catch-up -> commit destroys the frozen source. The two-host
//!      integration's same-host precursor.
//!
//! Wired into ci.yml's `test-firecracker` job.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

mod common;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn migration_capture_freezes_abort_resumes_commit_destroys() {
    // ---- gating (same as checkpoint_chain) ----
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set");
            return;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return;
    }
    for bin in ["firecracker", "docker", "mke2fs"] {
        if std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(true)
        {
            eprintln!("SKIP: {bin} not on PATH");
            return;
        }
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agent = Path::new(&manifest_dir)
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agent.exists() {
        eprintln!("SKIP: musl engram-agentd not built at {}", agent.display());
        return;
    }
    // The teleport-restore leg is UFFD by construction (the migration
    // override forces it regardless of the configured File mode), so
    // the handler binary must exist — same convention as snapshot_uffd.
    // Low-disk hosts (the dev VM at >90% used) trip the cache's
    // free-space floor and evict just-staged migration chunks mid-test.
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");
    let handler = Path::new(&manifest_dir).join("../../target/debug/engram-uffd-handler");
    if !handler.exists() {
        eprintln!(
            "SKIP: engram-uffd-handler not built at {} — run `cargo build -p engram-uffd-handler`",
            handler.display()
        );
        return;
    }

    // ---- 1. Bake an agentd-injected rootfs ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-migration-src-test\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images dir");
    let work = tempfile::tempdir().expect("work dir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store.clone());
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-migration-src-test".into(),
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
        .expect("ext4 bake with agent injection");

    // ---- 2. PooledBackend (FC inner, chunked, dirty tracking) ----
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    cfg.net_pool = None;
    // File for ordinary creates/restores — the migration restore must
    // OVERRIDE to Uffd on its own (the product fix this test pins).
    cfg.restore_mode = RestoreMode::File;
    cfg.uffd_handler_bin = handler;
    // The handler subprocess must reach the SAME blob store + NVMe
    // chunk cache the host-agent uses (prod wires both to shared
    // paths; the dest pull stages divergent chunks into this cache).
    cfg.uffd_blob_root = Some(work.path().join("blob"));
    cfg.uffd_cache_root = Some(work.path().join("chunk-cache"));
    cfg.track_dirty_pages = true;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let inner = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let mut cache_cfg =
        engram_chunk_store::cache::ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 1024 * 1024 * 1024;
    let cache = engram_chunk_store::ChunkCache::new(cache_cfg);
    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
            .with_chunk_cache(cache.clone())
            .with_checkpoint_dir(work.path().join("checkpoints")),
    );

    let spec = SandboxSpec {
        image: "engram-migration-src-test".into(),
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
    let vm = pooled.create(spec.clone()).await.expect("create vm");

    // Full seed checkpoint, then a post-checkpoint marker — the
    // teleport delta the capture must carry.
    let _ = exec(&pooled, vm, "true").await;
    let ckpt1 = pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("seed checkpoint");
    let m1 = ckpt1.memory_manifest.expect("seed manifest");
    let sum = plant_marker(&pooled, vm, 1).await;

    // ---- 3. Capture: frozen + local-only artifacts ----
    let out = pooled.migration_capture(vm).await.expect("capture");
    assert_eq!(
        out.memory_manifest_ref.manifest_id, m1.manifest_id,
        "capture continues the session's fork lineage"
    );
    assert_eq!(out.memory_manifest_ref.version, m1.version + 1);
    assert!(
        !out.new_memory_chunk_hashes.is_empty(),
        "the post-checkpoint marker must be in the transfer set"
    );
    // Unpublished: the v+1 manifest is inline-only.
    assert!(
        chunk_store
            .get_manifest(out.memory_manifest_ref)
            .await
            .is_err(),
        "capture must not publish the manifest (durability is the dest's catch-up)"
    );
    // Every transfer chunk is cache-resident without a store fallback.
    for h in &out.new_memory_chunk_hashes {
        let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(*h);
        cache
            .get(hash, || async {
                Err(engram_chunk_store::error::ChunkStoreError::Internal(
                    "transfer chunk must be cache-resident".into(),
                ))
            })
            .await
            .expect("transfer chunk in local cache");
    }
    // Double-capture refused while the export is open.
    assert!(pooled.migration_capture(vm).await.is_err());

    // ---- 4. Abort: lossless resume ----
    pooled
        .migration_abort(vm, &out.export_id)
        .await
        .expect("abort");
    let check = exec(&pooled, vm, "sha256sum /dev/shm/marker1 | cut -d' ' -f1").await;
    assert_eq!(check.trim(), sum, "marker survives the aborted move");
    // The chain is intact: the next checkpoint diffs and re-mints the
    // never-published v+1 without a version conflict.
    let ckpt2 = pooled
        .checkpoint_sandbox(vm)
        .await
        .expect("post-abort checkpoint");
    let m2 = ckpt2.memory_manifest.expect("post-abort manifest");
    assert_eq!(m2.manifest_id, m1.manifest_id);
    assert_eq!(
        m2.version,
        m1.version + 1,
        "aborted capture's ref re-minted cleanly"
    );
    pooled.destroy(vm).await.expect("destroy vm1");

    // ---- 5. The full same-host teleport loop over real gRPC ----
    // Serve THIS PooledBackend as a HostService on loopback — the
    // "source host". The destination pull dials it exactly as a peer
    // host-agent would.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener); // free the port for tonic (test-local race is fine)
    let serve_inner: Arc<dyn engram_core::traits::HostClient> =
        Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(pooled.clone()));
    tokio::spawn(engram_host_agent::grpc_server::boot(
        addr,
        serve_inner,
        None,
    ));
    assert!(
        common::wait_tcp_bound(addr, std::time::Duration::from_secs(5)).await,
        "source HostService did not bind {addr} within 5s"
    );

    let vm2 = pooled.create(spec).await.expect("create vm2");
    let _ = exec(&pooled, vm2, "true").await;
    let ckpt_v2 = pooled
        .checkpoint_sandbox(vm2)
        .await
        .expect("vm2 seed checkpoint");
    let moved_sum = plant_marker(&pooled, vm2, 7).await;
    let out2 = pooled.migration_capture(vm2).await.expect("capture vm2");

    // Build the destination restore metadata the coordinator (PR4)
    // will assemble: the captured snapshot + the migration rider.
    let mut metadata = ckpt_v2.clone();
    metadata.id = out2.snapshot_id;
    metadata.memory_manifest = Some(out2.memory_manifest_ref);
    metadata.disk_manifest =
        (!out2.disk_manifest_json.is_empty()).then_some(out2.disk_manifest_ref);
    metadata.state_blob_key = None;
    metadata.sidecar_blob_key = None;
    metadata.migration_source = Some(engram_core::types::snapshot::MigrationSourceInfo {
        export_id: out2.export_id.clone(),
        source_addr: format!("http://{addr}"),
        memory_manifest_json: out2.memory_manifest_json.clone(),
        disk_manifest_json: out2.disk_manifest_json.clone(),
        memory_manifest_ref: out2.memory_manifest_ref,
        disk_manifest_ref: out2.disk_manifest_ref,
        new_memory_chunk_hashes: out2.new_memory_chunk_hashes.clone(),
        new_disk_chunk_hashes: out2.new_disk_chunk_hashes.clone(),
        hot_chunks: vec![],
        post_copy: false,
        peer_addr: None,
        peer_token: None,
        sidecar_json: Vec::new(),
    });

    let moved = pooled.restore(metadata).await.expect("teleport restore");
    let check = exec(&pooled, moved, "sha256sum /dev/shm/marker7 | cut -d' ' -f1").await;
    assert_eq!(
        check.trim(),
        moved_sum,
        "the post-checkpoint marker survives the move — the behavioral \
         differentiator vs snapshot-rehome"
    );

    // Durability catch-up: snapshot_wait returns the row metadata once
    // chunks + manifests are store-durable.
    let row = pooled.snapshot_wait(moved).await.expect("catch-up");
    assert_eq!(row.memory_manifest, Some(out2.memory_manifest_ref));
    chunk_store
        .get_manifest(out2.memory_manifest_ref)
        .await
        .expect("memory manifest durable after catch-up");
    for h in &out2.new_memory_chunk_hashes {
        let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(*h);
        chunk_store
            .get_chunk(hash)
            .await
            .expect("transfer chunk durable after catch-up");
    }

    // Commit destroys the frozen source; the moved VM lives on.
    pooled
        .migration_commit(vm2, &out2.export_id)
        .await
        .expect("commit");
    let listed = pooled.list().await.expect("list");
    assert!(!listed.contains(&vm2), "committed source must be destroyed");
    assert!(listed.contains(&moved), "the moved VM survives the commit");
    pooled.destroy(moved).await.expect("destroy moved");
}

async fn plant_marker(backend: &Arc<PooledBackend>, id: engram_core::SandboxId, n: u32) -> String {
    let out = exec(
        backend,
        id,
        &format!(
            "head -c 4194304 /dev/urandom > /dev/shm/marker{n} \
             && sha256sum /dev/shm/marker{n} | cut -d' ' -f1"
        ),
    )
    .await;
    let sum = out.trim().to_string();
    assert_eq!(sum.len(), 64, "expected a sha256, got {sum:?}");
    sum
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
