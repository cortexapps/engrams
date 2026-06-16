//! Capture-time `[warm]` hook (image warm-process prewarm) against a real
//! Firecracker microVM.
//!
//! Covers the two halves of the warm-hook contract on `PooledBackend`:
//!
//!   1. `warm_hook_process_survives_base_snapshot` — a `[warm]` command
//!      that starts a detached process is run by `build_base_snapshot`
//!      AFTER agentd-ready and BEFORE the memory snapshot freezes, so the
//!      process is captured into the base snapshot and is still alive in a
//!      session restored from it. This is the whole point: every restored
//!      session inherits a warm process (e.g. a gradle daemon) with no
//!      cold start.
//!   2. `warm_hook_nonzero_exit_fails_capture` — a warm command that exits
//!      non-zero fails the capture (fail-loud): `build_base_snapshot`
//!      returns `Err`, so the enable aborts rather than silently shipping
//!      a "cold" base snapshot that the hook claimed to warm.
//!
//! Linux + KVM + firecracker + Docker + mke2fs + musl agentd only (the
//! exec verification needs an agentd-baked rootfs). Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test warm_hook_capture -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

/// Marker the warm command leaves running; `pgrep -f` must find it in the
/// restored session. A distinctive sleep duration so it can't collide with
/// anything else in the guest.
const WARM_SENTINEL: &str = "sleep 2147480";

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn warm_hook_process_survives_base_snapshot() {
    let Some(env) = TestEnv::gate() else { return };
    let pooled = env.pooled();
    let rootfs = env.bake("engram-warm-hook-test").await;

    // The warm command detaches a long-lived process (the gradle-daemon
    // stand-in) into its own session so it outlives the exec process group,
    // then writes a marker and exits 0. This is exactly the shape a real
    // warm hook takes (`gradle --daemon` self-detaches the same way).
    let warm = WarmConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "setsid sh -c 'exec {WARM_SENTINEL}' </dev/null >/dev/null 2>&1 & \
                 echo warmed > /dev/shm/engram-warm-marker"
            ),
        ],
        timeout_secs: Some(60),
        workdir: None,
    };

    let meta = pooled
        .build_base_snapshot(env.spec(&rootfs), Some(warm))
        .await
        .expect("base-snapshot capture with a passing warm hook");

    // Restore a fresh session from the captured base snapshot and confirm
    // the warmed process came back live (the snapshot froze it running).
    let restored = pooled
        .restore_fresh(meta)
        .await
        .expect("restore from warm base snapshot");

    let pids = exec(
        &pooled,
        restored,
        &format!("pgrep -f '{WARM_SENTINEL}' || echo MISSING"),
    )
    .await;
    assert!(
        pids.trim() != "MISSING" && !pids.trim().is_empty(),
        "the warm process must be alive in the restored session, got {pids:?}"
    );
    let marker = exec(&pooled, restored, "cat /dev/shm/engram-warm-marker").await;
    assert_eq!(
        marker.trim(),
        "warmed",
        "warm marker must survive the snapshot"
    );

    pooled.destroy(restored).await.expect("destroy restored");
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn warm_hook_nonzero_exit_fails_capture() {
    let Some(env) = TestEnv::gate() else { return };
    let pooled = env.pooled();
    let rootfs = env.bake("engram-warm-hook-fail-test").await;

    // Fail-loud: a warm command that exits non-zero must abort the capture.
    let warm = WarmConfig {
        command: vec!["/bin/sh".into(), "-c".into(), "exit 7".into()],
        timeout_secs: Some(60),
        workdir: None,
    };

    let err = pooled
        .build_base_snapshot(env.spec(&rootfs), Some(warm))
        .await
        .expect_err("a non-zero warm hook must FAIL the capture (fail-loud)");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("[warm]"),
        "capture error must attribute the failure to the warm hook, got {msg}"
    );
}

// ---- test harness -------------------------------------------------------

struct TestEnv {
    kernel: std::path::PathBuf,
    agent: std::path::PathBuf,
    work: tempfile::TempDir,
    images: tempfile::TempDir,
    chunk_store: engram_chunk_store::ChunkStore,
}

impl TestEnv {
    /// Skip (returning `None`) unless the full FC toolchain is present —
    /// mirrors `checkpoint_chain`'s gating so the test no-ops on macOS / a
    /// runner without KVM rather than failing.
    fn gate() -> Option<Self> {
        let kernel = match std::env::var("FC_TEST_KERNEL") {
            Ok(p) => std::path::PathBuf::from(p),
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
            let missing = std::env::var_os("PATH")
                .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(true);
            if missing {
                eprintln!("SKIP: {bin} not on PATH");
                return None;
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
            return None;
        }
        let work = tempfile::tempdir().expect("work dir");
        let images = tempfile::tempdir().expect("images dir");
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
        );
        let chunk_store = engram_chunk_store::ChunkStore::new(blob);
        std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
        Some(Self {
            kernel,
            agent,
            work,
            images,
            chunk_store,
        })
    }

    fn pooled(&self) -> Arc<PooledBackend> {
        let mut cfg = FirecrackerConfig::with_kernel(self.kernel.clone());
        cfg.net_pool = None;
        cfg.restore_mode = RestoreMode::File;
        cfg.default_boot_args =
            "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
        let inner = Arc::new(FirecrackerBackend::new(self.work.path(), cfg));
        let mut cache_cfg =
            engram_chunk_store::cache::ChunkCacheConfig::new(self.work.path().join("chunk-cache"));
        cache_cfg.budget_bytes = 1024 * 1024 * 1024;
        Arc::new(
            PooledBackend::new(inner)
                .with_chunk_store(
                    self.chunk_store.clone(),
                    self.work.path().join("materialize"),
                )
                .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
                .with_checkpoint_dir(self.work.path().join("checkpoints")),
        )
    }

    /// Bake a minimal agentd-injected debian rootfs and return its path.
    async fn bake(&self, name: &str) -> std::path::PathBuf {
        let src = tempfile::tempdir().expect("source dir");
        std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
        std::fs::write(
            src.path().join("engram.toml"),
            format!("name = \"{name}\"\n"),
        )
        .unwrap();
        let baker = Builder::new(DockerCli::new(), self.chunk_store.clone());
        let outcome = baker
            .build(&BuildRequest {
                source: src.path().to_path_buf(),
                repo: name.into(),
                tag: "warm-1".into(),
                images_dir: self.images.path().to_path_buf(),
                format: Format::Ext4,
                agent_injection: Some(AgentInjection {
                    agent_binary: self.agent.clone(),
                    vsock_port: ENGRAM_AGENTD_PORT,
                    transport: engram_image_builder::Transport::Vsock,
                    init_script: None,
                }),
            })
            .await
            .expect("ext4 bake with agent injection");
        outcome.rootfs_path
    }

    fn spec(&self, rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "engram-warm-hook-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
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
        }
    }
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
    let mut stdout = Vec::new();
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(_) => {}
            ExecEvent::Exit(_) => break,
        }
    }
    String::from_utf8_lossy(&stdout).into_owned()
}
