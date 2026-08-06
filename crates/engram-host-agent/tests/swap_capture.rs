//! ADR 0112 D3 end-to-end: the capture-time swap protocol through
//! `PooledBackend` against a real Firecracker microVM.
//!
//! What this pins, minimally sized (64 MiB swap, 256 MiB guest):
//!
//!   1. agentd armed swap at boot; a `checkpoint_sandbox` (Periodic
//!      flavor, ≈0 used swap) runs `swapoff` BEFORE the pause and
//!      re-arms after — the live guest ends the capture with swap ON.
//!   2. The captured memory image contains NO swap state: a `restore`
//!      of that checkpoint wakes with `/proc/swaps` EMPTY (the core
//!      invariant — the restored device is a fresh zero-filled file,
//!      and the image must not reference it). Bind-time re-arm is
//!      agentd's job and out of scope for a backend-level test.
//!   3. The restored guest re-arms cleanly by hand (`mkswap`+`swapon`
//!      of the writable non-vda disk succeed against the fresh
//!      backing), proving the device is usable, not just absent.
//!
//! Gating mirrors `checkpoint_chain.rs` (Linux + KVM + firecracker +
//! musl agentd + busybox).
// tests drive a live system; wall clock here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

const SWAP_MIB: u32 = 64;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots microVMs"]
async fn capture_disarms_swap_and_restore_wakes_swapless() {
    // ---- gating (mirrors checkpoint_chain.rs) ----
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
    for bin in ["firecracker", "mksquashfs"] {
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

    // ---- 1. Bake an agentd-injected rootfs ----
    let images = tempfile::tempdir().expect("images dir");
    let work = tempfile::tempdir().expect("work dir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
    );
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

    // ---- 2. PooledBackend over FC, swap-armed spec ----
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
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
    // The post-capture re-arm reaches the backend through the weak
    // self-ref (issue #529 pattern) — install it like production does.
    pooled.set_self_ref(&pooled);

    let spec = SandboxSpec {
        image: "engram-swap-capture-test".into(),
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
        swap_mib: Some(SWAP_MIB),
    };
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
    let sandbox = pooled.create(spec).await.expect("create");
    let session_id = engram_core::SessionId::new();
    pooled.bind_session(session_id, sandbox);

    // agentd arms swap at boot — wait for it.
    wait_for(&pooled, sandbox, "grep -q /dev/vd /proc/swaps", 20).await;

    // ---- 3. Checkpoint (Periodic flavor): disarm → capture → re-arm ----
    let t = Instant::now();
    let ckpt = pooled
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint of a swap-armed guest");
    eprintln!(
        "SWAP: checkpoint {} ms, memory manifest {:?}",
        t.elapsed().as_millis(),
        ckpt.memory_manifest,
    );
    // The re-arm is detached — poll until the LIVE guest has swap back.
    wait_for(&pooled, sandbox, "grep -q /dev/vd /proc/swaps", 20).await;
    pooled.destroy(sandbox).await.expect("destroy original");

    // ---- 4. Restore: the image must wake SWAPLESS ----
    let restored = pooled.restore(ckpt).await.expect("restore");
    let swaps = exec_ok(
        &pooled,
        restored,
        "cat /proc/swaps; true",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        !swaps.contains("/dev/vd"),
        "restored memory image still references swap — the disarm \
         invariant is broken (/proc/swaps: {swaps})",
    );

    // ---- 5. The fresh device re-arms cleanly (what bind would do) ----
    let rearm = exec_ok(
        &pooled,
        restored,
        "for d in /sys/block/vd*; do n=$(basename $d); \
         [ \"$n\" != vda ] && [ \"$(cat $d/ro)\" = 0 ] && \
         mkswap /dev/$n >/dev/null && swapon /dev/$n && echo REARMED-$n; done",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        rearm.contains("REARMED-"),
        "fresh swap device failed to re-arm on the restored guest: {rearm:?}",
    );

    pooled.destroy(restored).await.expect("destroy restored");
}

/// Poll a shell predicate inside the guest until it exits 0 (agentd
/// boot / detached re-arm settle) or panic after `budget_secs`.
async fn wait_for(
    backend: &Arc<PooledBackend>,
    id: engram_core::types::ids::SandboxId,
    predicate: &str,
    budget_secs: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match try_exec(backend, id, predicate).await {
            Ok((Some(0), _)) => return,
            Ok((status, out)) => last = Some(format!("exit {status:?}: {out}")),
            Err(e) => last = Some(format!("exec error: {e}")),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("guest predicate `{predicate}` never true (last: {last:?})");
}

/// Run a shell command, panic on nonzero exit, return stdout.
async fn exec_ok(
    backend: &Arc<PooledBackend>,
    id: engram_core::types::ids::SandboxId,
    sh: &str,
    budget: Duration,
) -> String {
    let deadline = Instant::now() + budget;
    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match try_exec(backend, id, sh).await {
            Ok((Some(0), out)) => return out,
            Ok((status, out)) => last = Some(format!("exit {status:?}: {out}")),
            Err(e) => last = Some(format!("exec error: {e}")),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("exec `{sh}` never succeeded (last: {last:?})");
}

async fn try_exec(
    backend: &Arc<PooledBackend>,
    id: engram_core::types::ids::SandboxId,
    sh: &str,
) -> Result<(Option<i32>, String), engram_core::SandboxError> {
    let stream = backend
        .exec_stream(
            id,
            ExecRequest {
                command: vec!["/bin/sh".into(), "-c".into(), sh.into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout: Some(Duration::from_secs(10)),
                exec_id: None,
                stdout_offset: None,
                stderr_offset: None,
                wake: None,
            },
        )
        .await?;
    let mut events = stream.events;
    let mut stdout = Vec::new();
    let mut exit = None;
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(_) => {}
            ExecEvent::Exit(status) => {
                exit = status;
                break;
            }
            ExecEvent::Refused(reason) => {
                return Err(engram_core::SandboxError::Snapshot(format!(
                    "refused: {reason}"
                )))
            }
        }
    }
    Ok((exit, String::from_utf8_lossy(&stdout).into_owned()))
}
