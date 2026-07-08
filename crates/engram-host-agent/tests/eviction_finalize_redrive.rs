//! Issue #529 end-to-end: host-durable eviction finalize survives a
//! host-agent process death mid-upload, against a REAL Firecracker
//! microVM.
//!
//! What this pins (the property the whole redesign exists for):
//!
//!   1. `snapshot_begin` on a real FC guest durably persists the
//!      `EvictionFinalizeRecord` (+ any drained NBD disk-pending chunks)
//!      BEFORE returning.
//!   2. "Generation A" (the host-agent process that started the
//!      finalize) is simulated as DYING mid-upload: its spawned
//!      finalize job is frozen (gated on the first chunk-store blob
//!      PUT) rather than allowed to complete, standing in for a SIGKILL
//!      between the durable persist and the upload finishing.
//!   3. "Generation B" — an entirely FRESH `PooledBackend` +
//!      `FirecrackerBackend` (disjoint in-RAM state: its own
//!      `capture_locks`, `pending_finalizes`, no knowledge of
//!      generation A's sandbox) pointed at the SAME `work_dir` — runs
//!      the SAME startup sequence a real host-agent restart runs
//!      (`live_attach::reattach_pass` to rejoin the still-live VM, then
//!      `resume_pending_finalizes` to re-drive the record from disk
//!      alone) and completes the finalize independently.
//!   4. The resulting durable `CheckpointRecord { kind: EvictionFinal }`
//!      restores cleanly with the planted marker byte-identical — zero
//!      guest-turn loss, the acceptance criterion this issue exists to
//!      satisfy.
//!
//! Sized to the property, not to realism: one guest, one marker, one
//! capture. No long sleeps — the gate is a `Notify`, not a timeout.
//!
//! Gating mirrors `checkpoint_chain.rs` (Linux + KVM + firecracker +
//! Docker + mke2fs + musl agentd). Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test eviction_finalize_redrive -- --ignored --nocapture
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
use engram_host_agent::checkpoint::CheckpointRecord;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

