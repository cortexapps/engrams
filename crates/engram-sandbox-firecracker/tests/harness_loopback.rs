//! Phase 5 sign-off: harness end-to-end on a real Firecracker microVM.
//!
//! Exercises the full pipeline:
//!   - bake an FC image with `engram-agentd` + `engram-bootstrap`
//!     injected
//!   - build a harness substrate ext4 from a tempdir containing
//!     `engram-harness-noop` at the canonical `<name>/harness` path
//!   - boot a sandbox with both the rootfs and the substrate attached
//!   - register a `HarnessSink` on the FC backend
//!   - call `start_agent` to push a `BootstrapLaunch` for the noop
//!     harness on vsock 1025
//!   - assert 3 `ToolCallCompleted` events arrive over vsock 1026
//!     within 30 s
//!
//! Heavy test (Docker pull + bake + microVM boot, ~30 s on the dev
//! VM); gated `#[ignore]` like the rest of the FC integration suite.
//! Run via:
//!
//!   bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh harness_loopback

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{AgentSpec, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_core::SandboxId;
use engram_harness_proto::HarnessEvent;
use engram_image_builder::{
    AgentInjection, BuildRequest, Builder, DockerCli, Ext4Packer, Format, Mke2fsPacker, Transport,
};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use parking_lot::Mutex;
// Note: harness-proto's `read_msg` is the canonical decode path; we
// use it instead of hand-rolling the length-prefix bincode decode.

use common::{fc_preflight, require_bin};

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn noop_harness_round_trips_three_tool_calls_on_real_fc() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }

    // The canonical entry point is `scripts/run-boot-test.sh
    // harness_loopback`, which rebuilds these musl binaries before
    // calling `cargo test`. Direct `cargo test --ignored` invocations
    // skip that step and risk silently running against a stale binary
    // (e.g. an `engram-bootstrap` from before commit 7ba0e61 doesn't
    // write the readiness byte, leading to a 15s timeout in
    // `start_agent`). The SKIP message points at the script.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let musl = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release");
    let agent_bin = musl.join("engram-agentd");
    let bootstrap_bin = musl.join("engram-bootstrap");
    let noop_bin = musl.join("engram-harness-noop");
    for (label, p) in [
        ("engram-agentd", &agent_bin),
        ("engram-bootstrap", &bootstrap_bin),
        ("engram-harness-noop", &noop_bin),
    ] {
        if !p.exists() {
            eprintln!(
                "SKIP: {label} not built at {}.\n  \
                 Run via the script — it builds these binaries fresh:\n    \
                 bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh harness_loopback\n  \
                 Or build manually:\n    \
                 cargo build -p {label} --target x86_64-unknown-linux-musl --release",
                p.display(),
            );
            return;
        }
    }

    // ---- 1. Bake the rootfs with agent+bootstrap injected ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        // debian-slim: minimal, glibc-based, but our musl binaries
        // are statically linked (PIE-static via .cargo/config.toml's
        // relocation-model=static) so the libc mismatch doesn't
        // matter.
        "FROM debian:bookworm-slim\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-harness-loopback-test\"\n",
    )
    .unwrap();

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
            repo: "engram-harness-loopback-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent_bin,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
                bootstrap_binary: Some(bootstrap_bin),
            }),
        })
        .await
        .expect("ext4 bake");

    // ---- 2. Build the harness substrate from a tempdir ----
    // Layout: <substrate_src>/noop/harness — matches HarnessRegistry's
    // expected pack layout. The init shim mounts the substrate at
    // /run/engram/harnesses, so the in-guest path is
    // /run/engram/harnesses/noop/harness.
    let substrate_src = tempfile::tempdir().expect("substrate src");
    let pack_dir = substrate_src.path().join("noop");
    std::fs::create_dir_all(&pack_dir).unwrap();
    tokio::fs::copy(&noop_bin, pack_dir.join("harness"))
        .await
        .expect("copy noop into substrate");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(pack_dir.join("harness"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(pack_dir.join("harness"), perms).unwrap();

    let substrate_path: PathBuf = images.path().join("harness-substrate.img");
    // Match the production harness_substrate::build sizing — overhead
    // for ext4 metadata + room to grow. A 16 MiB minimum prevents
    // mke2fs from rejecting tiny images.
    let dir_size = std::fs::metadata(pack_dir.join("harness")).unwrap().len();
    let substrate_size = engram_image_builder::recommended_size(dir_size).max(16 * 1024 * 1024);
    Mke2fsPacker::default()
        .pack(substrate_src.path(), &substrate_path, substrate_size)
        .await
        .expect("pack harness substrate");

    // ---- 3. Set up FC backend with a HarnessSink ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // Unprivileged test — TAP/iptables provisioning needs root.
    // The harness path doesn't need IP networking; vsock is its own
    // transport.
    cfg.net_pool = None;
    let backend = Arc::new(FirecrackerBackend::new(work.path(), cfg));

    // Sink that drives the bytes into a local copy of HarnessHub-ish
    // logic — for this smoke test we don't need a full hub, just
    // proof that events flow. Decode events in-task and push onto a
    // shared Vec.
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: engram_core::traits::HarnessSink = Arc::new(move |stream| {
        let collected = collected_for_sink.clone();
        // Per-connection: do the HarnessAttach handshake (read first
        // frame, ack ok), then drain steady-state frames until the
        // harness closes. Mirrors `engram_host_agent::harness::run_connection`
        // but inline so the test doesn't need to pull in the full
        // hub. Decode failure stops the loop.
        tokio::spawn(async move {
            let mut stream = stream;
            let (mut reader, mut writer) = tokio::io::split(stream.as_mut());
            let _attach: engram_harness_proto::HarnessAttach =
                match engram_harness_proto::read_msg(&mut reader).await {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("test sink: handshake read failed: {e}");
                        return;
                    }
                };
            let ack = engram_harness_proto::HarnessAttachAck {
                ok: true,
                message: None,
            };
            if let Err(e) = engram_harness_proto::write_msg(&mut writer, &ack).await {
                eprintln!("test sink: ack write failed: {e}");
                return;
            }
            while let Ok(frame) =
                engram_harness_proto::read_msg::<_, engram_harness_proto::HarnessFrame>(&mut reader)
                    .await
            {
                if let engram_harness_proto::HarnessFrame::Event(ev) = frame {
                    collected.lock().push(ev);
                }
            }
        });
    });
    backend.set_harness_sink(sink);

    // ---- 4. Create the sandbox with rootfs + substrate ----
    let sandbox_spec = SandboxSpec {
        image: "engram-harness-loopback-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        harness_pack_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        harness_substrate: Some(substrate_path),
        network: Default::default(),
        canonical_memory_manifest: None,
    };
    let sandbox_id: SandboxId = backend.create(sandbox_spec).await.expect("create");

    // ---- 5. Push BootstrapLaunch via start_agent ----
    let session_id = engram_core::SessionId::new();
    let port = engram_harness_proto::HARNESS_VSOCK_PORT.to_string();
    let argv = vec![
        "/run/engram/harnesses/noop/harness".to_string(),
        "--vsock-host".into(),
        port,
        "--session-id".into(),
        session_id.to_string(),
    ];
    let mut env = HashMap::new();
    env.insert("ENGRAM_NOOP_TOOL_CALLS".into(), "3".into());
    env.insert("ENGRAM_NOOP_INTERVAL_MS".into(), "10".into());
    env.insert("ENGRAM_NOOP_TOOL_CALL_DURATION_MS".into(), "10".into());
    if let Err(e) = backend
        .start_agent(sandbox_id, AgentSpec { argv, env })
        .await
    {
        // Without this dump, a start_agent timeout looks like
        // "the byte didn't arrive" with no way to see why bootstrap
        // didn't write it. Show the guest console so the in-VM
        // failure mode (init crashed, bootstrap panicked,
        // ENGRAM_TRANSPORT misconfigured, etc.) is visible.
        let jail_dir = work.path().join(sandbox_id.to_string());
        let log_path = jail_dir.join("firecracker.log");
        if let Ok(s) = tokio::fs::read_to_string(&log_path).await {
            eprintln!("--- firecracker.log tail (start_agent failure) ---");
            for line in s.lines().rev().take(120).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            eprintln!("--- end firecracker.log ---");
        } else {
            eprintln!("(firecracker.log not readable at {})", log_path.display());
        }
        let _ = backend.destroy(sandbox_id).await;
        panic!("start_agent: {e}");
    }

    // ---- 6. Wait for 3 ToolCallCompleted events ----
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut completed = 0usize;
    while std::time::Instant::now() < deadline {
        completed = collected
            .lock()
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ToolCallCompleted { .. }))
            .count();
        if completed >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let snapshot = collected.lock().clone();
    // On assertion failure, dump the firecracker.log tail so the
    // failure tells us why the in-guest path is broken (init didn't
    // run, harness binary missing, vsock dial failed, etc.) instead
    // of just "0 events".
    if completed < 3 {
        let jail_dir = work.path().join(sandbox_id.to_string());
        let log_path = jail_dir.join("firecracker.log");
        if let Ok(s) = tokio::fs::read_to_string(&log_path).await {
            eprintln!("--- firecracker.log tail ---");
            for line in s.lines().rev().take(80).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            eprintln!("--- end firecracker.log ---");
        } else {
            eprintln!("(firecracker.log not readable at {})", log_path.display());
        }
    }
    backend.destroy(sandbox_id).await.expect("destroy");

    assert!(
        completed >= 3,
        "expected 3 ToolCallCompleted events within 30s; saw {completed}. \
         All events: {:?}",
        snapshot,
    );
}
