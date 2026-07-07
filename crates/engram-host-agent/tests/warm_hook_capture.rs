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

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format, InitInjection};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

/// The long-lived process the warm command leaves running (a
/// gradle-daemon stand-in). A distinctive sleep duration so nothing else
/// in the guest collides. We track it by PID (recorded to a file), not by
/// name — `debian:bookworm-slim` has no `procps`/`pgrep`.
const WARM_SENTINEL: &str = "sleep 2147480";

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn warm_hook_process_survives_base_snapshot() {
    let Some(env) = TestEnv::gate() else { return };
    let pooled = env.pooled();
    let rootfs = env.bake("engram-warm-hook-test").await;

    // The warm command backgrounds a long-lived process, records its PID +
    // a marker, and exits 0. `nohup` keeps it alive after the exec's shell
    // exits (it's reparented to init, not killed — agentd's exec only
    // SIGKILLs the direct child via kill_on_drop). This is the shape a real
    // warm hook takes (`gradle --daemon` likewise outlives the launching
    // shell).
    let warm = WarmConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "nohup {WARM_SENTINEL} </dev/null >/dev/null 2>&1 & \
                 echo $! > /dev/shm/engram-warm-pid && \
                 echo warmed > /dev/shm/engram-warm-marker"
            ),
        ],
        timeout_secs: Some(60),
        workdir: None,
        network: None,
    };

    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);
    let meta = pooled
        .build_base_snapshot(
            env.spec(&rootfs),
            Some(warm),
            Default::default(),
            progress_tx,
        )
        .await
        .expect("base-snapshot capture with a passing warm hook");

    // Restore a fresh session from the captured base snapshot and confirm
    // the warmed process came back live (the snapshot froze it running).
    let restored = pooled
        .restore_fresh(meta, Vec::new())
        .await
        .expect("restore from warm base snapshot");

    // The snapshot froze the guest's process table, so the warmed PID is
    // still valid after restore. Assert it's alive via /proc (no procps in
    // the slim rootfs). `kill -0` is a shell builtin and needs no procps.
    let alive = exec(
        &pooled,
        restored,
        "PID=$(cat /dev/shm/engram-warm-pid 2>/dev/null); \
         if [ -n \"$PID\" ] && kill -0 \"$PID\" 2>/dev/null; then echo ALIVE; else echo MISSING; fi",
    )
    .await;
    assert!(
        alive.contains("ALIVE"),
        "the warm process must be alive in the restored session, got {alive:?}"
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
async fn warm_hook_sees_manifest_env() {
    // Regression: the capture-time `[warm]` hook must run with the image's
    // manifest `[env]` (e.g. JAVA_HOME), like /exec and restored sessions.
    // The capture VM's agentd has NO durable session env (no session bind;
    // `merge_session_env` is a no-op on FC), so `build_base_snapshot` passes
    // the manifest `[env]` through as the hook's `ExecRequest.env`. Before
    // that, the hook ran with no environment — a real `gradle`/`node` warmup
    // that reads JAVA_HOME failed fast (dev-brain: exit 1 in ~40 ms). Here the
    // hook requires a manifest var and exits non-zero if it's absent, so a
    // regressed env pass-through fails the capture (fail-loud) instead of
    // silently shipping a cold snapshot.
    let Some(env) = TestEnv::gate() else { return };
    let pooled = env.pooled();
    let rootfs = env.bake("engram-warm-hook-env-test").await;

    let warm = WarmConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            // Mirrors how `./gradlew` depends on JAVA_HOME: the hook is
            // useless without the manifest env, so make that explicit.
            "[ \"$ENGRAM_WARM_ENV_PROBE\" = present ] || exit 9; \
             echo \"$ENGRAM_WARM_ENV_PROBE\" > /dev/shm/engram-warm-env"
                .into(),
        ],
        timeout_secs: Some(60),
        workdir: None,
        network: None,
    };

    // The probe rides the manifest `[env]` (SandboxSpec.env) — the same
    // channel JAVA_HOME/PATH travel on for a real image.
    let mut spec = env.spec(&rootfs);
    spec.env
        .insert("ENGRAM_WARM_ENV_PROBE".into(), "present".into());

    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);
    let meta = pooled
        .build_base_snapshot(spec, Some(warm), Default::default(), progress_tx)
        .await
        .expect("warm hook must see the manifest [env]; capture should succeed");

    // Confirm the value the hook observed was the manifest one (not a stray
    // default), surviving into a restored session.
    let restored = pooled
        .restore_fresh(meta, Vec::new())
        .await
        .expect("restore");
    let seen = exec(&pooled, restored, "cat /dev/shm/engram-warm-env").await;
    assert_eq!(
        seen.trim(),
        "present",
        "the warm hook must observe the manifest [env] value"
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
        network: None,
    };

    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);
    let err = pooled
        .build_base_snapshot(
            env.spec(&rootfs),
            Some(warm),
            Default::default(),
            progress_tx,
        )
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
    /// ADR 0080: the staged agentd bundle every backend/spec references.
    staged: common::StagedAgentdBundle,
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
        for bin in ["firecracker", "docker", "mke2fs", "mksquashfs"] {
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
        // ADR 0080: agentd rides its reserved bundle slot — stage the
        // fixture bundle once for every backend this fixture builds.
        let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
        );
        let chunk_store = engram_chunk_store::ChunkStore::new(blob);
        std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
        Some(Self {
            kernel,
            staged,
            work,
            images,
            chunk_store,
        })
    }

    fn pooled(&self) -> Arc<PooledBackend> {
        let mut cfg = FirecrackerConfig::with_kernel(self.kernel.clone());
        cfg.bundle_dir = self.staged.bundle_dir.clone();
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
                init_injection: Some(InitInjection {
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
            aux_ro_drives: vec![self.staged.agentd_slot()],
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
