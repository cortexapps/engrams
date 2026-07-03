//! ADR 0068: self-verified host capability probes.
//!
//! Each probe answers one question the host can prove about itself —
//! "am I actually ready for X" — instead of the coordinator inferring
//! it from a registered boolean or a later-discovered side-gate (the
//! gRPC listener gate, the tmpfs substrate withhold, the NBD dev
//! fallback, wire-version skew: four separate incident-driven bandages
//! at four different layers, none of them a property the scheduler
//! could see). `probe_all` is run once at startup (before the first
//! register) and re-run on every heartbeat tick; every probe here is
//! cheap by construction — statfs/stat/one TCP connect/a memfd-backed
//! uffd self-test are all sub-millisecond, and the FC binary version
//! is probed once and cached (a subprocess spawn per tick would not
//! be).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use engram_core::types::host::{CapStatus, HostCapabilities};

/// Inputs the probe loop needs, resolved once at startup from the
/// selected `SandboxBackend` + `HostAgentConfig` (`main.rs` picks the
/// backend; `lib.rs::run` builds this before spawning the
/// registration task). Kept as one struct so `probe_all` has a single
/// argument and adding a new probe doesn't ripple through every call
/// site.
#[derive(Clone, Debug)]
pub struct ProbeInputs {
    /// `"firecracker" | "vz" | "process"` — selected from the same
    /// arm `main.rs` picks the `SandboxBackend` in (ADR 0068 decision
    /// 11: detection stays above the cfg-gated leaves, not
    /// re-derived here).
    pub backend: String,
    /// Localhost address to TCP-self-connect for the gRPC gate.
    /// `None` when the gRPC server is disabled (mode=all, or the
    /// standalone dev binary with no `--grpc-listen-addr`) — the
    /// probe reports `NotApplicable`.
    pub grpc_probe_addr: Option<SocketAddr>,
    /// The bundle dir the backend reads its bundle stamp from
    /// (`SandboxBackend::bundle_dir()` — ADR 0062 single source of
    /// truth, so the vector reports readiness for the exact dir
    /// restore attaches from).
    pub bundle_dir: PathBuf,
    /// `Some` only when `backend == "firecracker"`.
    pub fc: Option<FcProbeInputs>,
}

/// FC-only probe inputs.
#[derive(Clone, Debug)]
pub struct FcProbeInputs {
    /// The configured `firecracker` binary path (`ENGRAM_FIRECRACKER_BIN`
    /// or a PATH lookup default).
    pub firecracker_bin: PathBuf,
    /// `engram_sandbox_firecracker::uffd_base_dir_from_env()` —
    /// `Some` only when the ADR 0045 substrate is configured
    /// (`ENGRAM_FC_UFFD_BASE_DIR`). `None` means this host runs the
    /// File-backend fallback (ADR 0022 Option A) instead — the
    /// substrate-specific capabilities (`base_shm_tmpfs`,
    /// `uffd_minor_shmem`) report `NotApplicable`, not `Failed`,
    /// because there's nothing to probe: the substrate was never
    /// asked for.
    pub uffd_base_dir: Option<PathBuf>,
}

/// `firecracker --snapshot-version` never changes mid-process — probe
/// once, cache. `OnceLock` (not `OnceCell`, no async init support
/// needed since we resolve the `Option<String>` result itself, not a
/// future) so concurrent heartbeat ticks never race the subprocess
/// spawn.
static FC_SNAPSHOT_VERSION: OnceLock<Option<String>> = OnceLock::new();

