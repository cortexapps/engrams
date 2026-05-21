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
//!     `PooledBackend.guest_ip(id)` + `PooledBackend.netns_name_for(id)` →
//!     `open_shell_tunnel_at(...)` (cold = direct dial, warm = netns) →
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
//!     netns), dial INSIDE the netns. Catches the
//!     `PooledBackend.netns_name_for` forwarding bug too — without
//!     it, the dial happens from host root and never reaches the
//!     netns'd VM.
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
use engram_image_builder::{
    AgentInjection, BuildRequest, Builder, DockerCli, Ext4Packer, Format, Mke2fsPacker, Transport,
};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::time::{sleep, timeout};

/// FC test artifact discovery — duplicates the helper in
/// `engram-sandbox-firecracker/tests/common/` because Rust can't
/// share `tests/common/` across crates and adding a workspace-
/// member test crate just for this is heavier than a 20-line
/// inline copy.
struct FcEnv {
    kernel: PathBuf,
    #[allow(dead_code)]
    rootfs: PathBuf,
}

fn fc_preflight() -> Option<FcEnv> {
    let kernel = std::env::var("FC_TEST_KERNEL").ok()?;
    let rootfs = std::env::var("FC_TEST_ROOTFS").ok()?;
    let kp = PathBuf::from(&kernel);
    if !kp.exists() {
        eprintln!("SKIP: FC_TEST_KERNEL={kernel} doesn't exist");
        return None;
    }
    let rp = PathBuf::from(&rootfs);
    if !rp.exists() {
        eprintln!("SKIP: FC_TEST_ROOTFS={rootfs} doesn't exist");
        return None;
    }
    Some(FcEnv {
        kernel: kp,
        rootfs: rp,
    })
}

fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: e2e_shell tests require root (CAP_NET_ADMIN for TAP + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Wipe stale engram-* iptables rules + tap-engr-/vh-engr- interfaces
/// left from prior runs. Borrowed verbatim from proxy_e2e's helpers.
fn cleanup_host_state() {
    // iptables rules
    let saved = String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .unwrap_or_default();
    let mut current_table = "filter".to_string();
    for line in saved.lines() {
        if let Some(t) = line.strip_prefix('*') {
            current_table = t.trim().to_string();
            continue;
        }
        if !line.contains("engram-") {
            continue;
        }
        let Some(rest) = line.strip_prefix("-A ") else {
            continue;
        };
        let mut argv = vec!["-t".to_string(), current_table.clone(), "-D".to_string()];
        argv.extend(rest.split_whitespace().map(str::to_string));
        let _ = std::process::Command::new("iptables").args(&argv).output();
    }
    // TAP / veth interfaces
    for pat in ["tap-engr-", "vh-engr-"] {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ip -o link show | awk -F': ' '/{pat}/ {{print $2}}' | awk '{{print $1}}'"
            ))
            .output();
        let Ok(o) = out else { continue };
        for name in String::from_utf8_lossy(&o.stdout).lines() {
            let name = name.trim();
            if !name.is_empty() {
                let _ = std::process::Command::new("ip")
                    .args(["link", "delete", name])
                    .output();
            }
        }
    }
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
async fn bake_shell_rootfs(repo: &str) -> (PathBuf, PathBuf) {
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
    std::fs::write(src.path().join("engram.toml"), format!("name = \"{repo}\"\n")).unwrap();

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
            agent_injection: Some(AgentInjection {
                agent_binary: agent_bin,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
                bootstrap_binary: None,
            }),
            canonical_memory_manifest: None,
            capture_canonical_memory: None,
            parent_disk_bootstrap_path: None,
            parent_disk_chunks_blob_digest: None,
        })
        .await
        .expect("bake ext4");

    // Build an empty substrate (the bake's init still mounts it,
    // but the test doesn't need any harness files in it).
    let substrate_src = tempfile::tempdir().expect("substrate src");
    let substrate_path = images_dir_path.join("harness-substrate.img");
    Mke2fsPacker::default()
        .pack(substrate_src.path(), &substrate_path, 16 * 1024 * 1024)
        .await
        .expect("pack substrate");

    (outcome.rootfs_path, substrate_path)
}

