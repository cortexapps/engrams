//! ADR 0028 Fix A end-to-end: the periodic checkpoint chain through
//! `PooledBackend` against a real Firecracker microVM.
//!
//! What this pins (the whole diff-first pipeline, on real KVM):
//!
//!   1. First `checkpoint_sandbox` = Full capture → chunks guest RAM
//!      into the store (manifest v1), removes the local memory.bin
//!      (ADR 0039 — no rolling memfile) + seeds the chain manifest-only,
//!      writes a durable `CheckpointRecord`.
//!   2. Second checkpoint = **Diff** capture → O(dirty) pause, sparse
//!      re-chunk from prev chunks + the sparse diff (same manifest id,
//!      version 2), second durable record.
//!   3. `restore()` from the *second* checkpoint's metadata on the
//!      same backend (File mode → memory.bin materializes from the
//!      v2 chunked manifest — the sparse manifest must reproduce the
//!      full image) and the guest proves BOTH markers (pre-seed +
//!      pre-diff) byte-identical via sha256 over exec.
//!   4. The durable records re-load from disk (the heartbeat advert
//!      payload) and `delete_acked` clears them.
//!
//! Gating mirrors `exec_real_vm.rs` (Linux + KVM + firecracker +
//! Docker + mke2fs + musl agentd) — the exec verification needs an
//! agentd-baked rootfs. Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test checkpoint_chain -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::checkpoint::CheckpointRecord;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{InitInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn checkpoint_chain_seeds_diffs_and_restores_mid_chain() {
    // ---- gating ----
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
    for bin in ["firecracker", "docker", "mke2fs", "mksquashfs"] {
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
        eprintln!(
            "SKIP: musl engram-agentd not built at {} — \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake an agentd-injected rootfs ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-ckpt-chain-test\"\n",
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
            repo: "engram-ckpt-chain-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            init_injection: Some(InitInjection {
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("ext4 bake with agent injection");

    // ---- 2. PooledBackend with chunk store + checkpoint dir, FC inner ----
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
    // ADR 0028: dirty tracking armed — required for Diff captures.
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
    let records_dir = pooled.checkpoint_records_dir().expect("records dir");

    let spec = SandboxSpec {
        image: "engram-ckpt-chain-test".into(),
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
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
    let sandbox = pooled.create(spec).await.expect("create");
    // The durable record only writes for session-bound sandboxes.
    let session_id = engram_core::SessionId::new();
    pooled.bind_session(session_id, sandbox);

    // ---- 3. Marker 1 → Full (seeding) checkpoint ----
    let sum1 = plant_marker(&pooled, sandbox, 1).await;
    let t = Instant::now();
    let ckpt1 = pooled
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint 1 (Full seed)");
    eprintln!(
        "CHAIN: checkpoint 1 (Full seed) {} ms, manifest {:?}",
        t.elapsed().as_millis(),
        ckpt1.memory_manifest,
    );
    let m1 = ckpt1
        .memory_manifest
        .expect("seed must publish a memory manifest");

    // ADR 0039: the Full capture chunked guest RAM into the store and
    // then removed the local memory.bin — committed snapshot dirs must
    // NOT carry the GiB-scale dump (the 61G prod leak). state.bin stays
    // so the snapshot is still restorable (memory.bin re-materializes
    // from the chunks via the manifest).
    let ckpt1_dir = pooled.snapshot_path_for(ckpt1.id);
    assert!(
        !ckpt1_dir.join("memory.bin").exists(),
        "ADR 0039: Full capture must remove the local memory.bin after chunking",
    );
    assert!(
        ckpt1_dir.join("state.bin").exists(),
        "state.bin must remain for restore",
    );

    // ---- 4. Marker 2 → Diff checkpoint ----
    let sum2 = plant_marker(&pooled, sandbox, 2).await;
    let t = Instant::now();
    let ckpt2 = pooled
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint 2 (Diff)");
    eprintln!(
        "CHAIN: checkpoint 2 (Diff) {} ms, manifest {:?}",
        t.elapsed().as_millis(),
        ckpt2.memory_manifest,
    );
    let m2 = ckpt2
        .memory_manifest
        .expect("diff must publish a memory manifest");
    assert_eq!(
        m2.manifest_id, m1.manifest_id,
        "the chain keeps one manifest id for its lifetime",
    );
    assert_eq!(m2.version, m1.version + 1, "diff ticks the version");

    // ---- 5. Durable records: both load, then ack-delete works ----
    let records = CheckpointRecord::load_all(&records_dir).await;
    assert_eq!(records.len(), 2, "one durable record per checkpoint");
    assert!(records.iter().all(|r| r.session_id == session_id));
    assert!(records.iter().any(|r| r.snapshot_id == ckpt2.id));
    CheckpointRecord::delete_acked(&records_dir, &[ckpt1.id]).await;
    let records = CheckpointRecord::load_all(&records_dir).await;
    assert_eq!(records.len(), 1, "acked record deleted, un-acked kept");

    // ---- 6. Restore from the DIFF checkpoint on the same backend ----
    // File mode materializes memory.bin from the v2 sparse-rechunk
    // manifest — the byte-fidelity proof for update_for_dirty_ranges_sparse
    // against real guest memory. Destroy first (frees the canonical
    // vsock path for the restored sibling).
    pooled.destroy(sandbox).await.expect("destroy original");
    let t = Instant::now();
    let restored = pooled
        .restore(ckpt2.clone())
        .await
        .expect("restore from diff checkpoint");
    eprintln!("CHAIN: restore(ckpt2) {} ms", t.elapsed().as_millis());

    let out = exec(
        &pooled,
        restored,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str()],
        "both markers must survive the chain restore byte-identical",
    );

    // ---- 7. ADR 0038 B2: the restored sandbox's chain was seeded
    // SPARSE on resume (manifest-only, no rolling memfile). Its NEXT
    // checkpoint must therefore be a DIFF via
    // `update_for_dirty_ranges_sparse` — NOT a fresh Full. This is the
    // production path (UFFD idle→active resume → first checkpoint) the
    // 60 s hang lived on; pre-fix it would have re-Full-seeded (a new
    // manifest_id) and faulted the whole working set in.
    let sum3 = plant_marker(&pooled, restored, 3).await;
    let t = Instant::now();
    let ckpt3 = pooled
        .checkpoint_sandbox(restored)
        .await
        .expect("checkpoint 3 (post-resume sparse diff)");
    eprintln!(
        "CHAIN: checkpoint 3 (post-resume sparse diff) {} ms, manifest {:?}",
        t.elapsed().as_millis(),
        ckpt3.memory_manifest,
    );
    let m3 = ckpt3
        .memory_manifest
        .expect("sparse diff must publish a memory manifest");
    assert_eq!(
        m3.manifest_id, m1.manifest_id,
        "resume-seeded chain keeps the source manifest id — proves it DIFFED (sparse), \
         not Full-seeded a fresh chain",
    );
    assert_eq!(m3.version, m2.version + 1, "sparse diff ticks the version");

    // ---- 8. Restore from the SPARSE-built checkpoint and verify all
    // three markers survive byte-identical — the end-to-end byte-
    // fidelity proof for `update_for_dirty_ranges_sparse` against real
    // guest memory (fetch-prev-chunk + apply-diff == current).
    pooled.destroy(restored).await.expect("destroy restored");
    let restored2 = pooled
        .restore(ckpt3.clone())
        .await
        .expect("restore from sparse diff checkpoint");
    let out = exec(
        &pooled,
        restored2,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 /dev/shm/marker3 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str(), sum3.as_str()],
        "all three markers survive the sparse-built checkpoint restore",
    );
    pooled.destroy(restored2).await.expect("destroy restored2");

    // ---- 9. ADR 0045 seed-at-create: a FRESH restore (the
    // base-snapshot create path) seeds the chain by FORKING the source
    // lineage — guest RAM at restore is byte-identical to the source
    // manifest, dirty tracking runs from the restore, but the source
    // manifest is SHARED across sessions, so each chain must own a
    // fresh manifest id. Two fresh restores from the same checkpoint
    // both capture successfully (pre-fix: the second one's diff raced
    // the first to publish src_id@v+1 and died on the chunk store's
    // version conflict — the e2e-stack regression), each on a DISTINCT
    // fresh lineage at v2 (v2 = a diff of the v1 fork; a Full would
    // have minted yet another id at v1).
    let fresh_a = pooled
        .restore_fresh(ckpt3.clone(), Vec::new())
        .await
        .expect("fresh restore A");
    // Marker planted BEFORE B exists: File-mode siblings restored from
    // the same snapshot exhibit a pre-existing interference where guest
    // writes made AFTER a sibling's restore are missing from the next
    // DIFF capture (Full captures are immune, which is why pre-seed-at-
    // create code never saw it; prod fresh creates run the UFFD
    // substrate, not File). Characterized while landing the lineage
    // fork — tracked separately, see issue #172.
    let sum4 = plant_marker(&pooled, fresh_a, 4).await;
    let fresh_b = pooled
        .restore_fresh(ckpt3.clone(), Vec::new())
        .await
        .expect("fresh restore B");
    let t = Instant::now();
    let ckpt_a = pooled
        .checkpoint_sandbox(fresh_a)
        .await
        .expect("checkpoint A (seed-at-create diff)");
    let ckpt_b = pooled
        .checkpoint_sandbox(fresh_b)
        .await
        .expect("checkpoint B (pre-fix: version conflict on the shared lineage)");
    eprintln!(
        "CHAIN: seed-at-create forked diffs {} ms, A {:?} B {:?}",
        t.elapsed().as_millis(),
        ckpt_a.memory_manifest,
        ckpt_b.memory_manifest,
    );
    let ma = ckpt_a
        .memory_manifest
        .expect("forked diff A must publish a memory manifest");
    let mb = ckpt_b
        .memory_manifest
        .expect("forked diff B must publish a memory manifest");
    assert_ne!(
        ma.manifest_id, m1.manifest_id,
        "seed-at-create forks the lineage — never chains on the shared source id",
    );
    assert_ne!(
        ma.manifest_id, mb.manifest_id,
        "each fresh session owns a distinct forked lineage",
    );
    assert_eq!(
        ma.version, 2,
        "v2 of the fork proves the first capture DIFFED (a Full mints a new id at v1)",
    );
    assert_eq!(mb.version, 2, "sibling fork also diffs at v2");

    // Byte-fidelity: restore the forked checkpoint built from A's diff
    // against the fork-published manifest and verify every marker —
    // including marker4, dirtied AFTER the fork seed.
    pooled.destroy(fresh_a).await.expect("destroy fresh_a");
    pooled.destroy(fresh_b).await.expect("destroy fresh_b");
    let restored3 = pooled
        .restore(ckpt_a)
        .await
        .expect("restore from seed-at-create forked checkpoint");
    let out = exec(
        &pooled,
        restored3,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 /dev/shm/marker3 /dev/shm/marker4 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str(), sum3.as_str(), sum4.as_str()],
        "all four markers survive the forked-lineage checkpoint restore",
    );
    pooled.destroy(restored3).await.expect("destroy restored3");
}

async fn plant_marker(backend: &Arc<PooledBackend>, id: engram_core::SandboxId, n: u32) -> String {
    let out = exec(
        backend,
        id,
        &format!(
            "head -c 8388608 /dev/urandom > /dev/shm/marker{n} \
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
        timeout: Some(Duration::from_secs(30)),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("agent never came up: {e:?}"),
        }
    };
    let mut events = stream.events;
    let (mut stdout, mut stderr, mut exit) = (Vec::new(), Vec::new(), None);
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(code) => {
                exit = code;
                break;
            }
        }
    }
    assert_eq!(
        exit,
        Some(0),
        "guest cmd failed: {cmd}\nstderr: {}",
        String::from_utf8_lossy(&stderr),
    );
    String::from_utf8_lossy(&stdout).into_owned()
}
