//! Durable chain-head rehydrate across a simulated host-agent pod roll,
//! against a REAL Firecracker microVM (2026-07-13 incident).
//!
//! What this pins:
//!
//!   1. A survivor VM's checkpoint chain is re-seeded from the durable
//!      `ChainHeadRecord` by generation B (`rehydrate_chain_heads` after
//!      `live_attach::reattach_pass` — the real startup sequence), so
//!      its first post-roll capture is an O(dirty-set) DIFF, not the
//!      FULL multi-GiB re-chunk the incident's evict paid.
//!   2. Byte fidelity of that roll-spanning diff: a marker dirtied
//!      between the pre-roll seed and the post-roll capture restores
//!      byte-identical — proving the surviving VM's KVM dirty-bitmap
//!      baseline really is the recorded chain head (the whole premise
//!      of re-seeding from host-local state).
//!   3. The torn-capture property: a diff that fails AFTER FC consumed
//!      the dirty bitmap (post-processing dies, standing in for the
//!      incident's 22:23:46 shutdown-torn capture) leaves NO chain-head
//!      record — the write-ahead invalidate ran before the create — so
//!      the next generation seeds nothing and captures a safe FULL on a
//!      fresh lineage. Seeding here from any earlier manifest (e.g. the
//!      coordinator's latest snapshots row) would corrupt.
//!
//! Sized to the property, not to realism: one guest, small markers,
//! two roll simulations, no long sleeps.
//!
//! Gating mirrors `checkpoint_chain.rs` (Linux + KVM + firecracker +
//! mke2fs + musl agentd). Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test chain_rehydrate -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::checkpoint::ChainHeadRecord;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