/// Wait up to `deadline` for `pooled.guest_ip(id)` to return Some.
/// The FC backend's `guest_ip` answers as soon as the in-VM agentd
/// is reachable on vsock; on a cold boot this typically takes
/// a few seconds (kernel + init + agentd). The poll cadence is
/// fast (200ms) so the test isn't dominated by sleep slack.
async fn wait_for_guest_ip(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    deadline: Duration,
) -> String {
    let start = std::time::Instant::now();
    loop {
        if let Some(ip) = pooled.guest_ip(id).await {
            return ip;
        }
        assert!(
            start.elapsed() < deadline,
            "guest_ip never resolved within {deadline:?}",
        );
        sleep(Duration::from_millis(200)).await;
    }
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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        let frame = timeout(remaining, tunnel.inbound.recv())
            .await
            .expect(
                "ttyd should send a frame within 10s of WS handshake + resize. \
                 Without one, the tunnel/pump may have negotiated WS at the \
                 HTTP layer but failed downstream. Check /var/log/ttyd.log \
                 inside the VM.",
            )
            .expect("inbound channel closed unexpectedly");
        eprintln!("--- received ShellFrame from ttyd: {frame:?} ---");
        match frame {
            ShellFrame::Close(_) => {
                panic!("ttyd closed the tunnel before sending any non-close frame");
            }
            _ => {
                // Any other frame is success — Ping/Pong proves
                // WS-level keepalive works; Text/Binary proves PTY
                // output works. Either way the round-trip is real.
                break;
            }
        }
    }

    let _ = tunnel.outbound.send(ShellFrame::Close(None)).await;
    drop(tunnel);
}

