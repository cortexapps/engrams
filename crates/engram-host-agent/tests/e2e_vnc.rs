//! True end-to-end VNC-tab test (ADR 0065/0066) over a real Firecracker sandbox
//! with the `browser` RO bundle mounted.
//!
//! Exercises the exact chain prod uses for the BROWSER tab, end to end over a
//! real microVM (the subsystem tests — `pooled_backend::tests` for the
//! `start_browser` forwarding, `engram-agentd::browser::tests` against a fake
//! launcher, `proxy_port::tests` for the relay framing — each cover one link;
//! this boots a real VM, mounts the real `browser` bundle, and reads the real
//! x11vnc over the real relay):
//!
//!   orchestrator `/vnc` → coord `EnsureBrowser` →
//!     `PooledBackend.start_browser(id)` (vsock → in-VM agentd → engram-browser
//!       launcher → Xvfb + openbox + chromium + x11vnc on the guest's loopback) →
//!     `PooledBackend.open_guest_stream(id, PROXY_PORT_VSOCK_PORT)` + a
//!       `RelayConnect{5900}` header (ADR 0066: agentd dials 127.0.0.1:5900
//!       in-guest and splices) →
//!     raw RFB byte round-trip with x11vnc.
//!
//! The assertion is the RFB ProtocolVersion handshake: x11vnc speaks FIRST in
//! RFB, sending the 12-byte `RFB 003.00x\n` banner the instant a viewer connects
//! (RFC 6143 §7.1.1). So the first bytes off the relay stream must start with
//! `RFB 003.` — proving the full chain ran: the browser bundle activated, the
//! launcher brought x11vnc up on loopback, agentd's relay dialed it in-guest,
//! and the bytes spliced back over vsock.
//!
//! Coverage: `e2e_vnc_cold_via_pooled_backend` — cold-created FC sandbox. (The
//! warm/netns path is gone with ADR 0066 — the relay reaches guest loopback
//! identically cold and warm, so there's no separate warm case to pin here.)
//!
//! Run on the dev VM (needs `just bundles-squashfs` first so `browser.squashfs`
//! is staged under `var/shared/`):
//!
//! ```sh
//! just bundles-squashfs
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! sudo -E cargo nextest run -p engram-host-agent --test e2e_vnc \
//!     --run-ignored all --test-threads=1
//! ```

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::{HarnessByteStream, SandboxBackend};
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec,
};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{InitInjection, BuildRequest, Builder, DockerCli, Format, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

// Shared FC e2e harness floor (preflight / root-check / host-state cleanup /
// guest-IP poll) — the same module `e2e_shell` uses.
mod common;
use common::{cleanup_host_state, fc_preflight, require_root, wait_for_guest_endpoints};

/// ADR 0080: the musl agentd the staged bundle fixture packs (the same
/// binary the old bake used to inject into the rootfs).
fn agentd_musl_bin() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest).join("../../target/x86_64-unknown-linux-musl/release/engram-agentd")
}

/// Where `just bundles-squashfs` stages the content-addressed squashfs bundles
/// (`<sha>.squashfs` + `current.json`) in the repo checkout. The FC backend's
/// `staged_bundle_path` joins `<bundle_dir>/<sha>.squashfs`, so pointing
/// `FirecrackerConfig.bundle_dir` here lets the cold-create attach the real
/// `browser` generation exactly as a prod host resolves it against
/// `/var/lib/engram/shared`.
const STAGED_BUNDLES_REL: &str = "var/shared";

/// Resolve the repo-root-relative `var/shared/` staging dir + the `browser`
/// bundle's content sha from `current.json`. Returns `None` (with a `SKIP:`
/// line) if the bundle wasn't staged — so a dev box that hasn't run
/// `just bundles-squashfs` skips cleanly instead of failing spuriously. CI's
/// firecracker lane runs `just bundles-squashfs` before this test, so the
/// bundle is always present there.
fn resolve_browser_bundle() -> Option<(PathBuf, String)> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    // crates/engram-host-agent → repo root is two levels up.
    let repo_root = Path::new(&manifest).join("..").join("..");
    let bundle_dir = repo_root.join(STAGED_BUNDLES_REL);
    let stamp_path = bundle_dir.join(AuxRoDrive::CURRENT_STAMP);

    let stamp = match std::fs::read_to_string(&stamp_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "SKIP: bundle stamp {} unreadable ({e}); run `just bundles-squashfs` first",
                stamp_path.display()
            );
            return None;
        }
    };
    // current.json is a flat {"name": "<sha>", ...} map (just bundles-squashfs).
    let map: HashMap<String, String> = match serde_json::from_str(&stamp) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "SKIP: bundle stamp {} is not valid JSON: {e}",
                stamp_path.display()
            );
            return None;
        }
    };
    let sha = match map.get("browser") {
        Some(s) => s.clone(),
        None => {
            eprintln!(
                "SKIP: no `browser` entry in {} — the browser bundle build was skipped \
                 (needs Docker); run `just bundles-squashfs` on a Docker-capable host",
                stamp_path.display()
            );
            return None;
        }
    };
    let staged = bundle_dir.join(AuxRoDrive::staged_file_name(&sha));
    if !staged.exists() {
        eprintln!(
            "SKIP: browser bundle {} stamped but not staged at {}",
            sha,
            staged.display()
        );
        return None;
    }
    // Canonicalize so the FC backend (which may run from a jail cwd) gets an
    // absolute path.
    let bundle_dir = bundle_dir.canonicalize().unwrap_or(bundle_dir);
    Some((bundle_dir, sha))
}

