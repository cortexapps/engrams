//! ADR 0023: end-to-end FC test for the in-guest forge vsock bridge.
//!
//! Boots a real microVM with `engram-agentd` baked in, registers a
//! `ForgeSink` on the backend (mirroring what the coordinator wires in
//! production), then `exec_stream`s `engram-agentd forge-credential`
//! inside the guest and verifies the full round-trip:
//!
//!   guest `engram-agentd forge-credential`
//!     → dials AF_VSOCK host:FORGE_VSOCK_PORT (1028)
//!     → FC per-sandbox forge UDS accept loop
//!     → our ForgeSink reads the `ForgeRequest`, writes a `ForgeResponse`
//!     → agentd prints the credential password to stdout
//!
//! The sink echoes the broker token back inside the password so the
//! assertion proves the request carried the in-guest env through intact.
//!
//! Heavy test (~30 s on the dev VM). Same preconditions as
//! `baked_harness_loopback`; canonical entry point is
//! `scripts/run-boot-test.sh forge_loopback`.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SessionId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_harness_proto::{read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format, InitInjection};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn forge_credential_round_trips_over_vsock() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") || !require_bin("mksquashfs") {
        return;
    }

    // ---- 0. Locate the prebuilt musl agentd (carries the ADR 0023
    //         `forge-credential` subcommand). ----
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agentd_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agentd_bin.exists() {
        eprintln!(
            "SKIP: missing prebuilt musl engram-agentd. Rebuild via:\n  \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh forge_loopback\n  \
             (expected at {})",
            agentd_bin.display(),
        );
        return;
    }

    // ---- 1. Bake a minimal (harness-less) image with agentd injected.
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\nRUN mkdir -p /workspace\n",
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
            repo: "forge-loopback-test".into(),
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

    // ---- 2. FC backend + a ForgeSink that answers FetchCredential. ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle (agentd + stamp + sentinel) and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_bin);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    // Echo the broker token back inside the password so the assertion
    // proves the in-guest env → ForgeRequest plumbing is intact.
    let sink: engram_core::traits::ForgeSink = Arc::new(move |mut stream| {
        tokio::spawn(async move {
            let req: ForgeRequest = match read_msg(&mut stream).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("forge sink: read failed: {e}");
                    return;
                }
            };
            // ADR 0056 P3c: `ForgeOp` is single-variant now (PR-open retired),
            // so this match is exhaustive without a fallback arm.
            let resp = match req.op {
                ForgeOp::FetchCredential { .. } => ForgeResponse::Credential {
                    username: "x-access-token".into(),
                    password: format!("ghs_canned_{}", req.broker_token),
                },
            };
            let _ = write_msg(&mut stream, &resp).await;
        });
    });
    backend.set_forge_sink(sink);

    let spec = SandboxSpec {
        image: "forge-loopback-test".into(),
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
    let sandbox_id = backend.create(spec).await.expect("create sandbox");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 3. exec `engram-agentd forge-credential` in the guest. ----
    let session_id = SessionId::new();
    let mut exec_env = HashMap::new();
    exec_env.insert("ENGRAM_SESSION_ID".to_string(), session_id.to_string());
    exec_env.insert("ENGRAM_FORGE_TOKEN".to_string(), "tok-abc123".to_string());
    // The forge client picks its transport via `engram_transport::from_env`;
    // pin vsock so the exec'd process dials the host correctly.
    exec_env.insert("ENGRAM_TRANSPORT".to_string(), "vsock".to_string());
    let req = ExecRequest {
        command: vec![
            "/run/engram/engram-agentd".into(),
            "forge-credential".into(),
            "--host".into(),
            "github.com".into(),
        ],
        stdin: None,
        env: exec_env,
        workdir: None,
        timeout: Some(Duration::from_secs(15)),
    };

    let stream = wait_for_agent(&backend, sandbox_id, &req, Duration::from_secs(25))
        .await
        .expect("agent never came up — see firecracker.log under work_dir");
    let (stdout, stderr, exit) = drain(stream.events).await;

    assert_eq!(
        exit,
        Some(0),
        "forge-credential should exit 0 (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&stdout),
        "ghs_canned_tok-abc123",
        "guest forge-credential should print the credential the sink minted \
         (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );

    backend.destroy(sandbox_id).await.expect("destroy sandbox");
}

/// Poll `exec_stream` until the in-guest agent accepts (it takes a
/// couple of seconds to boot + bind vsock 1024), then return the stream.
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
