//! ADR 0026: end-to-end FC test for the in-guest artifact-upload vsock
//! bridge.
//!
//! Boots a real microVM with `engram-agentd` baked in, registers an
//! `UploadSink` on the backend (mirroring what the coordinator wires in
//! production), then `exec_stream`s a shell that writes a file and runs
//! `engram-agentd share-file` inside the guest, verifying the full
//! round-trip:
//!
//!   guest `engram-agentd share-file --file <p>`
//!     → dials AF_VSOCK host:UPLOAD_VSOCK_PORT (1029)
//!     → FC per-sandbox upload UDS accept loop
//!     → our UploadSink reads the `UploadRequest` header + the raw body,
//!       captures the bytes, writes an `UploadResponse::Shared`
//!     → agentd prints a confirmation to stdout
//!
//! The sink captures the streamed body so the assertion proves the bytes
//! crossed the wire intact — including a payload LARGER than the 16 MiB
//! single-frame cap, exercising the post-header streaming path.
//!
//! Heavy test (~30 s on the dev VM). Same preconditions as
//! `forge_loopback`; canonical entry point is
//! `scripts/run-boot-test.sh upload_loopback`.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SessionId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_harness_proto::{read_msg, write_msg, UploadOp, UploadRequest, UploadResponse};
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;

use common::{drain, fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn share_file_round_trips_over_vsock() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("mksquashfs") {
        return;
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agentd_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agentd_bin.exists() {
        eprintln!(
            "SKIP: missing prebuilt musl engram-agentd. Rebuild via:\n  \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh upload_loopback\n  \
             (expected at {})",
            agentd_bin.display(),
        );
        return;
    }

    // ---- 1. Bake a minimal (harness-less) image with agentd injected. ----
    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
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

    // ---- 2. FC backend + an UploadSink that drains the body. ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle (agentd + stamp + sentinel) and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_bin);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    // Captured by the sink for post-run assertions: broker token, ext,
    // advertised size, and the full streamed body.
    struct Captured {
        token: String,
        ext: String,
        size_bytes: u64,
        body: Vec<u8>,
    }
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let captured_sink = captured.clone();
    let sink: engram_core::traits::UploadSink = Arc::new(move |stream| {
        let captured = captured_sink.clone();
        tokio::spawn(async move {
            let (mut read_half, mut write_half) = tokio::io::split(stream);
            let header: UploadRequest = match read_msg(&mut read_half).await {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("upload sink: header read failed: {e}");
                    return;
                }
            };
            let UploadOp::ShareFile {
                ext, size_bytes, ..
            } = header.op.clone();
            let mut body = vec![0u8; size_bytes as usize];
            if let Err(e) = read_half.read_exact(&mut body).await {
                eprintln!("upload sink: body read failed: {e}");
                return;
            }
            *captured.lock() = Some(Captured {
                token: header.broker_token.clone(),
                ext,
                size_bytes,
                body,
            });
            let resp = UploadResponse::Shared {
                artifact_id: "0190testartifactid".into(),
                media_type: "image/png".into(),
                size_bytes,
            };
            let _ = write_msg(&mut write_half, &resp).await;
        });
    });
    backend.set_upload_sink(sink);

    let spec = SandboxSpec {
        image: "upload-loopback-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![staged.agentd_slot()],
    };
    let sandbox_id = backend.create(spec).await.expect("create sandbox");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 3. Write a >16 MiB "png" in the guest and share it. The leading
    //         PNG magic satisfies the client-side ext check; the size
    //         forces the post-header streaming path (past the frame cap). ----
    let session_id = SessionId::new();
    let mut exec_env = HashMap::new();
    exec_env.insert("ENGRAM_SESSION_ID".to_string(), session_id.to_string());
    exec_env.insert("ENGRAM_UPLOAD_TOKEN".to_string(), "utok-xyz789".to_string());
    exec_env.insert("ENGRAM_TRANSPORT".to_string(), "vsock".to_string());
    // 20 MiB: PNG magic header + zero fill. `head -c` makes the size exact.
    let make_and_share = "set -e; \
         printf '\\211PNG\\r\\n\\032\\n' > /tmp/shot.png; \
         head -c 20971512 /dev/zero >> /tmp/shot.png; \
         /run/engram/engram-agentd share-file --file /tmp/shot.png --caption 'boot test'";
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), make_and_share.into()],
        stdin: None,
        env: exec_env,
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
    };

    let stream = wait_for_agent(&backend, sandbox_id, &req, Duration::from_secs(25))
        .await
        .expect("agent never came up — see firecracker.log under work_dir");
    let (stdout, stderr, exit) = drain(stream.events).await;

    assert_eq!(
        exit,
        Some(0),
        "share-file should exit 0 (stdout: {}, stderr: {})",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );
    assert!(
        String::from_utf8_lossy(&stdout).contains("as artifact"),
        "share-file should print a confirmation (stdout: {})",
        String::from_utf8_lossy(&stdout),
    );

    let got = captured.lock().take().expect("sink captured the upload");
    assert_eq!(
        got.token, "utok-xyz789",
        "broker token crossed the wire intact"
    );
    assert_eq!(got.ext, "png");
    assert_eq!(
        got.size_bytes, 20_971_520,
        "header advertised the full size"
    );
    assert_eq!(
        got.body.len(),
        20_971_520,
        "full body streamed past the 16 MiB frame cap"
    );
    assert_eq!(&got.body[..8], b"\x89PNG\r\n\x1a\n", "PNG magic preserved");

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
