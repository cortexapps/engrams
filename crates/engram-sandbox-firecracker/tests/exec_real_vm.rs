//! End-to-end Phase 2 sign-off: bake an Engram-agent-baked ext4
//! rootfs, boot a real Firecracker microVM with that rootfs, and
//! exec_stream a command through the in-guest agent over vsock.
//!
//! Heavy test (Docker pull + bake + microVM boot, ~30 s on the dev
//! VM) — gated `#[ignore]` like the other FC integration tests.
//!
//! Preconditions: Linux + KVM + firecracker on PATH + Docker daemon
//! reachable + the `engram-agentd` binary already built at
//! `target/<profile>/engram-agentd`. The runner script
//! (`scripts/run-boot-test.sh`) ensures the binary is built; running
//! manually requires `cargo build -p engram-agentd` first.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn exec_runs_inside_baked_microvm() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }

    // Find the statically-linked musl agent. Built by run-boot-test.sh
    // before this test runs (or manually via
    //   cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
    // ). Static-musl is required because the agent runs inside an
    // arbitrary rootfs (debian, alpine) where Nix's glibc paths from
    // a dev-shell build don't exist.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: static-musl engram-agentd not built at {}.\n  Run: cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake an Engram-agent-baked ext4 image ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        // debian-slim has glibc + /bin/sh, matches our agent's libc.
        // No package installs needed — agent + init shim are all we
        // ship into the rootfs ourselves.
        "FROM debian:bookworm-slim\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-agent-vm-test\"\n",
    )
    .unwrap();

    let images = tempfile::tempdir().expect("images dir");
    let docker = DockerCli::new();
    let baker = Builder::new(docker);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-agent-vm-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent,
                vsock_port: ENGRAM_AGENTD_PORT,
                init_script: None,
                bootstrap_binary: None,
                harness_binaries: Vec::new(),
            }),
        })
        .await
        .expect("ext4 bake with agent injection");
    assert!(
        outcome.rootfs_path.extension().and_then(|s| s.to_str()) == Some("ext4"),
        "expected rootfs.ext4, got {}",
        outcome.rootfs_path.display()
    );

    // ---- 2. Set up FC backend, boot the microVM ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Boot directly into our init shim. Without this the kernel
    // would try to exec /sbin/init (debian's systemd) which we
    // don't have configured.
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "engram-agent-vm-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        cpu: CpuLimit { vcpus: 1 },
        // 256 MiB: enough for debian-slim's kernel-mounted FS + the
        // agent. Smaller VMs OOM in early boot.
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
    };
    let sandbox_id = backend.create(spec).await.expect("create");

    // ---- 3. Wait for the in-guest agent to come up ----
    // After InstanceStart, the kernel boots, runs /sbin/engram-init,
    // which exec's the agent. The agent then binds AF_VSOCK port
    // 1024. We poll exec_stream until it succeeds — once the agent
    // accepts the connection, we have a working stream.
    let req = ExecRequest {
        command: vec!["echo".into(), "hello-from-microvm".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };

    // Make the FC backend leave the jail dir behind on failure so we
    // can grab firecracker.log for a post-mortem when the test fails.
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let stream = match wait_for_agent(&backend, sandbox_id, &req, Duration::from_secs(20)).await {
        Ok(s) => s,
        Err(e) => {
            // Dump every firecracker.log we find under work_dir so the
            // test output captures whatever FC said on its way to the
            // hang / failure mode.
            for entry in std::fs::read_dir(work.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let log = entry.path().join("firecracker.log");
                if log.exists() {
                    let content = std::fs::read_to_string(&log).unwrap_or_default();
                    eprintln!("--- {} ---\n{content}", log.display());
                }
            }
            panic!("agent never came up: {e:?}");
        }
    };

    // ---- 4. Drain events; assert stdout + exit ----
    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(
        stdout,
        b"hello-from-microvm\n",
        "stdout mismatch (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(exit, Some(0), "expected clean exit");

    // ---- 5. Cleanup ----
    backend.destroy(sandbox_id).await.expect("destroy");
}

/// Poll `exec_stream` once every 500ms for up to `budget`. The agent
/// inside the guest takes a couple of seconds to boot — connecting
/// before it binds vsock returns Vm("connect ... ECONNRESET"). Once
/// it's up, the call returns a live stream.
async fn wait_for_agent(
    backend: &FirecrackerBackend,
    sandbox_id: engram_core::types::ids::SandboxId,
    req: &ExecRequest,
    budget: Duration,
) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError> {
    let deadline = std::time::Instant::now() + budget;
    let mut last_err = None;
    while std::time::Instant::now() < deadline {
        match backend.exec_stream(sandbox_id, req.clone()).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_err.expect("no attempts made"))
}
