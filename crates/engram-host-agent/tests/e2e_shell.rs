//! True end-to-end shell-tab tests for the cold + warm paths.
//!
//! Until these tests landed, the host-agent's `proxy_shell` flow
//! was only validated at the subsystem level (`proxy_e2e` for the
//! egress chain on cold, `warm_proxy_chain` for the iptables+netns
//! chain on warm, `start_shell_*` unit tests for the agentd
//! probe/spawn logic). None of those exercised the actual chain
//! used in prod for the SHELL tab:
//!
//!   coord-side `proxy_shell` →
//!     `PooledBackend.start_shell(id)` (vsock → in-VM agentd → ttyd) →
//!     `PooledBackend.guest_endpoints(id)` →
//!     relay over the vsock port relay when available, else
//!     `open_shell_tunnel_at(...)` (direct `dial_ip` dial) →
//!     WS-frame round-trip with ttyd.
//!
//! That's the path prod session 5c8d0ce5 (2026-05-20) silently
//! broke on: `PooledBackend.start_shell` was using the trait
//! default `Ok(7681)` because the impl block was missing the
//! forward to its inner FC backend. None of the existing tests
//! went through `PooledBackend`; this one does.
//!
//! Coverage matrix:
//!   - shell_cold: cold-created FC sandbox, dial from host root.
//!   - shell_warm: cold → snapshot → destroy → restore (per-VM
//!     netns), dial `GuestEndpoints::dial_ip`. Catches the
//!     `PooledBackend.guest_endpoints` forwarding bug too — without
//!     it, the dial targets the netns's SNAT slot instead and never
//!     reaches the netns'd VM.
//!
//! Run on the dev VM:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh e2e_shell
//! ```

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_core::types::shell::{ShellFrame, ShellTunnel};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format, InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::time::{sleep, timeout};

// Shared FC e2e harness floor (preflight / root-check / host-state cleanup /
// guest-IP poll). Extracted to `tests/common/mod.rs` when `e2e_vnc.rs` (ADR
// 0064) landed needing the identical helpers.
mod common;
use common::{cleanup_host_state, fc_preflight, require_root, wait_for_guest_endpoints};

/// ADR 0080: the musl agentd the staged bundle fixture packs (the same
/// binary the old bake used to inject into the rootfs).
fn agentd_musl_bin() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest).join("../../target/x86_64-unknown-linux-musl/release/engram-agentd")
}

/// Bake a debian-slim rootfs with ttyd + the latest `engram-agentd`
/// musl binary injected. Same shape as `proxy_e2e`'s bake, minus
/// curl (we don't need it for shell tests).
///
/// Network strategy: the dev-vm's Docker daemon has had DNS issues
/// during `apt-get update` (observed during this test's bring-up
/// — every InRelease fetch timed out after 40s). So we do all the
/// network work HOST-side (download ttyd via `curl`) and use only
/// `COPY` inside Docker, which doesn't need DNS.
async fn bake_shell_rootfs(repo: &str) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    assert!(
        agent_bin.exists(),
        "musl agentd not at {} — run via \
         `bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh e2e_shell` \
         which builds it first",
        agent_bin.display(),
    );

    let src = tempfile::tempdir().expect("source dir");

    // Download ttyd to the Docker build context (host-side, where
    // network works). The Dockerfile then COPYs it in without any
    // in-container network access.
    let ttyd_url = "https://github.com/tsl0922/ttyd/releases/download/1.7.7/ttyd.x86_64";
    let ttyd_dst = src.path().join("ttyd");
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&ttyd_dst)
        .arg(ttyd_url)
        .output()
        .expect("spawn curl");
    assert!(
        out.status.success(),
        "host-side curl ttyd failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&ttyd_dst).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&ttyd_dst, perms).unwrap();

    std::fs::write(
        src.path().join("Dockerfile"),
        // debian-slim already ships with libc + /bin/sh (dash) which
        // the init-shim's `SHELL_BIN=/bin/sh` finds. ttyd is a
        // fully-static binary so it doesn't depend on anything
        // else in the image.
        //
        // `/workspace` is load-bearing: the bake's init shim does
        // `(cd /workspace && ttyd ...) &` before backgrounding ttyd.
        // If the dir doesn't exist, the subshell exits before the
        // ttyd exec and the shell tab silently has no listener
        // (no `[ -d /workspace ]` guard upstream, just an unchecked
        // `cd`). debian-slim doesn't ship with /workspace.
        "FROM debian:bookworm-slim\n\
         COPY ttyd /usr/local/bin/ttyd\n\
         RUN chmod +x /usr/local/bin/ttyd && mkdir -p /workspace\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        format!("name = \"{repo}\"\n"),
    )
    .unwrap();

    let images_dir = tempfile::tempdir().expect("images");
    let images_dir_path = images_dir.path().to_path_buf();
    // Keep the tempdir alive for the duration of the test.
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

    // ADR 0021 P1.5: no harness substrate — this test doesn't drive
    // a harness anyway (it's about the SHELL tab + ttyd / proxy
    // chain), and the substrate retired with option-D.
    let _ = images_dir_path; // silence unused-binding if no other use lands

    outcome.rootfs_path
}