/// Bake a debian-bookworm rootfs with `engram-agentd` injected. The `browser`
/// bundle is mounted as a separate RO squashfs aux drive (NOT baked into the
/// rootfs) — exactly the production attach. The base is glibc debian-slim so
/// the bundle's glibc-linked chromium/Xvfb/x11vnc run against it.
///
/// Network strategy: like `e2e_shell`'s bake, all network work is HOST-side
/// (none needed here — the rootfs is just debian-slim + the injected agentd),
/// so the in-container build needs no DNS (the dev-vm Docker daemon's apt DNS
/// is flaky).
async fn bake_browser_rootfs(repo: &str) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    assert!(
        agent_bin.exists(),
        "musl agentd not at {} — build it first \
         (`cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release`)",
        agent_bin.display(),
    );

    let src = tempfile::tempdir().expect("source dir");

    // debian-bookworm-slim ships glibc + /bin/sh (dash). The browser bundle
    // (chromium + Xvfb + x11vnc + openbox) is glibc-linked against this same
    // bookworm baseline (see deploy/bundles/browser/build.sh), so it runs
    // here. `/workspace` mirrors e2e_shell's bake (the init shim cd's into it).
    // No browser packages in the rootfs itself — the bundle carries them.
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\n\
         RUN mkdir -p /workspace /opt/engram/dyn\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        format!("name = \"{repo}\"\n"),
    )
    .unwrap();

    let images_dir = tempfile::tempdir().expect("images");
    let images_dir_path = images_dir.path().to_path_buf();
    std::mem::forget(images_dir);

    let chunk_root = tempfile::tempdir().expect("chunk store");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    std::mem::forget(chunk_root);
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: repo.into(),
            tag: "warm-1".into(),
            images_dir: images_dir_path.clone(),
            format: Format::Ext4,
            init_injection: Some(InitInjection {
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("bake ext4");

    outcome.rootfs_path
}

/// Build the `browser` bundle aux drive for slot 0: `dyn_0` at
/// `/opt/engram/dyn/0`, squashfs, content-resolved to `sha`. This is exactly
/// what coord's per-session resolve hands the host for a profile that selects
/// the `browser` bundle (ADR 0055 reserved-slot model).
fn browser_aux_drive(sha: String) -> AuxRoDrive {
    AuxRoDrive {
        sha256: Some(sha),
        ..AuxRoDrive::reserved_slot(0)
    }
}

/// Reach the in-guest x11vnc (`:5900`) over the ADR-0066 vsock port relay via
/// the `PooledBackend` that wraps the FC backend — the exact chain the
/// orchestrator's `/vnc` route drives (`EnsureBrowser` → `start_browser`, then
/// a `PortRelay`/`open_guest_stream` reach). Going through `PooledBackend` is
/// what catches the `start_browser` / `open_guest_stream` forwarding bugs (the
/// ADR 0065 PooledBackend guards).
async fn open_vnc_relay_stream(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
) -> HarnessByteStream {
    // start_browser asks agentd to bring up Xvfb + chromium + x11vnc (bound to
    // the guest's loopback :5900) and probe it ready before replying, so the
    // relay dial below finds a listener.
    pooled
        .start_browser(id)
        .await
        .expect("pooled.start_browser must succeed (browser bundle activated + x11vnc up)");

    // Open a vsock stream to agentd's port relay, then ask it to dial the guest's
    // 127.0.0.1:5900 (ADR 0066): agentd makes the loopback connection in-guest
    // and splices raw RFB bytes back over vsock. FC returns Some(stream); None
    // would mean no VM boundary (Process), which this FC test never hits.
    let mut stream = pooled
        .open_guest_stream(id, engram_harness_proto::PROXY_PORT_VSOCK_PORT)
        .await
        .expect("open_guest_stream must succeed")
        .expect("FC backend must return Some(vsock stream), not None");
    engram_harness_proto::write_msg(
        &mut stream,
        // x11vnc's RFB port; matches engram_agentd::browser::DEFAULT_VNC_PORT.
        &engram_harness_proto::RelayConnect { target_port: 5900 },
    )
    .await
    .expect("write RelayConnect{5900} to agentd relay");
    let ack: engram_harness_proto::RelayAck = engram_harness_proto::read_msg(&mut stream)
        .await
        .expect("read RelayAck from agentd relay");
    assert!(
        ack.ok,
        "relay dial to guest 127.0.0.1:5900 failed (x11vnc not on loopback?): {:?}",
        ack.error
    );
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + sudo + the staged browser bundle"]
async fn e2e_vnc_cold_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    let (bundle_dir, browser_sha) = match resolve_browser_bundle() {
        Some(b) => b,
        None => return,
    };
    cleanup_host_state();

    // ---- 1. Bake a glibc debian rootfs with agentd (no browser baked in) ----
    let rootfs_path = bake_browser_rootfs("engram-e2e-vnc-cold").await;

    // ---- 2. Wrap FC in PooledBackend, pointing bundle_dir at var/shared ----
    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_musl_bin());
    // One bundle_dir per backend: copy the repo-staged browser squashfs
    // into the fixture dir (the browser rides a resolved sha in the spec,
    // so only its staged file must exist there).
    std::fs::copy(
        bundle_dir.join(AuxRoDrive::staged_file_name(&browser_sha)),
        staged
            .bundle_dir
            .join(AuxRoDrive::staged_file_name(&browser_sha)),
    )
    .expect("copy browser bundle into fixture dir");
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    // ---- 3. Cold-create with the browser bundle mounted at dyn_0 ----
    let spec = SandboxSpec {
        image: "engram-e2e-vnc-cold".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        // The browser stack (Xvfb + chromium) is heavier than ttyd; give it
        // real headroom so the cold start doesn't thrash.
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 1024 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![browser_aux_drive(browser_sha.clone()), staged.agentd_slot()],
    };
    let sandbox_id = pooled.create(spec).await.expect("create");

    // ---- 4. Wait for in-VM agentd (guest_endpoints resolves once vsock answers) ----
    let _endpoints = wait_for_guest_endpoints(&pooled, sandbox_id, Duration::from_secs(30)).await;

    // ---- 5. Activate the bundle: a readiness-probe SpawnHarness (empty argv)
    //         runs agentd's `engram_session_bundles::activate`, which mounts +
    //         symlinks the bundle's `bin/engram-browser` onto /usr/local/bin
    //         (on agentd's PATH). Without this, start_browser's
    //         `Command::new("engram-browser")` would ENOENT. This is the same
    //         empty-argv path a DevVm-mode session takes (see e2e_harness). ----
    // Relies on harness_supervisor::spawn running activate() (which wire_bins
    // symlinks engram-browser onto PATH) BEFORE the empty-argv early-return —
    // see crates/engram-agentd/src/harness_supervisor.rs (activate() call at
    // ~L162, the `req.argv.is_empty()` early-return at ~L170).
    pooled
        .start_agent(
            sandbox_id,
            AgentSpec {
                // ADR 0073: epoch 1 = the test's sole binding generation.
                binding_epoch: 1,
                argv: Vec::new(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("start_agent (empty-argv readiness probe → activates browser bundle)");

    // ---- 6. Reach x11vnc over the ADR-0066 relay (the /vnc route's path) ----
    let mut stream = open_vnc_relay_stream(&pooled, sandbox_id).await;

    // ---- 7. Assert the RFB ProtocolVersion banner comes back ----
    // x11vnc speaks first in RFB: the 12-byte ProtocolVersion "RFB 003.00x\n"
    // (RFC 6143 §7.1.1). A banner here proves the FULL chain ran: the browser
    // bundle activated → the launcher brought x11vnc up on the guest's loopback
    // → agentd's relay dialed 127.0.0.1:5900 in-guest → raw RFB bytes spliced
    // back over vsock. (Banner-necessary-but-not-sufficient — x11vnc serves it
    // even if chromium died — but start_browser's readiness probe already gated
    // on x11vnc AND chromium's CDP endpoint, so a banner means the stack is live.)
    let mut banner = [0u8; 12];
    timeout(Duration::from_secs(30), stream.read_exact(&mut banner))
        .await
        .expect("RFB ProtocolVersion banner within 30s of opening the relay stream")
        .expect("relay stream closed before any RFB bytes");
    eprintln!("--- received RFB banner from x11vnc: {banner:?} ---");
    assert!(
        banner.starts_with(b"RFB 003."),
        "expected RFB ProtocolVersion banner (`RFB 003.`); got {banner:?}",
    );

    // ---- 8. Teardown ----
    drop(stream);
    pooled
        .stop_browser(sandbox_id)
        .await
        .expect("stop_browser (tear down the in-guest browser stack)");
    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}