/// Whether `path` is on a tmpfs/shmem mount. Moved here from
/// `image_prefetch::dir_is_tmpfs` (ADR 0068) — one probe
/// implementation, two consumers: the prefetch readiness withhold
/// (which orders `prewarm_base_shm` after the mount, per the ADR 0045
/// comment there) and this capability vector (which is what the
/// scheduler actually gates placement on now). `TMPFS_MAGIC`
/// (0x0102_1994) covers tmpfs and shmem (incl. `/dev/shm`). A missing
/// path or a probe error reads as "not tmpfs" (i.e. not ready). On
/// non-Linux there is no substrate, so this is vacuously true — same
/// posture the original had.
#[cfg(target_os = "linux")]
pub fn dir_is_tmpfs(path: &Path) -> bool {
    match nix::sys::statfs::statfs(path) {
        Ok(s) => s.filesystem_type() == nix::sys::statfs::TMPFS_MAGIC,
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn dir_is_tmpfs(_path: &Path) -> bool {
    true
}

/// A single TCP self-connect to the host-agent's own gRPC listen
/// port, proving the listener is bound + backlogging. Single
/// attempt, short timeout — no retry loop. ADR 0068 deletes the old
/// 30s blocking gate (`lib.rs`'s pre-heartbeat loop): the heartbeat's
/// per-tick re-probe of this same check IS the retry, so a
/// still-binding listener just shows as `Failed` on this tick and
/// `Ok` on a later one, with the coordinator excluding placements in
/// between instead of the host racing to heartbeat before it can
/// actually serve anything.
pub async fn probe_grpc_self_connect(addr: Option<SocketAddr>) -> CapStatus {
    let Some(addr) = addr else {
        return CapStatus::NotApplicable;
    };
    let probe_addr = if addr.ip().is_unspecified() {
        match addr.ip() {
            IpAddr::V4(_) => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port()),
            IpAddr::V6(_) => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port()),
        }
    } else {
        addr
    };
    match tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::net::TcpStream::connect(probe_addr),
    )
    .await
    {
        Ok(Ok(_)) => CapStatus::Ok(None),
        Ok(Err(e)) => CapStatus::Failed(format!("connect {probe_addr}: {e}")),
        Err(_) => CapStatus::Failed(format!("connect {probe_addr}: timed out after 200ms")),
    }
}

/// ADR 0045 substrate readiness: the UFFD base dir must be a
/// tmpfs/shmem mount (`UFFDIO_REGISTER MINOR`, the canonical-page
/// sharing op, is shmem-only). `NotApplicable` when the substrate
/// isn't configured — nothing to probe, the host runs the
/// File-backend fallback instead (ADR 0068 guardrail: this probe
/// doesn't change that fallback, only makes the substrate's own
/// readiness visible to the scheduler instead of silently failing
/// the first restore that lands on an unmounted node).
pub async fn probe_base_shm_tmpfs(uffd_base_dir: Option<&Path>) -> CapStatus {
    let Some(dir) = uffd_base_dir else {
        return CapStatus::NotApplicable;
    };
    // Create the dir first so the statfs reflects the real backing
    // fs — mirrors the ordering `image_prefetch::prefetch_one` used
    // before this probe moved out of it: a subdir of an
    // always-present tmpfs, or a not-yet-mounted dedicated mountpoint
    // on the overlay. Mounting a tmpfs over an existing dir later is
    // fine.
    let _ = tokio::fs::create_dir_all(dir).await;
    if dir_is_tmpfs(dir) {
        CapStatus::Ok(None)
    } else {
        CapStatus::Failed(format!(
            "{} is not a tmpfs/shmem mount yet (node-prep may not have mounted it)",
            dir.display()
        ))
    }
}

