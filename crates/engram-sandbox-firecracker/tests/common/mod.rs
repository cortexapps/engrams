//! Helpers shared by every FC integration test. Lives at
//! `tests/common/mod.rs` (cargo's standard pattern for shared
//! test-only code) so each `tests/<name>.rs` brings it in via
//! `mod common;`.
//!
//! Five tests live in this directory and they all need the same four
//! things: skip cleanly when the host can't run Firecracker, find a
//! binary on `$PATH`, drain an `ExecStream` into stdout/stderr/exit,
//! and resolve the cached test artifacts. Fifth duplicate prompted
//! the extraction.

#![allow(dead_code)] // Each test only uses a subset of helpers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
use engram_core::types::image::OciRuntimeDefaults;
use engram_core::types::sandbox::ExecEvent;
use engram_rootfs_materializer::{inject_init, pack_tree, InitInjection};
use futures::StreamExt;

/// Successful preflight: paths to the cached vmlinux + ext4 rootfs.
pub struct FcEnv {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
}

/// Verify the host can drive Firecracker for an `#[ignore]`'d test:
/// `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` env vars set, `/dev/kvm`
/// present, `firecracker` on `$PATH`. Prints a `SKIP:` line and
/// returns `None` on the first missing prereq so the test can
/// `let env = match common::fc_preflight() { Some(e) => e, None => return };`
/// without growing per-test boilerplate.
///
/// Tests that need additional binaries (e.g. `docker`, `mksquashfs`)
/// follow up with [`require_bin`].
pub fn fc_preflight() -> Option<FcEnv> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    if which("firecracker").is_none() {
        eprintln!("SKIP: firecracker binary not on PATH");
        return None;
    }
    Some(FcEnv { kernel, rootfs })
}

/// Successful compat preflight: the cached artifacts plus the two
/// Firecracker binaries (stock + fork) to round-trip a snapshot between.
pub struct CompatEnv {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub stock_bin: PathBuf,
    pub fork_bin: PathBuf,
}

/// Like [`fc_preflight`] but for the stock↔fork snapshot-compat test
/// (ADR 0045 Phase B): needs `FC_TEST_KERNEL` / `FC_TEST_ROOTFS` +
/// `/dev/kvm`, plus BOTH `ENGRAM_FC_STOCK_BIN` and `ENGRAM_FC_FORK_BIN`
/// pointing at the two binaries to interoperate. No `firecracker`-on-`$PATH`
/// requirement: the test sets `FirecrackerConfig::firecracker_bin` per
/// backend. Prints `SKIP:` and returns `None` on the first missing prereq.
pub fn compat_preflight() -> Option<CompatEnv> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set; run scripts/fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
    let stock_bin = match std::env::var("ENGRAM_FC_STOCK_BIN") {
        Ok(p) if Path::new(&p).is_file() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_STOCK_BIN not set to an existing stock firecracker binary");
            return None;
        }
    };
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if Path::new(&p).is_file() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set to an existing forked firecracker binary");
            return None;
        }
    };
    Some(CompatEnv {
        kernel,
        rootfs,
        stock_bin,
        fork_bin,
    })
}

/// Returns `false` (after printing `SKIP:`) if `bin` isn't on `$PATH`.
/// Used by tests with extra binary requirements (docker, mksquashfs).
pub fn require_bin(bin: &str) -> bool {
    if which(bin).is_none() {
        eprintln!("SKIP: {bin} not on PATH");
        return false;
    }
    true
}

/// Drain an `ExecStream`'s events into separated stdout/stderr buffers
/// plus the terminal exit code. Stops at the first `Exit` event.
pub async fn drain(
    mut stream: impl StreamExt<Item = ExecEvent> + Unpin,
) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some(ev) = stream.next().await {
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(code) => {
                exit_code = code;
                break;
            }
        }
    }
    (stdout, stderr, exit_code)
}

/// Tiny `which`: walk `$PATH`, return the first match. Avoids pulling
/// the `which` crate as a dev-dep.
pub fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|p| Path::new(p).join(bin))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Readiness polling.
//
// These tests boot real microVMs, but a microVM boots in ~0.3s and a
// snapshot/restore round-trips in ~1s. The slow way to wait for that work is
// a fixed `sleep` sized for the worst case; the fast way is to poll the
// observable the test already cares about and return the instant it holds.
// `boot.rs` proves the pattern (it polls the API socket and finishes in
// 0.34s). The helpers below generalize it so the rest of the suite can stop
// sleeping. Bound the worst case with a generous ceiling; pay only the real
// latency in the common case.
//
// NB: most of these guests boot the public ubuntu rootfs to `init=/bin/bash`
// with no in-guest agentd, so `wait_agent_ready` is not available — the
// host-observable signals are the FC API socket, the serial console (funneled
// to a log file), and `/proc` gauges.
// ---------------------------------------------------------------------------