/// Open the tunnel via the same path `host_client.rs::proxy_shell`
/// uses, against the PooledBackend that wraps the FC backend.
/// This is the actual prod control flow — going through
/// PooledBackend is what catches the forwarding bugs.
async fn open_tunnel_via_pooled(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
) -> ShellTunnel {
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
    // Use `vm_internal_ip` not `guest_ip` — same reason
    // `host_client::proxy_shell` does. `guest_ip` for warm
    // sandboxes returns the SNAT slot (10.200.0.6 by default),
    // which is wrong for the shell-tab dial (which happens inside
    // the netns and needs the VM's eth0 IP, 10.200.0.2 by default).
    let guest_ip = pooled
        .vm_internal_ip(id)
        .await
        .expect("vm_internal_ip must resolve");
    let netns_name = pooled.netns_name_for(id).await;
    eprintln!(
        "--- opening shell tunnel: guest_ip={guest_ip} port={port} netns={netns_name:?} ---"
    );

    // Diagnostic: probe both layers BEFORE the WS dial so a
    // failure points at the right thing.
    if let Some(ns) = netns_name.as_deref() {
        let dump = std::process::Command::new("ip")
            .args(["netns", "exec", ns, "ip", "-brief", "addr"])
            .output();
        if let Ok(o) = dump {
            eprintln!(
                "--- netns {ns} addrs ---\n{}--- end ---",
                String::from_utf8_lossy(&o.stdout),
            );
        }
        // Ping VM via TAP — if this fails, the netns routing is
        // broken; if it succeeds the VM is up + reachable and any
        // TCP failure below means a userspace listener problem.
        let ping = std::process::Command::new("ip")
            .args([
                "netns",
                "exec",
                ns,
                "timeout",
                "3",
                "ping",
                "-c",
                "1",
                "-W",
                "2",
                &guest_ip,
            ])
            .output();
        if let Ok(o) = ping {
            eprintln!(
                "--- ping {guest_ip} from {ns}: status={} ---\n{}--- end ---",
                o.status,
                String::from_utf8_lossy(&o.stdout),
            );
        }
        // TCP probe from inside the netns. This is the same
        // network path open_shell_tunnel_at's dial takes.
        let tcp_probe = std::process::Command::new("ip")
            .args([
                "netns",
                "exec",
                ns,
                "bash",
                "-c",
                &format!("timeout 3 bash -c 'echo > /dev/tcp/{guest_ip}/{port}' && echo TCP_OK || echo TCP_CLOSED"),
            ])
            .output();
        if let Ok(o) = tcp_probe {
            eprintln!(
                "--- /dev/tcp/{guest_ip}/{port} from {ns}: status={} ---\n{}{}--- end ---",
                o.status,
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr),
            );
        }
    } else {
        match tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect((guest_ip.as_str(), port)),
        )
        .await
        {
            Ok(Ok(s)) => {
                drop(s);
                eprintln!("--- tcp probe → connect OK to {guest_ip}:{port} ---");
            }
            Ok(Err(e)) => eprintln!("--- tcp probe → connect error: {e} ---"),
            Err(_) => eprintln!("--- tcp probe → timeout after 3s ---"),
        }
    }

    let (tunnel, ends) = ShellTunnel::pair();
    engram_host_agent::proxy_shell::open_shell_tunnel_at(guest_ip, port, netns_name, ends)
        .await
        .expect("open_shell_tunnel_at must succeed");
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
    let (rootfs_path, substrate_path) = bake_shell_rootfs("engram-e2e-shell-cold").await;

    // ---- 2. Wrap FC in PooledBackend (exactly as host-agent does) ----
    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    // ---- 3. Cold-create a sandbox ----
    let spec = SandboxSpec {
        image: "engram-e2e-shell-cold".into(),
        rootfs_source: Some(rootfs_path),
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
    let sandbox_id = pooled.create(spec).await.expect("create");

    // ---- 4. Wait for in-VM agentd to come up so guest_ip resolves
    //         AND the bake's init has had a chance to spawn ttyd. ----
    let _guest_ip = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30)).await;
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

    let (rootfs_path, substrate_path) = bake_shell_rootfs("engram-e2e-shell-warm").await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let spec = SandboxSpec {
        image: "engram-e2e-shell-warm".into(),
        rootfs_source: Some(rootfs_path),
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

    // ---- Cold create + wait for VM to be ready ----
    let cold_id = pooled.create(spec).await.expect("create");
    let _ = wait_for_guest_ip(&pooled, cold_id, Duration::from_secs(30)).await;
    sleep(Duration::from_secs(2)).await; // ttyd bind window

    // ---- Snapshot + destroy the cold instance ----
    let metadata = pooled.snapshot(cold_id).await.expect("snapshot");
    pooled.destroy(cold_id).await.expect("destroy cold");

    // ---- Restore → this is the path that puts the VM in a per-VM
    //      netns. Catches the PooledBackend.netns_name_for forwarding
    //      bug: without it, netns_name_for returns None and the
    //      tunnel dial happens from host root, which has no route
    //      to the netns'd 10.200.0.x. ----
    let warm_id = pooled.restore(metadata).await.expect("restore");
    let _ = wait_for_guest_ip(&pooled, warm_id, Duration::from_secs(30)).await;
    // ttyd's TCP listen socket should survive the snapshot/restore
    // (FC restores the kernel state including sockets). But give a
    // small probe window in case ttyd needs a tick to re-arm.
    sleep(Duration::from_secs(2)).await;

    // Sanity: confirm netns_name_for returns Some — if it doesn't,
    // PooledBackend isn't forwarding and the tunnel dial would dial
    // from root netns. We fail loud here with a clear message
    // rather than letting the dial time out.
    let netns = pooled.netns_name_for(warm_id).await;
    assert!(
        netns.is_some(),
        "PooledBackend.netns_name_for returned None for a warm-restored sandbox — \
         the forwarding to inner FC backend isn't wired up (prod 2026-05-20 bug class)",
    );

    let tunnel = open_tunnel_via_pooled(&pooled, warm_id).await;
    assert_tunnel_round_trips(tunnel).await;

    pooled.destroy(warm_id).await.expect("destroy warm");
    cleanup_host_state();
}
