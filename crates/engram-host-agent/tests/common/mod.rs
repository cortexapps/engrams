//! Shared readiness-polling helpers for the engram-host-agent integration
//! tests. These replace fixed `tokio::time::sleep` "settle" windows with
//! bounded polling so the serial CI suite isn't paying a blind margin on
//! every synchronization point (and so a too-tight margin can't flake).
//!
//! Each top-level file in `tests/` is its own crate, so only the helpers a
//! given test binary actually calls are reachable from it — hence the
//! module-wide `dead_code` allow.
#![allow(dead_code)]

use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::image::OciRuntimeDefaults;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{inject_init, pack_tree, InitInjection};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Poll `TcpStream::connect(addr)` on a 20 ms tick until it succeeds (the
/// listener is bound and accepting) or `timeout` elapses. Returns whether
/// the address became connectable. Modeled on the in-crate precedent in
/// `grpc_pause_resume.rs::boot_grpc_server` (and `boot.rs`'s socket poll).
pub async fn wait_tcp_bound(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Generic bounded async poll: call `pred` every `interval` until it
/// resolves `true` or `timeout` elapses. Returns whether the predicate
/// became true in time. Intended for exec-based predicates (e.g. polling a
/// guest file via the test's `exec` helper) where each probe is itself
/// async.
pub async fn poll_until_async<F, Fut>(timeout: Duration, interval: Duration, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// FC test artifact discovery — duplicates the helper in
/// `engram-sandbox-firecracker/tests/common/` because Rust can't share
/// `tests/common/` across crates and adding a workspace-member test crate just
/// for this is heavier than a 20-line inline copy.
pub struct FcEnv {
    pub kernel: PathBuf,
    #[allow(dead_code)]
    pub rootfs: PathBuf,
}

/// Skip cleanly unless `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` are set and exist.
/// Prints a `SKIP:` line and returns `None` so callers can
/// `let env = match common::fc_preflight() { Some(e) => e, None => return };`.
pub fn fc_preflight() -> Option<FcEnv> {
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

/// Skip unless running as root (these tests need CAP_NET_ADMIN for the TAP +
/// iptables that `host_startup` / warm-restore netns set up). Prints a `SKIP:`
/// line and returns `false` otherwise.
pub fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: FC e2e tests require root (CAP_NET_ADMIN for TAP + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Wipe stale engram-* iptables rules + tap-engr-/vh-engr- interfaces left from
/// prior runs. Borrowed verbatim from proxy_e2e's helpers so a crash mid-test
/// doesn't leak host state into the next run.
pub fn cleanup_host_state() {
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

/// Wait up to `deadline` for `pooled.guest_endpoints(id)` to return Some.
/// The FC backend answers as soon as the in-VM agentd is reachable on
/// vsock; on a cold boot this typically takes a few seconds (kernel + init +
/// agentd). The poll cadence is fast (200ms) so the test isn't dominated by
/// sleep slack.
pub async fn wait_for_guest_endpoints(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    deadline: Duration,
) -> GuestEndpoints {
    let start = std::time::Instant::now();
    loop {
        if let Some(endpoints) = pooled.guest_endpoints(id).await {
            return endpoints;
        }
        assert!(
            start.elapsed() < deadline,
            "guest_endpoints never resolved within {deadline:?}",
        );
        sleep(Duration::from_millis(200)).await;
    }
}

/// ADR 0080: a staged agentd bundle fixture — the test-side mirror of the
/// host's `/var/lib/engram/shared` staging. Builds a squashfs carrying
/// `engram-agentd` + its `agentd.sha256` content stamp, stages it
/// content-addressed under `bundle_dir`, and writes a `current.json`
/// stamping both `agentd` and `sentinel` (the sentinel is a minimal
/// `mount.json` placeholder so symbolic slot resolution finds every key
/// it needs). Point `FirecrackerConfig.bundle_dir` at `bundle_dir` and
/// include [`agentd_slot`](Self::agentd_slot) (symbolic) in the spec's
/// `aux_ro_drives` — the backend resolves it exactly like prod.
pub struct StagedAgentdBundle {
    pub bundle_dir: PathBuf,
    /// sha256 of the staged agentd squashfs (the `current.json[agentd]`
    /// value / staged file name).
    pub squashfs_sha: String,
    /// Content stamp of the agentd binary itself (`agentd.sha256`
    /// inside the bundle — what RefreshAgent compares).
    pub binary_sha: String,
}

impl StagedAgentdBundle {
    /// The symbolic agentd reserved slot to include in a spec (the
    /// backend resolves it against the staged stamp).
    pub fn agentd_slot(&self) -> engram_core::types::sandbox::AuxRoDrive {
        engram_core::types::sandbox::AuxRoDrive::reserved_slot(
            engram_core::types::sandbox::AuxRoDrive::AGENTD_SLOT_INDEX,
        )
    }
}

/// Stage an agentd bundle + sentinel into `bundle_dir` (created if
/// missing). Requires `mksquashfs` — gate callers with
/// `require_bin("mksquashfs")`.
pub fn stage_agentd_bundle(bundle_dir: &Path, agentd_binary: &Path) -> StagedAgentdBundle {
    use engram_core::types::sandbox::AuxRoDrive;
    std::fs::create_dir_all(bundle_dir).expect("bundle_dir");

    let sha_hex = |bytes: &[u8]| -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let pack = |tree: &Path| -> Vec<u8> {
        let out = tempfile::tempdir().expect("squashfs out dir");
        let out_path = out.path().join("bundle.squashfs");
        // Mirrors the reproducible recipe in deploy/bundles/_pack.sh /
        // coordinator squashfs.rs.
        let status = std::process::Command::new("mksquashfs")
            .arg(tree)
            .arg(&out_path)
            .args(["-comp", "zstd", "-all-root", "-noappend", "-no-xattrs"])
            .env("SOURCE_DATE_EPOCH", "0")
            .status()
            .expect("spawn mksquashfs (is squashfs-tools installed?)");
        assert!(status.success(), "mksquashfs failed");
        std::fs::read(&out_path).expect("read squashfs")
    };
    let stage = |bytes: &[u8]| -> String {
        let sha = sha_hex(bytes);
        std::fs::write(bundle_dir.join(AuxRoDrive::staged_file_name(&sha)), bytes)
            .expect("stage squashfs");
        sha
    };

    // agentd bundle: the binary + its content stamp.
    let binary = std::fs::read(agentd_binary).expect("read agentd binary");
    let binary_sha = sha_hex(&binary);
    let tree = tempfile::tempdir().expect("agentd tree");
    std::fs::write(tree.path().join("engram-agentd"), &binary).expect("write agentd");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            tree.path().join("engram-agentd"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod agentd");
    }
    std::fs::write(tree.path().join("agentd.sha256"), format!("{binary_sha}\n"))
        .expect("write agentd stamp");
    let agentd_sha = stage(&pack(tree.path()));

    // Sentinel: the minimal placeholder every other reserved slot
    // resolves to at capture.
    let sentinel_tree = tempfile::tempdir().expect("sentinel tree");
    std::fs::write(
        sentinel_tree.path().join("mount.json"),
        "{\"kind\":\"sentinel\"}\n",
    )
    .expect("write sentinel mount.json");
    let sentinel_sha = stage(&pack(sentinel_tree.path()));

    let stamp = format!(
        "{{\"{}\":\"{agentd_sha}\",\"{}\":\"{sentinel_sha}\"}}\n",
        AuxRoDrive::AGENTD_STAMP_KEY,
        AuxRoDrive::SENTINEL_STAMP_KEY,
    );
    std::fs::write(bundle_dir.join(AuxRoDrive::CURRENT_STAMP), stamp).expect("write stamp");

    StagedAgentdBundle {
        bundle_dir: bundle_dir.to_path_buf(),
        squashfs_sha: agentd_sha,
        binary_sha,
    }
}

// ---------------------------------------------------------------------------
// ADR 0080 §D: docker-free fixture rootfs bake (mirror of the FC test crate's
// `common::bake_fixture_ext4`). Phase 4 retired the docker-based bake; the
// host-agent integration tests baked a minimal rootfs whose whole userland is
// `/bin/sh` + coreutils (agentd is static musl), so we build that tree from a
// static busybox and pack it with the same `Mke2fsPacker` + stage-1 init the
// enable-time materializer uses. Extra binaries (a COPY'd harness/claude, a
// gcc-built probe, `ttyd`) are laid on in the `customize` closure. NO docker.
// ---------------------------------------------------------------------------

/// Result of [`bake_fixture_ext4`] — the retired bake's `BuildOutcome`
/// shape the host-agent tests still consume.
pub struct BakedFixture {
    pub rootfs_path: PathBuf,
    pub disk_manifest: Option<ManifestRef>,
    pub runtime_defaults: OciRuntimeDefaults,
}

/// Static busybox from the runner — the fixture guest's whole userland. The
/// FC CI lane apt-installs `busybox-static` (or set `BUSYBOX_STATIC`).
pub fn find_busybox() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BUSYBOX_STATIC") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    ["/bin/busybox", "/usr/bin/busybox", "/sbin/busybox"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(Path::to_path_buf)
}

/// Lay a minimal FHS skeleton + a static busybox (an applet symlink for every
/// `busybox --list` applet) into `tree` — see the FC crate's copy for the
/// rationale.
pub fn busybox_rootfs(tree: &Path, busybox: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for dir in [
        "bin",
        "sbin",
        "dev",
        "etc",
        "opt",
        "proc",
        "root",
        "run",
        "sys",
        "tmp",
        "usr/bin",
        "usr/sbin",
        "usr/local/bin",
        "workspace",
    ] {
        std::fs::create_dir_all(tree.join(dir))?;
    }
    let bb = tree.join("bin/busybox");
    std::fs::copy(busybox, &bb)?;
    std::fs::set_permissions(&bb, std::fs::Permissions::from_mode(0o755))?;
    let list = std::process::Command::new(busybox).arg("--list").output()?;
    for applet in String::from_utf8_lossy(&list.stdout).split_whitespace() {
        if applet.is_empty() || applet.contains('/') {
            continue;
        }
        let link = tree.join("bin").join(applet);
        if !link.exists() {
            std::os::unix::fs::symlink("busybox", &link)?;
        }
    }
    Ok(())
}

/// Copy a dynamically-linked host tool into `tree` at `guest_rel` with its
/// `ldd` shared-object closure — the docker-free way to hand a fixture a real
/// tool without an apt layer. See the FC crate's copy for details.
pub fn copy_host_tool_with_closure(
    tree: &Path,
    tool: &Path,
    guest_rel: &str,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let place = |src: &Path, rel: &str| -> std::io::Result<()> {
        let dst = tree.join(rel.trim_start_matches('/'));
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, &dst)?;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    };
    place(tool, guest_rel)?;
    let ldd = std::process::Command::new("ldd").arg(tool).output()?;
    for line in String::from_utf8_lossy(&ldd.stdout).lines() {
        for token in line.split_whitespace() {
            if token.starts_with('/') {
                let src = Path::new(token);
                if src.is_file() {
                    place(src, token)?;
                }
            }
        }
    }
    Ok(())
}

/// ADR 0080 §D: bake a fixture ext4 rootfs WITHOUT docker (mirror of the FC
/// crate's helper). Gate callers with [`find_busybox`] (pure-Rust pack).
pub async fn bake_fixture_ext4(
    out_ext4: &Path,
    chunk_store: &ChunkStore,
    busybox: &Path,
    init: Option<InitInjection>,
    customize: impl FnOnce(&Path) -> std::io::Result<()>,
) -> BakedFixture {
    let tree = tempfile::tempdir().expect("fixture tree dir");
    busybox_rootfs(tree.path(), busybox).expect("busybox skeleton");
    customize(tree.path()).expect("fixture customize");
    if let Some(injection) = &init {
        inject_init(tree.path(), injection)
            .await
            .expect("inject stage-1 init");
    }
    if let Some(parent) = out_ext4.parent() {
        std::fs::create_dir_all(parent).expect("out_ext4 parent");
    }
    // ADR 0093: pure-Rust deterministic pack — no mke2fs, no gate.
    tokio::task::block_in_place(|| pack_tree(tree.path(), out_ext4)).expect("pack_tree");
    let manifest = chunk_store
        .chunk_file(out_ext4, ManifestKind::Disk, None)
        .await
        .expect("chunk fixture ext4");
    let manifest_ref = manifest.content_ref();
    if chunk_store.get_manifest(manifest_ref).await.is_err() {
        chunk_store
            .put_manifest(manifest_ref, &manifest)
            .await
            .expect("put fixture manifest");
    }
    BakedFixture {
        rootfs_path: out_ext4.to_path_buf(),
        disk_manifest: Some(manifest_ref),
        runtime_defaults: OciRuntimeDefaults::default(),
    }
}