/// Poll a synchronous predicate every `interval` until it returns `true`, or
/// `timeout` elapses. Returns whether the condition was met. The drop-in
/// replacement for a fixed `sleep` whose purpose was "wait for X to become
/// true" where X is a cheap fs / `/proc` / in-memory check.
pub async fn poll_until(
    timeout: Duration,
    interval: Duration,
    mut pred: impl FnMut() -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// [`poll_until`] for an async predicate — e.g. a `backend.list()`-shaped
/// condition that has to `.await`.
pub async fn poll_until_async<F, Fut>(timeout: Duration, interval: Duration, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Poll until `path` (a Firecracker API socket) is ACCEPTING connections, or
/// `timeout` elapses. Promoted from `boot.rs` so the raw-`spawn_firecracker`
/// tests can drop their fixed post-spawn sleeps. 50ms ticks.
///
/// A real `connect()`, not `path.exists()`: the socket file appears at
/// `bind()`, before `listen()` is accepting, so an existence poll can pass in
/// the bind→listen window and the first API request then dies with
/// ECONNREFUSED — exactly the `put_machine_config: Connection refused` flake
/// that hit `aux_ro_drive_content_swap_under_snapshot_is_the_incident` on the
/// suite-startup thundering herd (CI run 28974774202). Connect-polling also
/// keeps retrying through an FC that bound and crashed, converting a
/// first-request panic into this helper's own bounded timeout with a
/// caller-visible `assert!` message.
pub async fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    poll_until(timeout, Duration::from_millis(50), || {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    })
    .await
}

/// Poll `path` until its contents contain ANY of `needles`, or `timeout`
/// elapses. Returns the final contents (a missing file reads as empty) so the
/// caller can build a useful assertion message on a miss.
///
/// Firecracker funnels the guest serial console (`console=ttyS0`) plus its own
/// stderr into one log file — `firecracker.log` under the jail dir for
/// backend-managed VMs, or the file the raw `spawn_firecracker` tests redirect
/// into — so a kernel- or guest-emitted marker is observable here without an
/// in-guest agent.
pub async fn wait_for_log_contains(path: &Path, needles: &[&str], timeout: Duration) -> String {
    let mut last = String::new();
    let met = poll_until(timeout, Duration::from_millis(100), || {
        last = std::fs::read_to_string(path).unwrap_or_default();
        needles.iter().any(|n| last.contains(n))
    })
    .await;
    let _ = met;
    last
}

/// Like [`wait_for_log_contains`], but waits until `needle` appears at least
/// `count` times — the post-resume read-loop assertions count distinct marker
/// lines. Returns the final contents.
pub async fn wait_for_log_count(
    path: &Path,
    needle: &str,
    count: usize,
    timeout: Duration,
) -> String {
    let mut last = String::new();
    let met = poll_until(timeout, Duration::from_millis(100), || {
        last = std::fs::read_to_string(path).unwrap_or_default();
        last.matches(needle).count() >= count
    })
    .await;
    let _ = met;
    last
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
// ADR 0080 §D: docker-free fixture rootfs bake.
//
// Phase 4 retired the docker-based image bake (docker build + export + pack).
// The FC integration tests only ever baked a minimal rootfs whose whole
// userland is `/bin/sh` + coreutils (agentd is static musl, so the guest
// needs no glibc), so we build that tree directly from a static busybox and
// pack it with the SAME `Mke2fsPacker` + stage-1 init the enable-time
// materializer uses. Tests that needed extra binaries (`socat`, `curl`, a
// gcc-built probe, a COPY'd harness) lay them onto the tree in the
// `customize` closure — [`copy_host_tool_with_closure`] copies a dynamically
// linked host tool + its `ldd` closure so the property each test asserts is
// unchanged. NO docker anywhere.
// ---------------------------------------------------------------------------

/// Result of [`bake_fixture_ext4`] — the retired bake's `BuildOutcome`
/// shape the FC tests still consume (`rootfs_path` + the chunked
/// `disk_manifest`).
pub struct BakedFixture {
    pub rootfs_path: PathBuf,
    pub disk_manifest: Option<ManifestRef>,
    pub runtime_defaults: OciRuntimeDefaults,
}

/// Static busybox from the runner — the fixture guest's whole userland. The
/// FC CI lane apt-installs `busybox-static`; dev boxes usually have one of the
/// standard paths (or set `BUSYBOX_STATIC`). Returns `None` (callers
/// `SKIP:`) when absent, mirroring [`require_bin`].
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

/// Lay a minimal FHS skeleton + a static busybox (with an applet symlink for
/// EVERY applet `busybox --list` reports) into `tree`. That gives the guest
/// `/bin/sh` (ash), the coreutils the stage-1 shim runs
/// (mount/mkdir/cat/cp/chmod/cut/…), and the userland the exec assertions
/// invoke (`echo`, `head`, `ip`, `ping`, `setsid`, …). Static busybox has no
/// shared-library deps, so nothing else is required for it to run.
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
    // Symlink every applet busybox ships to it, so `bin/<applet>` resolves.
    let list = std::process::Command::new(busybox).arg("--list").output()?;
    for applet in String::from_utf8_lossy(&list.stdout).split_whitespace() {
        // Applets are bare names; skip any with a path component (defensive).
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

/// Copy a dynamically-linked host tool (`tool`, an absolute path — resolve
/// via [`which`]) into `tree` at `guest_rel` (e.g. `usr/bin/socat`) together
/// with every shared object `ldd` reports, preserving each `.so`'s host path
/// inside the tree. The docker-free way to give a fixture a real `socat` /
/// `curl` without an apt layer; the KVM CI runner + dev-vm have these on the
/// host. Best-effort on the closure (a static tool reports none).
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
    // `ldd` prints lines like `libx.so => /path/libx.so (0x...)` plus the
    // dynamic loader `/lib64/ld-linux-...`. Copy every absolute path it names
    // to that same path inside the tree.
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

/// ADR 0080 §D: bake a fixture ext4 rootfs WITHOUT docker. Lays the busybox
/// skeleton into a scratch tree, runs `customize` on it (COPY-equivalents:
/// extra files / [`copy_host_tool_with_closure`] tools), injects the stage-1
/// init shim (`init`), `Mke2fsPacker`-packs it into `out_ext4`, and chunks it
/// into `chunk_store` (content-derived manifest, like the retired bake).
/// Gate callers with [`find_busybox`] (pure-Rust pack — no mke2fs).
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
    // Chunk the ext4 into the content-addressed store, mirroring the retired
    // Builder's ext4 branch: content-derived ref, idempotent put.
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
