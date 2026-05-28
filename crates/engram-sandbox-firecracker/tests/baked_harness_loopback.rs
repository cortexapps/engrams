//! ADR 0021 P1.6: end-to-end FC test for the baked-in harness path.
//!
//! Replaces the deleted `harness_loopback.rs` (which mounted the
//! harness as a separate ext4 substrate the host PATCH'd in via the
//! retired option-D `swap_harness_drive`). The new model bakes the
//! harness directly into the image rootfs at the manifest-declared
//! `[harness] exec` path: this test exercises that by COPY'ing the
//! prebuilt `engram-harness-noop` binary into `/opt/noop/harness`
//! during the image bake, then booting it through `FirecrackerBackend`
//! and verifying the harness child:
//!
//! 1. Is exec'd by agentd from the rootfs path (no drive mount).
//! 2. Dials back to the host on vsock port `HARNESS_VSOCK_PORT`
//!    (1026).
//! 3. Plays the noop attach + RunStarted handshake the host's
//!    HarnessSink expects.
//!
//! Heavy test (~30 s on the dev VM). Preconditions: Linux + KVM +
//! Firecracker + Docker reachable + prebuilt musl `engram-agentd` /
//! `engram-harness-noop`. The canonical entry point is
//! `scripts/run-boot-test.sh baked_harness_loopback`, which rebuilds
//! the musl binaries before invoking cargo test. Direct
//! `cargo test --ignored` skips that and risks running against a
//! stale agent.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SessionId;
use engram_core::types::sandbox::{AgentSpec, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_harness_proto::{
    read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessEvent, HarnessFrame,
    HARNESS_VSOCK_PORT,
};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::time::timeout;

use common::{fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn baked_noop_harness_emits_run_started() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }

    // ---- 0. Locate prebuilt musl artifacts -----------------------
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let musl_release = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release");
    let agentd_bin = musl_release.join("engram-agentd");
    let noop_bin = musl_release.join("engram-harness-noop");
    if !agentd_bin.exists() || !noop_bin.exists() {
        eprintln!(
            "SKIP: missing prebuilt musl binaries. The script rebuilds them:\n  \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh baked_harness_loopback\n  \
             Or manually:\n  \
             cargo build -p engram-agentd -p engram-harness-noop \\\n  \
                   --target x86_64-unknown-linux-musl --release\n  \
             Expected paths:\n    {}\n    {}",
            agentd_bin.display(),
            noop_bin.display(),
        );
        return;
    }

    // ---- 1. Bake an image with the noop binary at /opt/noop/harness
    // The image manifest's `[harness]` block declares the launch
    // contract; for this test we drive `start_agent` directly with
    // argv (it's an FC-backend integration test, not a coord-flow
    // test), so the manifest doesn't strictly have to land — but we
    // include it so the rootfs matches what a real session-create
    // would produce.
    let src = tempfile::tempdir().expect("source dir");
    std::fs::copy(&noop_bin, src.path().join("harness")).expect("copy noop into build context");
    std::fs::write(
        src.path().join("Dockerfile"),
        // debian-slim has glibc + /bin/sh; matches agentd's musl
        // contract well (musl binaries run fine on glibc rootfses).
        // COPY drops the noop binary into the canonical baked path.
        "FROM debian:bookworm-slim\n\
         RUN mkdir -p /opt/noop\n\
         COPY harness /opt/noop/harness\n\
         RUN chmod +x /opt/noop/harness\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        // Custom-harness form: `name` + absolute `exec` + no
        // `version` (the binary is whatever this Dockerfile COPY'd).
        // The baker validates this against the rootfs at bake time
        // (P0 `validate_custom_harness`).
        r#"
name = "baked-noop-test"

[harness]
name = "noop"
exec = "/opt/noop/harness"
"#,
    )
    .unwrap();

    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "baked-noop-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agentd_bin,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
            parent_disk_bootstrap_path: None,
            parent_disk_chunks_blob_digest: None,
        })
        .await
        .expect("ext4 bake with agent injection + baked noop harness");

    // ---- 2. Set up FC backend + harness sink ---------------------
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Unprivileged test — matches lifecycle.rs / exec_real_vm.rs.
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    // HarnessSink captures inbound vsock connections from the noop
    // harness into a tokio channel so the test thread can drive the
    // attach + run-started handshake. Mirrors what HarnessHub does
    // in production.
    let (sink_tx, mut sink_rx) =
        tokio::sync::mpsc::unbounded_channel::<engram_core::traits::HarnessByteStream>();
    let sink: engram_core::traits::HarnessSink = {
        let sink_tx = sink_tx.clone();
        Arc::new(move |stream| {
            let _ = sink_tx.send(stream);
        })
    };
    backend.set_harness_sink(sink);

    let spec = SandboxSpec {
        image: "baked-noop-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
    };
    let sandbox_id = backend.create(spec).await.expect("create sandbox");

    // Keep the jail dir around on failure so we can post-mortem
    // firecracker.log (same pattern as exec_real_vm.rs).
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 3. SpawnHarness with the rootfs path --------------------
    // Mirrors what coord's `resolve_harness` would build for a
    // `HarnessDial::Vsock` backend (FC). No drive mount, no
    // substrate — just the manifest-resolved `exec` + the standard
    // dial flags. Wait for agent ready first so the spawn doesn't
    // race the kernel boot.
    wait_for_agent_ready(&backend, sandbox_id, Duration::from_secs(20))
        .await
        .expect("agent never came up — see firecracker.log under work_dir");

    let session_id = SessionId::new();
    let agent = AgentSpec {
        argv: vec![
            "/opt/noop/harness".into(),
            "--port".into(),
            HARNESS_VSOCK_PORT.to_string(),
            "--session-id".into(),
            session_id.to_string(),
        ],
        env: HashMap::new(),
        host_ca_pem: None,
    };
    backend
        .start_agent(sandbox_id, agent)
        .await
        .expect("start_agent — agentd should exec the baked harness");

    // ---- 4. Drive the noop's attach + RunStarted handshake -------
    let mut stream = timeout(Duration::from_secs(15), sink_rx.recv())
        .await
        .expect("HarnessSink timed out — noop harness didn't dial back on vsock 1026")
        .expect("HarnessSink channel closed");

    let attach: HarnessAttach = read_msg(&mut stream)
        .await
        .expect("read HarnessAttach from noop");
    assert_eq!(
        attach.session_id, session_id,
        "noop should echo the --session-id flag we passed it",
    );

    write_msg(
        &mut stream,
        &HarnessAttachAck {
            ok: true,
            message: None,
        },
    )
    .await
    .expect("write HarnessAttachAck");

    let frame: HarnessFrame = read_msg(&mut stream)
        .await
        .expect("read HarnessFrame::Event after attach ack");
    match frame {
        HarnessFrame::Event(HarnessEvent::RunStarted { run_id, .. }) => {
            assert!(
                !run_id.is_empty(),
                "noop's RunStarted should carry a non-empty run_id",
            );
        }
        other => panic!(
            "expected HarnessEvent::RunStarted as the first post-attach frame; got {other:?}",
        ),
    }

    // ---- 5. Cleanup ---------------------------------------------
    // Drop the stream first so the noop's writer task wakes up on
    // the broken pipe; otherwise destroy waits on a still-running
    // child.
    drop(stream);
    backend.destroy(sandbox_id).await.expect("destroy sandbox");
}

/// Wait for agentd to dial its ready port. The backend exposes this
/// indirectly via `start_agent` (which polls `wait_agent_ready`
/// internally), but for this test we want the spawn to happen
/// *after* readiness — so we poll a no-op SpawnHarness ourselves.
///
/// Implementation: lean on `exec_stream` like `exec_real_vm.rs`
/// does. A successful `echo` proves agentd is reachable, after which
/// the real SpawnHarness in step 3 is essentially zero-wait.
async fn wait_for_agent_ready(
    backend: &FirecrackerBackend,
    sandbox_id: engram_core::types::ids::SandboxId,
    budget: Duration,
) -> Result<(), engram_core::SandboxError> {
    use engram_core::types::sandbox::ExecRequest;
    let req = ExecRequest {
        command: vec!["true".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let deadline = std::time::Instant::now() + budget;
    let mut last_err = None;
    while std::time::Instant::now() < deadline {
        match backend.exec_stream(sandbox_id, req.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_err.expect("at least one poll attempt"))
}