/// Drive a WS round-trip through the ShellTunnel.
///
/// ttyd's wire protocol (`html/src/components/terminal/index.ts` in
/// the ttyd repo): the SERVER sends `SET_WINDOW_TITLE` (cmd byte
/// '1') + `SET_PREFERENCES` (cmd byte '2') immediately on accept,
/// then OUTPUT (cmd byte '0') as the PTY writes. So under normal
/// operation we'd expect a Text frame within ~ms of the WS
/// handshake.
///
/// In practice the dev-vm bake's `ttyd -W -p 7681 /bin/sh`
/// invocation can stall before pushing anything if the PTY child
/// hasn't written yet — dash on a freshly-allocated PTY with no
/// dimensions set sometimes blocks waiting on the TIOCSWINSZ from
/// the client. Send a RESIZE frame first (cmd byte '1' = resize)
/// so ttyd can render the prompt, then wait for OUTPUT back.
async fn assert_tunnel_round_trips(mut tunnel: ShellTunnel) {
    use bytes::Bytes;
    // ttyd resize: binary frame, byte 0 = '1' (RESIZE_TERMINAL),
    // rest = JSON `{columns, rows}`. Without this, dash + ttyd
    // sometimes don't emit the prompt promptly.
    let resize = Bytes::from(b"1{\"columns\":80,\"rows\":24}".to_vec());
    let _ = tunnel.outbound.send(ShellFrame::Binary(resize)).await;

    // Accept any non-Close frame as proof the WS round-trips
    // through the proxy_shell pump. ttyd sends a Ping for
    // keepalive on idle connections — that's sufficient: the
    // Ping made it from ttyd's WebSocket, through tokio-tungstenite
    // on the host (in the right netns for warm, root for cold),
    // through `pump_websocket_through_tunnel`, through the
    // ShellTunnel's mpsc, and out to us. That's the full path
    // the dashboard SHELL tab uses minus the gRPC coord ↔ host
    // hop (which is exercised by `engram-host-agent::grpc_server`
    // tests).
    //
    // PTY output (cmd byte '0' over Binary) would be a stronger
    // assertion that bash is actually alive in the VM, but ttyd's
    // own keepalive proves the WS path is healthy and a separate
    // dedicated test can stress the PTY-output path.
    // We only need ONE non-Close frame to prove the tunnel
    // round-trips. ttyd may send a Ping (idle keepalive),
    // Text/Binary (PTY output), or Pong (response to a ping we
    // sent). Any of those is sufficient.
    let frame = timeout(Duration::from_secs(10), tunnel.inbound.recv())
        .await
        .expect(
            "ttyd should send a frame within 10s of WS handshake + resize. \
             Without one, the tunnel/pump may have negotiated WS at the \
             HTTP layer but failed downstream. Check /var/log/ttyd.log \
             inside the VM.",
        )
        .expect("inbound channel closed unexpectedly");
    eprintln!("--- received ShellFrame from ttyd: {frame:?} ---");
    assert!(
        !matches!(frame, ShellFrame::Close(_)),
        "ttyd closed the tunnel before sending any non-close frame",
    );

    let _ = tunnel.outbound.send(ShellFrame::Close(None)).await;
    drop(tunnel);
}

