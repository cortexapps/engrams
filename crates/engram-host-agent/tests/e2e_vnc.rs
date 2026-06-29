//! True end-to-end VNC-tab test (ADR 0064) over a real Firecracker sandbox
//! with the `browser` RO bundle mounted.
//!
//! Until this test landed, the host-agent's `proxy_vnc` flow was only
//! validated at the subsystem level (`proxy_vnc::tests` against a fake
//! byte-echo TCP server; `pooled_backend::tests` for the `start_browser` /
//! forwarding wiring; `engram-agentd::browser::tests` against a fake python
//! launcher). None of those booted a real VM, mounted the real `browser`
//! bundle, and dialed the real x11vnc — the exact chain prod uses for the
//! BROWSER tab:
//!
//!   coord-side `proxy_vnc` →
//!     `PooledBackend.start_browser(id)` (vsock → in-VM agentd → engram-browser
//!       launcher → Xvfb + openbox + chromium + x11vnc) →
//!     `PooledBackend.vm_internal_ip(id)` + `PooledBackend.netns_name_for(id)` →
//!     `open_vnc_tunnel_at(...)` (cold = direct dial, warm = netns) →
//!     raw RFB byte round-trip with x11vnc.
//!
//! The assertion is the RFB ProtocolVersion handshake: x11vnc speaks FIRST in
//! RFB, sending the 12-byte `RFB 003.00x\n` banner the instant a viewer
//! connects (RFC 6143 §7.1.1). So the first inbound `ShellFrame::Binary` from
//! the tunnel must start with `RFB 003.` — proving the full chain ran: the
//! browser bundle activated, the launcher brought x11vnc up, the host dialed it
//! in the right netns, and `pump_tcp_through_tunnel` relayed the bytes back.
//!
//! Coverage: `vnc_cold` — cold-created FC sandbox, dial from host root. (The
//! warm/netns dial is exercised by `e2e_shell`'s warm case through the SAME
//! `connect_tcp_in_netns_linux` dialer `proxy_vnc` reuses; this test pins the
//! VNC-specific half — bundle → launcher → x11vnc → RFB banner.)
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

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec,
};
use engram_core::types::shell::{ShellFrame, ShellTunnel};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::time::timeout;

// Shared FC e2e harness floor (preflight / root-check / host-state cleanup /
// guest-IP poll) — the same module `e2e_shell` uses.
mod common;
use common::{cleanup_host_state, fc_preflight, require_root, wait_for_guest_ip};

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
            agent_injection: Some(AgentInjection {
                agent_binary: agent_bin,
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

/// Open the VNC tunnel via the same path `host_client.rs::proxy_vnc` uses,
/// against the `PooledBackend` that wraps the FC backend. Going through
/// `PooledBackend` is what catches the `start_browser` / `netns_name_for`
/// forwarding bugs (the ADR 0064 PooledBackend guards).
async fn open_vnc_tunnel_via_pooled(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
) -> ShellTunnel {
    // start_browser asks agentd to bring up Xvfb + chromium + x11vnc and probe
    // the VNC port before replying, so the dial below finds a listener.
    let port = pooled
        .start_browser(id)
        .await
        .expect("pooled.start_browser must succeed (browser bundle activated + x11vnc up)");
    // `vm_internal_ip` not `guest_ip` — same reason `proxy_vnc` does: x11vnc
    // binds the VM's in-VM eth0 IP inside the per-VM netns, not the netns veth.
    let guest_ip = pooled
        .vm_internal_ip(id)
        .await
        .expect("vm_internal_ip must resolve");
    let netns_name = pooled.netns_name_for(id).await;
    eprintln!("--- opening vnc tunnel: guest_ip={guest_ip} port={port} netns={netns_name:?} ---");

    let (tunnel, ends) = ShellTunnel::pair();
    engram_host_agent::proxy_vnc::open_vnc_tunnel_at(guest_ip, port, netns_name, ends)
        .await
        .expect("open_vnc_tunnel_at must succeed");
    tunnel
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
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    // The FC backend resolves aux drives at `<bundle_dir>/<sha>.squashfs`.
    cfg.bundle_dir = bundle_dir;
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
        aux_ro_drives: vec![browser_aux_drive(browser_sha)],
    };
    let sandbox_id = pooled.create(spec).await.expect("create");

    // ---- 4. Wait for in-VM agentd (guest_ip resolves once vsock answers) ----
    let _guest_ip = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30)).await;

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
                argv: Vec::new(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            },
        )
        .await
        .expect("start_agent (empty-argv readiness probe → activates browser bundle)");

    // ---- 6. Open the VNC tunnel via PooledBackend (proxy_vnc's path) ----
    let mut tunnel = open_vnc_tunnel_via_pooled(&pooled, sandbox_id).await;

    // ---- 7. Assert the RFB ProtocolVersion banner comes back ----
    // x11vnc speaks first in RFB: the 12-byte ProtocolVersion "RFB 003.00x\n".
    // 30s is generous — the browser cold start (Xvfb → openbox → chromium →
    // x11vnc) is heavier than ttyd, and start_browser already waited for
    // x11vnc to ACCEPT, so the banner should land within a tick of the dial.
    let first = timeout(Duration::from_secs(30), tunnel.inbound.recv())
        .await
        .expect("RFB ProtocolVersion banner within 30s of opening the VNC tunnel")
        .expect("vnc tunnel inbound channel closed before any frame");
    eprintln!("--- received first ShellFrame from x11vnc: {first:?} ---");
    match first {
        ShellFrame::Binary(b) => assert!(
            b.starts_with(b"RFB 003."),
            "expected RFB ProtocolVersion banner (`RFB 003.`); got {:?}",
            &b[..b.len().min(12)],
        ),
        other => panic!("expected RFB banner (Binary frame), got {other:?}"),
    }

    // ---- 8. Teardown ----
    let _ = tunnel.outbound.send(ShellFrame::Close(None)).await;
    drop(tunnel);
    pooled
        .stop_browser(sandbox_id)
        .await
        .expect("stop_browser (tear down the in-guest browser stack)");
    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}
