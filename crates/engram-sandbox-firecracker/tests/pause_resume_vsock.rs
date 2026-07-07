//! Plain pause→resume must not sever the vsock data path (ADR 0074 rung 2).
//!
//! Upstream FC v1.16 (and our fork until the fix) arms the vsock RX-delivery
//! gate (`pending_event_ack`) from `kick()` on EVERY `resume_vm` — but a plain
//! `PATCH /vm Paused → Resumed` cycle queues no `TRANSPORT_RESET` for the guest
//! to ack, so the gate never clears: every host→guest vsock delivery
//! black-holes and the FC event loop busy-spins. In production that turned the
//! rung-2 parked-paused un-pause into a permanently wedged session (prompts
//! "delivered" into an intact-but-gated connection; in-guest exec hung).
//!
//! This test proves the property end-to-end on the FORK binary: boot a real
//! microVM with the in-guest agent, exec over vsock (baseline), pause + resume
//! the VM in place, then exec again — the second exec must complete. It runs
//! only when `ENGRAM_FC_FORK_BIN` points at a forked firecracker (stock v1.16
//! fails this property by design of its snapshot-only kick), matching the
//! `stock_fork_snapshot_compat` gating: skipped on a submodule-bump PR before
//! the fork artifact publishes, exercised on every main run after.
//!
//! Sized to the property: one bake, one VM, one pause/resume cycle, two execs.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format, InitInjection};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + Docker + ENGRAM_FC_FORK_BIN; bakes a rootfs and boots a microVM"]
async fn vsock_delivers_after_plain_pause_resume() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") || !require_bin("mksquashfs") {
        return;
    }
    // Fork-only: stock v1.16's snapshot-only kick() arms the RX gate on plain
    // resume (the bug under test), so asserting the property against stock
    // would just re-prove upstream's defect. Same skip contract as
    // stock_fork_snapshot_compat.
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if Path::new(&p).exists() => std::path::PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set to an existing forked firecracker binary");
            return;
        }
    };

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: static-musl engram-agentd not built at {}.\n  \
             Build it first:\n    \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake an agent-baked ext4 image (same recipe as exec_real_vm) ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();

    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-pause-resume-test".into(),
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

    // ---- 2. Boot the microVM on the FORK binary ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle (agentd + stamp + sentinel) and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.firecracker_bin = fork_bin;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "engram-pause-resume-test".into(),
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
    let sandbox_id = backend.create(spec).await.expect("create");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 3. Baseline: exec over vsock works before the pause ----
    let req = ExecRequest {
        command: vec!["echo".into(), "pre-pause".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let stream = wait_for_agent(&backend, sandbox_id, &req, Duration::from_secs(20))
        .await
        .expect("agent never came up before the pause");
    let (stdout, _, exit) = drain(stream.events).await;
    assert_eq!(stdout, b"pre-pause\n");
    assert_eq!(exit, Some(0));

    // ---- 4. Plain pause → resume (the rung-2 park/un-pause cycle) ----
    backend.pause(sandbox_id).await.expect("pause");
    tokio::time::sleep(Duration::from_millis(200)).await;
    backend.resume(sandbox_id).await.expect("resume");

    // ---- 5. The property: vsock still delivers after the un-pause ----
    // Pre-fix this never completes (the RX gate is armed with nothing to
    // ack: the agentd connect/exec black-holes), so the bounded budget IS
    // the assertion.
    let req = ExecRequest {
        command: vec!["echo".into(), "post-resume".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };
    // Hard outer timeout as well: a gated CONNECT can make the dial BLOCK
    // (not error), which would hang the test instead of failing it.
    let stream = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_agent(&backend, sandbox_id, &req, Duration::from_secs(15)),
    )
    .await
    .expect("exec dial blocked after plain pause->resume (vsock RX gated?)")
    .expect("vsock black-holed after plain pause->resume (RX gate armed with no reset to ack?)");
    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(
        stdout,
        b"post-resume\n",
        "stdout mismatch after resume (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(exit, Some(0), "expected clean exit after resume");

    backend.destroy(sandbox_id).await.expect("destroy");
}

/// Poll `exec_stream` once every 500ms for up to `budget` (same shape as
/// exec_real_vm's helper — the in-guest agent needs a moment to bind vsock,
/// and post-resume the first connect can race the vCPUs waking).
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