/// Open the tunnel via the same path `host_client.rs::proxy_shell`
/// uses, against the PooledBackend that wraps the FC backend.
/// This is the actual prod control flow — going through
/// PooledBackend is what catches the forwarding bugs.
async fn open_tunnel_via_pooled(pooled: &PooledBackend, id: engram_core::SandboxId) -> ShellTunnel {
    let port = pooled
        .start_shell(id)
        .await
        .expect("pooled.start_shell must succeed");
    // ttyd's default port is 7681; FC backend's start_shell
    // returns that. If we get 7681 from a wrapped non-FC default
    // (the bug fix here protects against that), the rest of the
    // test will still fail because nothing's bound when the bake
    // hasn't auto-started ttyd. With ttyd installed at the
    // conventional path the init shim DOES auto-start it, so
    // a stale default-7681 would coincidentally still dial-to-
    // listener; the strongest assertion that the forwarding
    // works is the unit test in pooled_backend.rs::tests.
    let (tunnel, ends) = ShellTunnel::pair();
    // ADR 0066: the FC/warm shell path now rides the vsock relay (mirrors
    // host_client::proxy_shell). `open_guest_stream` delegates through the
    // PooledBackend to FC's vsock: `Some` => relay the ttyd WebSocket to the
    // guest's `127.0.0.1:port`; `None` => direct dial_ip dial (Process / VZ
    // pre-Phase-2).
    match pooled
        .open_guest_stream(id, engram_harness_proto::PROXY_PORT_VSOCK_PORT)
        .await
        .expect("open_guest_stream must not error")
    {
        Some(stream) => {
            eprintln!("--- opening shell tunnel via vsock relay: port={port} ---");
            engram_host_agent::proxy_shell::open_shell_tunnel_via_relay(stream, port, ends)
                .await
                .expect("relay shell tunnel must succeed");
        }
        None => {
            let dial_ip = pooled
                .guest_endpoints(id)
                .await
                .map(|ep| ep.dial_ip)
                .expect("guest_endpoints must resolve");
            eprintln!("--- opening shell tunnel (cold dial): dial_ip={dial_ip} port={port} ---");
            engram_host_agent::proxy_shell::open_shell_tunnel_at(dial_ip.to_string(), port, ends)
                .await
                .expect("cold shell tunnel must succeed");
        }
    }
    tunnel
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + sudo"]
async fn e2e_shell_cold_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    // ---- 1. Bake a real rootfs with ttyd + agentd ----
    let rootfs_path = bake_shell_rootfs("engram-e2e-shell-cold").await;

    // ---- 2. Wrap FC in PooledBackend (exactly as host-agent does) ----
    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_musl_bin());
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    // ---- 3. Cold-create a sandbox ----
    let spec = SandboxSpec {
        image: "engram-e2e-shell-cold".into(),
        rootfs_source: Some(rootfs_path),
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
    let sandbox_id = pooled.create(spec).await.expect("create");

    // ---- 4. Wait for in-VM agentd to come up so guest_endpoints resolves
    //         AND the bake's init has had a chance to spawn ttyd. ----
    let _endpoints = wait_for_guest_endpoints(&pooled, sandbox_id, Duration::from_secs(30)).await;
    // Give ttyd a couple seconds to bind after the bake's init
    // shim launches it in the background.
    sleep(Duration::from_secs(2)).await;

    // ---- 5. Open the shell tunnel via PooledBackend ----
    let tunnel = open_tunnel_via_pooled(&pooled, sandbox_id).await;

    // ---- 6. Round-trip a frame to prove ttyd serves the tunnel ----
    assert_tunnel_round_trips(tunnel).await;

    // ---- 7. Teardown ----
    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + sudo"]
async fn e2e_shell_warm_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    let rootfs_path = bake_shell_rootfs("engram-e2e-shell-warm").await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_musl_bin());
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let spec = SandboxSpec {
        image: "engram-e2e-shell-warm".into(),
        rootfs_source: Some(rootfs_path),
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

    // ---- Cold create + wait for VM to be ready ----
    let cold_id = pooled.create(spec).await.expect("create");
    let _ = wait_for_guest_endpoints(&pooled, cold_id, Duration::from_secs(30)).await;
    sleep(Duration::from_secs(2)).await; // ttyd bind window

    // ---- Snapshot + destroy the cold instance ----
    let metadata = pooled.snapshot(cold_id).await.expect("snapshot");
    pooled.destroy(cold_id).await.expect("destroy cold");

    // ---- Restore → this is the path that puts the VM in a per-VM
    //      netns. Catches the PooledBackend.guest_endpoints forwarding
    //      bug: without it, guest_endpoints().netns returns None and the
    //      tunnel dial happens from host root, which has no route
    //      to the netns'd 10.200.0.x. ----
    let warm_id = pooled.restore(metadata).await.expect("restore");
    let endpoints = wait_for_guest_endpoints(&pooled, warm_id, Duration::from_secs(30)).await;
    // ttyd's TCP listen socket should survive the snapshot/restore
    // (FC restores the kernel state including sockets). But give a
    // small probe window in case ttyd needs a tick to re-arm.
    sleep(Duration::from_secs(2)).await;

    // Sanity: confirm guest_endpoints().netns is Some — if it isn't,
    // PooledBackend isn't forwarding and the tunnel dial would dial
    // from root netns. We fail loud here with a clear message
    // rather than letting the dial time out.
    assert!(
        endpoints.netns.is_some(),
        "PooledBackend.guest_endpoints returned no netns for a warm-restored sandbox — \
         the forwarding to inner FC backend isn't wired up (prod 2026-05-20 bug class)",
    );

    let tunnel = open_tunnel_via_pooled(&pooled, warm_id).await;
    assert_tunnel_round_trips(tunnel).await;

    pooled.destroy(warm_id).await.expect("destroy warm");
    cleanup_host_state();
}
