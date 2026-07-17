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
//! The test first checks the RFB ProtocolVersion handshake, then drives a local
//! fixture through Playwright CLI, records a real WebM, verifies its semantic
//! snapshot and annotated screenshot, completes the RFB handshake, requests a
//! tiny RAW framebuffer rectangle, and decodes the fixture's high-contrast
//! center pixel. This proves x11vnc and Chromium are alive and expose the same
//! painted page, and that explicit video needs no runtime download.
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
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// High-contrast fixture rendered by the shared Chrome. The center of the page
/// is deliberately magenta so the RFB assertion can decode a tiny 32x32 raw
/// rectangle rather than transfer a full 1440x1080 framebuffer through the
/// 2-vCPU CI microVM.
const BROWSER_E2E_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Engram browser paint fixture</title>
<style>html,body{height:100%;margin:0}body{background:#d726ff;color:#101828;font:32px sans-serif}
h1{margin:0;padding:48px;background:#fff}button{margin:48px;padding:16px;font:20px sans-serif}</style></head>
<body><h1>ENGRAM_BROWSER_PAINTED</h1><button>Annotated target</button></body></html>"#;

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
async fn bake_browser_rootfs(busybox: &Path) -> PathBuf {
    let images_dir = tempfile::tempdir().expect("images");
    let images_dir_path = images_dir.path().to_path_buf();
    std::mem::forget(images_dir);

    let chunk_root = tempfile::tempdir().expect("chunk store");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    std::mem::forget(chunk_root);
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    // debian-slim's userland is stood in for by the static busybox (ADR 0080
    // §D: docker-free bake). The browser bundle (chromium + Xvfb + x11vnc +
    // openbox) is glibc-linked and carries its own libs — it mounts as a
    // separate RO squashfs aux drive, NOT baked into the rootfs. `/workspace`
    // (from busybox_rootfs) + `/opt/engram/dyn` (the bundle mount point) are
    // all the fixture needs.
    let outcome = common::bake_fixture_ext4(
        &images_dir_path.join("rootfs.ext4"),
        &chunk_store,
        busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |tree| {
            std::fs::create_dir_all(tree.join("opt/engram/dyn"))?;
            std::fs::create_dir_all(tree.join("workspace"))?;
            std::fs::write(tree.join("workspace/browser-e2e.html"), BROWSER_E2E_HTML)?;
            Ok(())
        },
    )
    .await;

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

#[derive(Clone, Copy, Debug)]
struct RfbPixelFormat {
    bits_per_pixel: u8,
    big_endian: bool,
    true_color: bool,
    red_max: u16,
    green_max: u16,
    blue_max: u16,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

impl RfbPixelFormat {
    fn parse(bytes: [u8; 16]) -> Self {
        Self {
            bits_per_pixel: bytes[0],
            big_endian: bytes[2] != 0,
            true_color: bytes[3] != 0,
            red_max: u16::from_be_bytes([bytes[4], bytes[5]]),
            green_max: u16::from_be_bytes([bytes[6], bytes[7]]),
            blue_max: u16::from_be_bytes([bytes[8], bytes[9]]),
            red_shift: bytes[10],
            green_shift: bytes[11],
            blue_shift: bytes[12],
        }
    }

    fn rgb(self, pixel: &[u8]) -> (u8, u8, u8) {
        assert!(
            self.true_color,
            "x11vnc must advertise a true-color pixel format"
        );
        let raw = match (self.bits_per_pixel, self.big_endian) {
            (32, false) => u32::from_le_bytes(pixel[..4].try_into().unwrap()),
            (32, true) => u32::from_be_bytes(pixel[..4].try_into().unwrap()),
            (16, false) => u16::from_le_bytes(pixel[..2].try_into().unwrap()) as u32,
            (16, true) => u16::from_be_bytes(pixel[..2].try_into().unwrap()) as u32,
            (bpp, _) => panic!("unsupported RFB bits-per-pixel: {bpp}"),
        };
        let channel = |shift: u8, max: u16| -> u8 {
            (((raw >> shift) & u32::from(max)) * 255 / u32::from(max)) as u8
        };
        (
            channel(self.red_shift, self.red_max),
            channel(self.green_shift, self.green_max),
            channel(self.blue_shift, self.blue_max),
        )
    }
}

/// Complete the no-auth RFB 3.8 handshake, request RAW encoding, and sample a
/// tiny rectangle at the framebuffer center. This proves Chrome painted the
/// page behind x11vnc; the protocol banner alone only proves x11vnc is alive.
async fn read_center_rgb(stream: &mut HarnessByteStream) -> (u8, u8, u8) {
    stream
        .write_all(b"RFB 003.008\n")
        .await
        .expect("write RFB client version");
    let security_count = stream.read_u8().await.expect("read security type count");
    assert!(security_count > 0, "RFB server offered no security types");
    let mut security_types = vec![0; usize::from(security_count)];
    stream
        .read_exact(&mut security_types)
        .await
        .expect("read RFB security types");
    assert!(
        security_types.contains(&1),
        "x11vnc did not offer None security: {security_types:?}"
    );
    stream.write_all(&[1]).await.expect("select None security");
    assert_eq!(
        stream.read_u32().await.expect("read SecurityResult"),
        0,
        "RFB None security rejected"
    );
    stream
        .write_all(&[1])
        .await
        .expect("write shared ClientInit");

    let width = stream.read_u16().await.expect("read framebuffer width");
    let height = stream.read_u16().await.expect("read framebuffer height");
    let mut pixel_format = [0; 16];
    stream
        .read_exact(&mut pixel_format)
        .await
        .expect("read server pixel format");
    let format = RfbPixelFormat::parse(pixel_format);
    let name_len = stream.read_u32().await.expect("read desktop name length");
    let mut name = vec![0; name_len as usize];
    stream
        .read_exact(&mut name)
        .await
        .expect("read desktop name");
    assert!(
        width >= 64 && height >= 64,
        "unexpected framebuffer {width}x{height}"
    );

    // SetEncodings: request only raw pixels (encoding 0).
    stream
        .write_all(&[2, 0, 0, 1, 0, 0, 0, 0])
        .await
        .expect("request raw RFB encoding");
    let rect_w = 32u16;
    let rect_h = 32u16;
    let x = width / 2 - rect_w / 2;
    let y = height / 2 - rect_h / 2;
    let mut request = vec![3, 0]; // FramebufferUpdateRequest, non-incremental
    request.extend_from_slice(&x.to_be_bytes());
    request.extend_from_slice(&y.to_be_bytes());
    request.extend_from_slice(&rect_w.to_be_bytes());
    request.extend_from_slice(&rect_h.to_be_bytes());
    stream
        .write_all(&request)
        .await
        .expect("request center framebuffer");

    // x11vnc can queue RAW updates (notably the 18x18 software cursor tile)
    // before it answers our explicit center request. The old decoder returned
    // the first pixel of the first queued rectangle and therefore graded the
    // top-left Chrome UI as though it were the page center. Read complete
    // server messages until a rectangle actually covers our probe point.
    //
    // Probe the upper-left corner of the centered 32x32 request rather than
    // its exact center: Xvfb starts the mouse cursor at screen center, and the
    // cursor's white pixels would otherwise obscure the fixture beneath it.
    let sample_x = x;
    let sample_y = y;
    let bytes_per_pixel = usize::from(format.bits_per_pixel / 8);
    loop {
        match stream
            .read_u8()
            .await
            .expect("read RFB server message type")
        {
            0 => {
                let _padding = stream.read_u8().await.expect("read framebuffer padding");
                let rectangles = stream.read_u16().await.expect("read rectangle count");
                assert!(rectangles > 0, "empty framebuffer update");
                for _ in 0..rectangles {
                    let rect_x = stream.read_u16().await.expect("read rect x");
                    let rect_y = stream.read_u16().await.expect("read rect y");
                    let w = stream.read_u16().await.expect("read rect width");
                    let h = stream.read_u16().await.expect("read rect height");
                    let encoding = stream.read_i32().await.expect("read rect encoding");
                    assert_eq!(encoding, 0, "server ignored requested raw encoding");
                    let mut pixels = vec![0; usize::from(w) * usize::from(h) * bytes_per_pixel];
                    stream
                        .read_exact(&mut pixels)
                        .await
                        .expect("read raw framebuffer pixels");

                    if sample_x >= rect_x
                        && sample_x < rect_x.saturating_add(w)
                        && sample_y >= rect_y
                        && sample_y < rect_y.saturating_add(h)
                    {
                        let pixel_x = usize::from(sample_x - rect_x);
                        let pixel_y = usize::from(sample_y - rect_y);
                        let offset = (pixel_y * usize::from(w) + pixel_x) * bytes_per_pixel;
                        return format.rgb(&pixels[offset..]);
                    }
                }
            }
            2 => {} // Bell has no payload.
            3 => {
                // ServerCutText may be emitted independently of framebuffer
                // updates. Consume it so the next byte is another message.
                let mut padding = [0u8; 3];
                stream
                    .read_exact(&mut padding)
                    .await
                    .expect("read ServerCutText padding");
                let len = stream.read_u32().await.expect("read ServerCutText length");
                let mut text = vec![0; len as usize];
                stream
                    .read_exact(&mut text)
                    .await
                    .expect("read ServerCutText payload");
            }
            message_type => panic!("unsupported RFB server message type: {message_type}"),
        }
    }
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
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
    cleanup_host_state();

    // ---- 1. Bake a busybox rootfs with agentd bundled (no browser baked in) ----
    let rootfs_path = bake_browser_rootfs(&busybox).await;

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
    // back over vsock. Banner-necessary-but-NOT-sufficient: chromium's own
    // liveness is deliberately not gated anywhere on this path —
    // `start_browser` only *warns* when CDP never answers (issue #569 chose
    // non-fatal so a slow cold start isn't misread as failure) — so a
    // crash-looping chrome behind a healthy x11vnc passes this test. That is
    // exactly how the Debian chromium 150.0.7871.46 startup-crash regression
    // (Debian bug #1141488, Jul 2026) reached prod with CI green; the guard
    // against a broken chrome is the version pin in
    // deploy/bundles/browser/build.sh, not this lane.
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

    // ---- 8. Drive the same Chrome, record it, and produce an annotation ----
    // Playwright CLI 0.1.17 rejects file:// navigation. Serve the fixture over
    // guest loopback instead, which also matches how production browser work
    // reaches pages. BusyBox httpd daemonizes only after successfully binding,
    // so the following navigation cannot race server startup.
    let driven = pooled
        .exec(
            sandbox_id,
            engram_core::types::sandbox::ExecRequest {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "/bin/busybox httpd -p 127.0.0.1:18080 -h /workspace && \
                     playwright-cli open http://127.0.0.1:18080/browser-e2e.html && \
                     playwright-cli video-start /workspace/browser-e2e.webm && \
                     playwright-cli video-chapter \"Fixture painted\" --duration 100 && \
                     playwright-cli video-stop && \
                     test -s /workspace/browser-e2e.webm && \
                     playwright-cli snapshot && \
                     playwright-cli highlight button --style 'outline: 4px solid cyan' && \
                     playwright-cli screenshot --filename /tmp/engram-browser-observations/browser-e2e.png && \
                     test -s /tmp/engram-browser-observations/browser-e2e.png"
                        .into(),
                ],
                stdin: None,
                env: HashMap::new(),
                workdir: Some("/workspace".into()),
                timeout: Some(Duration::from_secs(60)),
            },
        )
        .await
        .expect("drive shared Chrome with Playwright CLI");
    assert_eq!(
        driven.exit_status,
        Some(0),
        "Playwright CLI failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&driven.stdout),
        String::from_utf8_lossy(&driven.stderr),
    );
    assert!(
        String::from_utf8_lossy(&driven.stdout).contains("ENGRAM_BROWSER_PAINTED"),
        "semantic snapshot did not observe fixture: {:?}",
        String::from_utf8_lossy(&driven.stdout),
    );

    // ---- 9. Decode a real framebuffer region from that same painted page ----
    let (red, green, blue) = timeout(Duration::from_secs(15), read_center_rgb(&mut stream))
        .await
        .expect("raw RFB framebuffer within 15s");
    assert!(
        red > 180 && green < 100 && blue > 180,
        "center framebuffer pixel was not fixture magenta: rgb({red}, {green}, {blue})"
    );

    // ---- 10. Teardown ----
    drop(stream);
    pooled
        .stop_browser(sandbox_id)
        .await
        .expect("stop_browser (tear down the in-guest browser stack)");
    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}