/// A `BlobStorage` whose `put_streaming` fails while armed — the torn
/// capture's post-processing failure (the sparse re-chunk's chunk PUT),
/// AFTER the FC diff create consumed the dirty bitmap.
struct FailPutsWhileArmed {
    inner: engram_storage_local::LocalBlobStorage,
    armed: AtomicBool,
}
#[async_trait::async_trait]
impl engram_core::traits::BlobStorage for FailPutsWhileArmed {
    async fn put_streaming(
        &self,
        key: &str,
        body: engram_core::traits::ByteStream,
    ) -> Result<u64, engram_core::error::BlobError> {
        if self.armed.load(Ordering::SeqCst) {
            return Err(engram_core::error::BlobError::Protocol(
                "torn-capture fixture: puts disabled".into(),
            ));
        }
        self.inner.put_streaming(key, body).await
    }
    async fn get_streaming(
        &self,
        key: &str,
    ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
        self.inner.get_streaming(key).await
    }
    async fn head(
        &self,
        key: &str,
    ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, engram_core::error::BlobError> {
        self.inner.list_prefix(prefix).await
    }
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots microVMs"]
async fn survivor_chain_rehydrates_across_a_roll_and_torn_captures_fall_back_to_full() {
    // ---- gating (same as checkpoint_chain.rs) ----
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
    for bin in ["firecracker", "mke2fs", "mksquashfs"] {
        if std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(true)
        {
            eprintln!("SKIP: {bin} not on PATH");
            return;
        }
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
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

    // ---- 1. Bake an agentd-injected rootfs (docker-free, ADR 0080 §D) ----
    let images = tempfile::tempdir().expect("images dir");
    let work = tempfile::tempdir().expect("work dir");
    let fail_puts = Arc::new(FailPutsWhileArmed {
        inner: engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
        armed: AtomicBool::new(false),
    });
    let blob: Arc<dyn engram_core::traits::BlobStorage> = fail_puts.clone();
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
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

    // ---- 2. Generation A ----
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
    cfg.track_dirty_pages = true;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let checkpoint_dir = work.path().join("checkpoints");
    let chains_dir = ChainHeadRecord::subdir(&checkpoint_dir);
    let make_pooled = |inner: Arc<FirecrackerBackend>| {
        let p = Arc::new(
            PooledBackend::new(inner)
                .with_chunk_store(chunk_store.clone(), work.path().join("materialize"))
                .with_checkpoint_dir(checkpoint_dir.clone()),
        );
        p.set_self_ref(&p);
        p
    };
    let pooled_a = make_pooled(Arc::new(FirecrackerBackend::new(work.path(), cfg.clone())));

    let spec = SandboxSpec {
        image: "engram-chain-rehydrate-test".into(),
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
    let sandbox = pooled_a.create(spec).await.expect("create");
    let session_id = engram_core::SessionId::new();
    pooled_a.bind_session(session_id, sandbox);

    // ---- 3. Marker 1 → Full seed checkpoint; chain-head record lands ----
    let sum1 = plant_marker(&pooled_a, sandbox, 1).await;
    let ckpt1 = pooled_a
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint 1 (Full seed)");
    let m1 = ckpt1.memory_manifest.expect("seed manifest");
    let record = ChainHeadRecord::load(&chains_dir, sandbox)
        .await
        .expect("chain-head record committed by the seed checkpoint");
    assert_eq!(record.manifest_ref, m1);
    assert_eq!(record.session_id, Some(session_id));

    // Marker 2: guest divergence AFTER the recorded head — exactly what
    // the post-roll diff must carry.
    let sum2 = plant_marker(&pooled_a, sandbox, 2).await;

    // ---- 4. The roll: generation B reattaches the survivor and
    // rehydrates the chain from the durable record. Generation A is
    // "dead" — never used again, but not dropped (a real pod death
    // doesn't run destructors; mirrors eviction_finalize_redrive). ----
    let _gen_a_dead = pooled_a;
    let inner_b = Arc::new(FirecrackerBackend::new(work.path(), cfg.clone()));
    let report = engram_host_agent::live_attach::reattach_pass(work.path(), &inner_b)
        .await
        .expect("reattach pass");
    assert!(
        !report.reattached.is_empty(),
        "generation B must reattach the still-live VM",
    );
    let pooled_b = make_pooled(inner_b);
    pooled_b.bind_session(session_id, sandbox);
    pooled_b.rehydrate_chain_heads().await;

    // ---- 5. First post-roll capture is a DIFF on the recorded chain ----
    let t = Instant::now();
    let ckpt2 = pooled_b
        .checkpoint_sandbox(sandbox)
        .await
        .expect("post-roll checkpoint");
    eprintln!(
        "CHAIN-REHYDRATE: post-roll checkpoint {} ms, manifest {:?}",
        t.elapsed().as_millis(),
        ckpt2.memory_manifest,
    );
    let m2 = ckpt2.memory_manifest.expect("post-roll manifest");
    assert_eq!(
        m2.manifest_id, m1.manifest_id,
        "post-roll capture must DIFF on the rehydrated chain (incident: it was a Full)",
    );
    assert_eq!(m2.version, m1.version + 1, "diff ticks the version");

    // ---- 6. Byte fidelity of the roll-spanning diff: the surviving
    // VM's dirty-bitmap baseline really was the recorded head. ----
    pooled_b.destroy(sandbox).await.expect("destroy survivor");
    let restored = pooled_b
        .restore(ckpt2.clone())
        .await
        .expect("restore from the roll-spanning diff checkpoint");
    let out = exec(
        &pooled_b,
        restored,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str()],
        "both markers must survive the roll-spanning diff byte-identical",
    );

    // ---- 7. Torn capture: the diff create consumes the bitmap, then
    // post-processing dies (puts disabled) — the write-ahead invalidate
    // already removed the record, so the next generation must seed
    // nothing and capture a FULL on a fresh lineage. ----
    // (`restored`'s chain was seeded sparse at restore and its record
    // committed; marker 3 is the divergence the torn diff consumes.)
    ChainHeadRecord::load(&chains_dir, restored)
        .await
        .expect("restore seeds the chain and commits its record");
    let _sum3 = plant_marker(&pooled_b, restored, 3).await;
    fail_puts.armed.store(true, Ordering::SeqCst);
    pooled_b
        .checkpoint_sandbox(restored)
        .await
        .expect_err("torn capture: post-processing must fail while puts are disabled");
    fail_puts.armed.store(false, Ordering::SeqCst);
    assert!(
        ChainHeadRecord::load(&chains_dir, restored).await.is_none(),
        "a torn capture must leave NO chain-head record",
    );

    // ---- 8. Generation C: rehydrate finds nothing for the torn
    // sandbox; its next capture is a recovery FULL on a fresh lineage. ----
    let _gen_b_dead = pooled_b;
    let inner_c = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let report = engram_host_agent::live_attach::reattach_pass(work.path(), &inner_c)
        .await
        .expect("reattach pass (generation C)");
    assert!(!report.reattached.is_empty());
    let pooled_c = make_pooled(inner_c);
    pooled_c.bind_session(session_id, restored);
    pooled_c.rehydrate_chain_heads().await;

    let ckpt3 = pooled_c
        .checkpoint_sandbox(restored)
        .await
        .expect("post-torn recovery checkpoint");
    let m3 = ckpt3.memory_manifest.expect("recovery manifest");
    assert_ne!(
        m3.manifest_id, m2.manifest_id,
        "recovery capture must NOT diff on the torn chain's lineage",
    );
    assert_eq!(m3.version, 1, "recovery is a Full seeding a fresh lineage");
    // And the recovery re-commits a record, so the ratchet resumes.
    let record = ChainHeadRecord::load(&chains_dir, restored)
        .await
        .expect("recovery Full re-commits the chain-head record");
    assert_eq!(record.manifest_ref, m3);

    pooled_c.destroy(restored).await.expect("destroy restored");
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