/// A `BlobStorage` whose FIRST `put_streaming` blocks until released —
/// freezes generation A's finalize job mid-leg (after the disk leg,
/// inside the memory leg's chunk upload) so generation B's redrive has
/// real, unfinished work to pick up.
struct GateFirstPut {
    inner: engram_storage_local::LocalBlobStorage,
    gate: Arc<tokio::sync::Notify>,
    armed: AtomicBool,
}
#[async_trait::async_trait]
impl engram_core::traits::BlobStorage for GateFirstPut {
    async fn put_streaming(
        &self,
        key: &str,
        body: engram_core::traits::ByteStream,
    ) -> Result<u64, engram_core::error::BlobError> {
        if !self.armed.swap(true, Ordering::SeqCst) {
            self.gate.notified().await;
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
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn eviction_finalize_survives_a_simulated_host_agent_death_mid_upload() {
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
    let blob_root = work.path().join("blob");
    let local_blob = engram_storage_local::LocalBlobStorage::new(blob_root.clone());
    let bake_blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let bake_chunk_store = engram_chunk_store::ChunkStore::new(bake_blob);
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &bake_chunk_store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |_tree| Ok(()),
    )
    .await;

    // ---- 2. Generation A: PooledBackend wrapping a GATED chunk store ----
    let gate = Arc::new(tokio::sync::Notify::new());
    let gated_blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(GateFirstPut {
        inner: local_blob,
        gate: gate.clone(),
        armed: AtomicBool::new(false),
    });
    let gated_chunk_store = engram_chunk_store::ChunkStore::new(gated_blob);

    let mut cfg = FirecrackerConfig::with_kernel(kernel.clone());
    cfg.net_pool = None;
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.restore_mode = RestoreMode::File;
    cfg.track_dirty_pages = true; // required for snapshot_begin's diff-checkpoint gate
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let inner_a = Arc::new(FirecrackerBackend::new(work.path(), cfg.clone()));
    let checkpoint_dir = work.path().join("checkpoints");
    let pooled_a = Arc::new(
        PooledBackend::new(inner_a)
            .with_chunk_store(gated_chunk_store.clone(), work.path().join("materialize"))
            .with_checkpoint_dir(checkpoint_dir.clone()),
    );
    pooled_a.set_self_ref(&pooled_a);

    let spec = SandboxSpec {
        image: "engram-evict-finalize-test".into(),
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

    let marker_sum = plant_marker(&pooled_a, sandbox).await;

    // ---- 3. snapshot_begin: durably persists, THEN the background job
    // freezes in the memory leg (the gated blob's first PUT) — standing
    // in for generation A dying between the durable persist and the
    // upload completing. ----
    let t = Instant::now();
    let snapshot_id = pooled_a
        .snapshot_begin(sandbox)
        .await
        .expect("snapshot_begin");
    eprintln!(
        "EVICT-FINALIZE: snapshot_begin returned in {} ms (durability boundary)",
        t.elapsed().as_millis(),
    );

    let finalize_dir = checkpoint_dir.join("finalize");
    let record_path = finalize_dir.join(format!("{snapshot_id}.json"));
    wait_for("finalize record on disk", || record_path.exists()).await;
    eprintln!("EVICT-FINALIZE: durable record confirmed on disk before any upload completed");

    // ---- 4. Generation B: fresh backend, fresh PooledBackend, same
    // work_dir. Reattach the still-live VM (the real host-agent startup
    // sequence, ADR 0044 K2) then re-drive the finalize purely from
    // what's on disk. ----
    let inner_b = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let report = engram_host_agent::live_attach::reattach_pass(work.path(), &inner_b)
        .await
        .expect("reattach pass");
    eprintln!(
        "EVICT-FINALIZE: generation B reattached {} sandbox(es)",
        report.reattached.len(),
    );
    assert!(
        !report.reattached.is_empty(),
        "generation B must reattach the still-live VM generation A never destroyed",
    );

    // Deliberately never un-gate `gate` — a truly dead process's task
    // never resumes; leaving generation A's job permanently frozen (it
    // gets dropped, along with the whole runtime, at test exit) is what
    // makes generation B's re-drive the ONLY path to completion, not a
    // race against a still-running rival.
    let _gate_intentionally_never_released = gate;

    let pooled_b = Arc::new(
        PooledBackend::new(inner_b)
            .with_chunk_store(gated_chunk_store, work.path().join("materialize"))
            .with_checkpoint_dir(checkpoint_dir.clone()),
    );
    pooled_b.set_self_ref(&pooled_b);
    pooled_b.resume_pending_finalizes().await;

    // ---- 5. The durable CheckpointRecord lands regardless of which
    // generation's job won the race — row-only-at-finalize, but now
    // host-owned and re-drivable. ----
    let records_dir = pooled_b.checkpoint_records_dir().expect("records dir");
    let checkpoint_path = records_dir.join(format!("{snapshot_id}.json"));
    wait_for("eviction-final checkpoint record after re-drive", || {
        checkpoint_path.exists()
    })
    .await;
    let bytes = tokio::fs::read(&checkpoint_path).await.unwrap();
    let checkpoint: CheckpointRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(checkpoint.session_id, session_id);
    assert_eq!(
        checkpoint.kind,
        engram_protocol::heartbeat::CheckpointKind::EvictionFinal
    );
    let memory_manifest = checkpoint
        .memory_manifest
        .expect("a captured memory.bin must chunk to a manifest");

    // The finalize record is gone on completion; the sandbox is gone
    // (destroyed by whichever generation's job reached terminal first).
    wait_for("in-flight finalize record cleared", || {
        !record_path.exists()
    })
    .await;
    wait_for("sandbox destroyed", || {
        futures::executor::block_on(pooled_b.list())
            .map(|l| !l.contains(&sandbox))
            .unwrap_or(false)
    })
    .await;

    // ---- 6. Restore from the landed manifests on a THIRD fresh
    // backend and prove the marker survived byte-identical — zero
    // guest-turn replay, the acceptance criterion. ----
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: snapshot_id,
        size_bytes: checkpoint.size_bytes,
        created_at: checkpoint.captured_at,
        image_version: checkpoint.image_version.clone(),
        disk_manifest: checkpoint.disk_manifest,
        memory_manifest: Some(memory_manifest),
        base_memory_manifest: None,
        migration_source: None,
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: checkpoint.aux_bundles.clone(),
        paused_at: Some(checkpoint.paused_at),
    };
    let restored = pooled_b
        .restore(metadata)
        .await
        .expect("restore from the redriven eviction-final checkpoint");
    let out = exec(
        &pooled_b,
        restored,
        "sha256sum /dev/shm/marker | cut -d' ' -f1",
    )
    .await;
    assert_eq!(
        out.trim(),
        marker_sum,
        "the marker planted before eviction must survive the crash-recovered finalize \
         byte-identical — zero guest-turn loss",
    );
    pooled_b.destroy(restored).await.expect("destroy restored");
}

async fn plant_marker(backend: &Arc<PooledBackend>, id: engram_core::SandboxId) -> String {
    let out = exec(
        backend,
        id,
        "head -c 1048576 /dev/urandom > /dev/shm/marker \
         && sha256sum /dev/shm/marker | cut -d' ' -f1",
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

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}