/// A userfaultfd + `UFFDIO_REGISTER` MINOR self-test against a
/// memfd-backed (shmem) page — no guest involved. Proves the kernel
/// version + this process's privilege level actually support
/// MINOR-fault resolution, the op the ADR 0045 substrate's
/// canonical-page sharing relies on (a passing `base_shm_tmpfs` alone
/// only proves the mount type, not that `UFFDIO_REGISTER MINOR`
/// itself succeeds on this kernel). `NotApplicable` when the
/// substrate isn't configured (no VMA to prove the mode against) or
/// off Linux. Never panics — any failed step returns `Failed(<step>:
/// <errno/message>)`; CI runners that set
/// `vm.unprivileged_userfaultfd=0` or lack `/dev/userfaultfd` report
/// `Failed`, not a crash (the `userfaultfd` crate's `UffdBuilder`
/// already falls back from `/dev/userfaultfd` to the `userfaultfd(2)`
/// syscall internally).
#[cfg(target_os = "linux")]
pub fn probe_uffd_minor_shmem(substrate_configured: bool) -> CapStatus {
    use std::os::fd::AsFd;

    use nix::sys::memfd::{memfd_create, MFdFlags};
    use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
    use userfaultfd::{RegisterMode, UffdBuilder};

    if !substrate_configured {
        return CapStatus::NotApplicable;
    }

    let page_size = 4096usize;
    let fd = match memfd_create("engram-uffd-probe", MFdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => return CapStatus::Failed(format!("memfd_create: {e}")),
    };
    if let Err(e) = nix::unistd::ftruncate(fd.as_fd(), page_size as i64) {
        return CapStatus::Failed(format!("ftruncate: {e}"));
    }
    let mapped = unsafe {
        // SAFETY: `fd` is a fresh memfd sized by the `ftruncate` just
        // above; `page_size` is the exact mapping length we unmap
        // with below. No other code observes this address.
        mmap(
            None,
            std::num::NonZeroUsize::new(page_size).expect("page_size is nonzero"),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            fd.as_fd(),
            0,
        )
    };
    let ptr = match mapped {
        Ok(p) => p,
        Err(e) => return CapStatus::Failed(format!("mmap: {e}")),
    };

    let result = (|| {
        let uffd = UffdBuilder::new()
            .close_on_exec(true)
            .non_blocking(true)
            .user_mode_only(true)
            .create()
            .map_err(|e| format!("uffd_create: {e}"))?;
        uffd.register_with_mode(ptr.as_ptr(), page_size, RegisterMode::MINOR)
            .map_err(|e| format!("register_minor: {e}"))?;
        let _ = uffd.unregister(ptr.as_ptr(), page_size);
        Ok::<(), String>(())
    })();

    // SAFETY: `ptr` is exactly the region `mmap` returned above, and
    // nothing retains a reference to it past this point (the uffd
    // register/unregister pair above already released the kernel's
    // hold on it).
    unsafe {
        let _ = munmap(ptr, page_size);
    }

    match result {
        Ok(()) => CapStatus::Ok(None),
        Err(e) => CapStatus::Failed(e),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_uffd_minor_shmem(_substrate_configured: bool) -> CapStatus {
    CapStatus::NotApplicable
}

/// The `nbd` kernel module is loaded (`/sys/module/nbd/parameters/nbds_max`
/// readable). `NotApplicable` off the FC backend. Deliberately does
/// NOT change `disk_daemon::slot::build_from_kernel`'s
/// materialize-to-file dev fallback (that stays exactly as it is) —
/// this only makes a prod misconfiguration (module never loaded on a
/// host that should have it) visible instead of a silent capability
/// downgrade.
pub fn probe_nbd(is_fc_backend: bool) -> CapStatus {
    if !is_fc_backend {
        return CapStatus::NotApplicable;
    }
    match crate::disk_daemon::slot::kernel_nbds_max() {
        Some(n) => CapStatus::Ok(Some(format!("nbds_max={n}"))),
        None => CapStatus::Failed("nbd module not loaded (no /sys/module/nbd)".to_string()),
    }
}

/// `bundles::read_stamp(dir)` returned a non-empty stamp — this host
/// has *some* current RO-bundle generation staged. Required for ANY
/// FC placement (`host_meets_capabilities`), not just bundle-carrying
/// images: a host that can't attach bundles can't serve the built-in
/// harness either (ADR 0062).
pub async fn probe_bundle_stamp(dir: &Path) -> CapStatus {
    if crate::bundles::read_stamp(dir).await.is_empty() {
        CapStatus::Failed(format!("no bundle stamp at {}", dir.display()))
    } else {
        CapStatus::Ok(None)
    }
}

/// `firecracker --snapshot-version` output (e.g. `"v10.0.0"`),
/// probed once and cached — a subprocess spawn on every 5s heartbeat
/// tick would be wasteful for a value that can't change without a
/// process restart. `None` off the FC backend.
pub async fn fc_snapshot_version(firecracker_bin: Option<&Path>) -> Option<String> {
    let bin = firecracker_bin?;
    if let Some(cached) = FC_SNAPSHOT_VERSION.get() {
        return cached.clone();
    }
    let output = tokio::process::Command::new(bin)
        .arg("--snapshot-version")
        .output()
        .await
        .ok();
    let version = output.and_then(|o| {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        } else {
            None
        }
    });
    // Only cache a SUCCESSFUL probe. `version == None` here can be a
    // transient failure (fork EAGAIN under boot-time load, the binary
    // momentarily missing mid-thin-layer-bake) — caching it would lock
    // this host into a permanently unconstrained (`fc_snapshot_version:
    // None`) posture until process restart: every snapshot it captures
    // stays version-NULL and every restore onto it stays unconstrained,
    // invisibly (`None` also legitimately means "not FC", so nothing
    // downstream distinguishes the two). A `None` re-probe is one cheap
    // failed subprocess spawn per heartbeat tick and self-heals once the
    // transient condition clears. `OnceLock::set` losing a race on the
    // success path just means a concurrent caller's freshly-probed value
    // (identical, since the binary doesn't change) is discarded in favor
    // of the winner's — harmless.
    if let Some(v) = &version {
        let _ = FC_SNAPSHOT_VERSION.set(Some(v.clone()));
    }
    version
}

/// Run every probe and assemble the vector this tick. `schema: 1`
/// unconditionally — a `HostCapabilities::default()` (`schema: 0`)
/// only ever comes from an old host-agent's absent field, never from
/// this function.
pub async fn probe_all(inputs: &ProbeInputs) -> HostCapabilities {
    let is_fc = inputs.backend == "firecracker";
    let uffd_base_dir = inputs
        .fc
        .as_ref()
        .and_then(|fc| fc.uffd_base_dir.as_deref());
    let substrate_configured = is_fc && uffd_base_dir.is_some();

    let grpc_self_connect = probe_grpc_self_connect(inputs.grpc_probe_addr).await;
    let base_shm_tmpfs = if is_fc {
        probe_base_shm_tmpfs(uffd_base_dir).await
    } else {
        CapStatus::NotApplicable
    };
    let uffd_minor_shmem = probe_uffd_minor_shmem(substrate_configured);
    let nbd = probe_nbd(is_fc);
    let bundle_stamp = probe_bundle_stamp(&inputs.bundle_dir).await;
    let fc_snapshot_version =
        fc_snapshot_version(inputs.fc.as_ref().map(|fc| fc.firecracker_bin.as_path())).await;

    HostCapabilities {
        schema: 1,
        backend: inputs.backend.clone(),
        grpc_self_connect,
        base_shm_tmpfs,
        uffd_minor_shmem,
        nbd,
        bundle_stamp,
        fc_snapshot_version,
        wire_version: engram_protocol::WIRE_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grpc_self_connect_not_applicable_when_no_addr() {
        assert_eq!(
            probe_grpc_self_connect(None).await,
            CapStatus::NotApplicable
        );
    }

    #[tokio::test]
    async fn grpc_self_connect_ok_against_a_real_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        assert_eq!(
            probe_grpc_self_connect(Some(addr)).await,
            CapStatus::Ok(None)
        );
    }

    #[tokio::test]
    async fn grpc_self_connect_failed_against_a_closed_port() {
        // Bind then immediately drop to free the port but make a
        // connect to it very likely to be refused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        match probe_grpc_self_connect(Some(addr)).await {
            CapStatus::Failed(_) => {}
            other => panic!("expected Failed against a closed port, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn base_shm_tmpfs_not_applicable_when_substrate_unconfigured() {
        assert_eq!(probe_base_shm_tmpfs(None).await, CapStatus::NotApplicable);
    }

    // `dir_is_tmpfs` (and therefore `probe_base_shm_tmpfs`) is vacuously
    // `true` off Linux (no substrate exists there) — see the fn doc.
    // This assertion is Linux-only for the same reason the original
    // `image_prefetch::dir_is_tmpfs_identifies_shmem_and_rejects_others`
    // test was.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn base_shm_tmpfs_rejects_a_plain_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        // A bare tempdir is never tmpfs on a dev machine / CI runner
        // (it's on the runner's real filesystem) — cross-checked
        // against `dir_is_tmpfs` directly, since a genuine tmpfs mount
        // isn't guaranteed to exist as `/tmp` on every CI image.
        if dir_is_tmpfs(tmp.path()) {
            // Some CI containers mount /tmp as tmpfs; skip rather
            // than flake.
            return;
        }
        assert!(matches!(
            probe_base_shm_tmpfs(Some(tmp.path())).await,
            CapStatus::Failed(_)
        ));
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn base_shm_tmpfs_vacuously_ok_off_linux() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_base_shm_tmpfs(Some(tmp.path())).await,
            CapStatus::Ok(None)
        );
    }

    /// ADR 0068 guardrail: this probe must never panic, and a
    /// `Failed` result must name the step that failed. We don't
    /// assert `Ok` — CI runners may set `vm.unprivileged_userfaultfd=0`
    /// or lack `/dev/userfaultfd` entirely.
    #[cfg(target_os = "linux")]
    #[test]
    fn uffd_minor_shmem_never_panics_and_failed_names_a_step() {
        match probe_uffd_minor_shmem(true) {
            CapStatus::Ok(_) => {}
            CapStatus::Failed(msg) => assert!(
                msg.contains(':'),
                "Failed detail must carry a `<step>: <error>` shape, got {msg:?}",
            ),
            other => panic!("expected Ok or Failed with substrate_configured=true, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uffd_minor_shmem_not_applicable_when_substrate_unconfigured() {
        assert_eq!(probe_uffd_minor_shmem(false), CapStatus::NotApplicable);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn uffd_minor_shmem_not_applicable_off_linux() {
        assert_eq!(probe_uffd_minor_shmem(true), CapStatus::NotApplicable);
    }

    #[test]
    fn nbd_not_applicable_off_fc_backend() {
        assert_eq!(probe_nbd(false), CapStatus::NotApplicable);
    }

    #[tokio::test]
    async fn bundle_stamp_failed_on_an_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_bundle_stamp(tmp.path()).await,
            CapStatus::Failed(_)
        ));
    }

    #[tokio::test]
    async fn fc_snapshot_version_none_when_binary_missing() {
        // A path that doesn't exist. Uses a distinct name so it
        // doesn't collide with the OnceLock cache another test in
        // this process may have already populated.
        let bin = PathBuf::from("/nonexistent/engram-test-firecracker-probe");
        assert_eq!(fc_snapshot_version(Some(&bin)).await, None);
        // A `None` (failed/transient) result must NEVER be cached — only
        // the success path calls `OnceLock::set`. Calling again with the
        // same missing binary re-probes (a cheap failed spawn) instead of
        // short-circuiting to a locked-in `None`; this is the actual
        // regression the review's finding 4 (OnceLock permanently caching
        // a transient probe failure) is about, so assert the repeat call
        // still reaches the real subprocess path and returns None on its
        // own merits rather than returning early via `.get()`.
        assert_eq!(fc_snapshot_version(Some(&bin)).await, None);
    }

    #[tokio::test]
    async fn fc_snapshot_version_none_off_fc_backend() {
        assert_eq!(fc_snapshot_version(None).await, None);
    }
}
