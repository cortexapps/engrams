//! Swap pages survive a snapshot and restore on a second pooled host.
#![cfg(target_os = "linux")]
// Tests drive a live system; wall clock and entropy are inputs (ADR 0098 D1).
#![allow(clippy::disallowed_methods)]
mod common;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn host(
    work: &Path,
    kernel: &Path,
    devices: &[PathBuf],
    store: &engram_chunk_store::ChunkStore,
) -> Arc<PooledBackend> {
    let mut config = FirecrackerConfig::with_kernel(kernel.to_path_buf());
    config.firecracker_bin = std::env::var_os("ENGRAM_FC_FORK_BIN").unwrap().into();
    config.net_pool = None;
    config.restore_mode = RestoreMode::File;
    config.track_dirty_pages = true;
    let inner = Arc::new(FirecrackerBackend::new(work, config));
    let pooled = Arc::new(
        PooledBackend::new(inner)
            .with_chunk_store(store.clone(), work.join("materialize"))
            .with_chunk_cache(engram_chunk_store::ChunkCache::new(
                engram_chunk_store::cache::ChunkCacheConfig::new(work.join("cache")),
            ))
            .with_nbd_pool(
                engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(devices.to_vec())
                    .unwrap(),
            )
            .with_nbd_owner_dir(work.join("owners"))
            .with_checkpoint_dir(work.join("checkpoints")),
    );
    pooled.set_self_ref(&pooled);
    pooled
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux, KVM, two NBD devices, fork, static agentd and C compiler"]
async fn swap_pages_survive_cross_host_snapshot() {
    let Some(devices) = common::postcopy::nbd_devices(2) else {
        return;
    };
    let Some(kernel) = std::env::var_os("FC_TEST_KERNEL").map(PathBuf::from) else {
        return;
    };
    let Some(busybox) = common::find_busybox() else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "engram_host_agent=debug,engram_chunk_store=debug,engram_sandbox_firecracker=info",
        )
        .with_test_writer()
        .try_init();
    // The runner's disk can sit under the cache's default free floor; the
    // live-teleport tests pin the same value.
    std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");
    let agent = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    assert!(agent.exists(), "build static agentd before the NBD lane");
    let fixture = tempfile::tempdir().unwrap();
    let sentinel = fixture.path().join("swap-pageout");
    assert!(std::process::Command::new("cc")
        .args(["-static", "-O2", "-o"])
        .arg(&sentinel)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/swap_pageout.c"))
        .status()
        .unwrap()
        .success());
    // A rootfs-local agent keeps this fixture to two virtio disks: vda and vdb.
    let init = fixture.path().join("init.sh");
    let shim = engram_rootfs_materializer::inject::DEFAULT_INIT_SHIM;
    let start = shim.find("AGENTD_DIR=\"\"").unwrap();
    let end = shim[start..].find("mkdir -p /run/engram").unwrap() + start;
    // A caller-supplied init is installed verbatim, so this fixture
    // fills the shim's placeholders itself (the default path does it).
    std::fs::write(
        &init,
        format!(
            "{}AGENTD_DIR=/opt/test-agentd\n{}",
            &shim[..start],
            &shim[end..]
        )
        .replace("__VSOCK_PORT__", &ENGRAM_AGENTD_PORT.to_string())
        .replace("__TRANSPORT__", Transport::Vsock.env_value()),
    )
    .unwrap();
    let store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(fixture.path().join("blob")),
    ));
    let image = common::bake_fixture_ext4(
        &fixture.path().join("rootfs.ext4"),
        &store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: Some(init),
        }),
        |tree| {
            std::fs::create_dir_all(tree.join("opt/test-agentd"))?;
            std::fs::copy(&agent, tree.join("opt/test-agentd/engram-agentd"))?;
            std::fs::copy(&sentinel, tree.join("bin/swap-pageout"))?;
            Ok(())
        },
    )
    .await;
    let source_work = tempfile::tempdir().unwrap();
    let source = host(source_work.path(), &kernel, &devices, &store);
    let spec = SandboxSpec {
        image: "swap-nbd-test".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: image.disk_manifest,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![],
        swap_mib: Some(64),
        swap_source: None,
        swap_manifest: None,
    };
    let base = timed("base capture", 120, common::postcopy::base(&source, spec)).await;
    assert!(base.swap_manifest.is_none(), "base must be swap-free");
    let vm = timed("fresh restore", 90, source.restore_fresh(base, vec![]))
        .await
        .unwrap();
    source.bind_session(engram_core::SessionId::new(), vm);
    wait_for(&source, vm, "test $(wc -l < /proc/swaps) -eq 1", 20).await;
    source
        .start_agent(
            vm,
            engram_core::types::sandbox::AgentSpec {
                argv: vec![],
                env: Default::default(),
                session_env: Default::default(),
                binding_epoch: 1,
                host_ca_pem: None,
            },
        )
        .await
        .unwrap();
    wait_for(&source, vm, "grep -q /dev/vdb /proc/swaps", 20).await;
    let geometry = exec_ok(&source, vm, "cat /sys/block/vdb/size /sys/block/vdb/queue/read_ahead_kb /sys/block/vda/queue/read_ahead_kb /proc/sys/vm/swappiness", Duration::from_secs(10)).await;
    assert_eq!(
        geometry.split_whitespace().collect::<Vec<_>>(),
        ["131072", "128", "4096", "100"]
    );
    assert!(source
        .swap_device(vm)
        .unwrap()
        .to_string_lossy()
        .starts_with("/dev/nbd"));
    let signature = exec_ok(
        &source,
        vm,
        "dd if=/dev/vdb bs=4096 count=1 2>/dev/null | sha256sum",
        Duration::from_secs(10),
    )
    .await;
    exec_ok(
        &source,
        vm,
        "swap-pageout >/tmp/pageout-log 2>&1 & echo $! >/tmp/pageout-pid",
        Duration::from_secs(10),
    )
    .await;
    wait_for(
        &source,
        vm,
        "test -f /tmp/pageout-ready && awk 'NR>1 && $4>0 {ok=1} END {exit !ok}' /proc/swaps",
        20,
    )
    .await;
    let snapshot = timed("snapshot", 120, source.snapshot(vm)).await.unwrap();
    assert!(snapshot.swap_manifest.is_some());
    timed("source destroy", 30, source.destroy(vm))
        .await
        .unwrap();
    drop(source);
    let dest_work = tempfile::tempdir().unwrap();
    let dest = host(dest_work.path(), &kernel, &devices, &store);
    let restored = timed("restore on second host", 120, dest.restore(snapshot))
        .await
        .unwrap();
    wait_for(&dest, restored, "grep -q /dev/vdb /proc/swaps", 20).await;
    dest.start_agent(
        restored,
        engram_core::types::sandbox::AgentSpec {
            argv: vec![],
            env: Default::default(),
            session_env: Default::default(),
            binding_epoch: 1,
            host_ca_pem: None,
        },
    )
    .await
    .unwrap();
    let restored_signature = exec_ok(
        &dest,
        restored,
        "dd if=/dev/vdb bs=4096 count=1 2>/dev/null | sha256sum",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        signature, restored_signature,
        "restore and bind must preserve the swap header"
    );
    exec_ok(
        &dest,
        restored,
        "kill -USR1 $(cat /tmp/pageout-pid)",
        Duration::from_secs(10),
    )
    .await;
    wait_for(
        &dest,
        restored,
        "test -f /tmp/pageout-ok && grep -q /dev/vdb /proc/swaps",
        20,
    )
    .await;
    timed("dest destroy", 30, dest.destroy(restored))
        .await
        .unwrap();
}

/// Bound a phase so a hang names its step instead of hitting the lane's
/// silent per-test timeout.
async fn timed<T>(label: &str, secs: u64, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(v) => v,
        Err(_) => panic!("{label} did not finish within {secs}s"),
    }
}

/// Poll a shell predicate inside the guest until it exits 0 (agentd
/// boot or bind completes) or panic after `budget_secs`.
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
