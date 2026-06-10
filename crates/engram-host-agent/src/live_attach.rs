//! Live-VM reattach pass (ADR 0009 §6, made load-bearing by ADR 0044 K2).
//!
//! Run once at host-agent startup (before dialing the coord) when the
//! backend is Firecracker. Scans the work_dir for
//! `<sandbox_id>/sandbox.json` manifests and, for each:
//!
//! - Reattach: pidfd-attach the still-live FC (and uffd-handler)
//!   process(es). Three-axis identity verification + FC API liveness +
//!   TAP check. On success the sandbox rejoins `backend.list()`, so the
//!   very first heartbeat re-advertises it — the coord never sees it
//!   missing, no session flip. This is the routine path under ADR 0044
//!   K2: a host-agent restart *detaches* its VMs (leaves them running
//!   under hostPID) and the successor re-adopts them here.
//! - Orphan-reap: when the FC process is truly gone (or identity
//!   fails), delete the manifest and let `orphan_reap.rs` reap the
//!   jail_dir. Reconcile then transitions the owning session, which
//!   warm-recovers elsewhere from its last periodic checkpoint.
//!
//! There is deliberately no "restore from a SIGTERM checkpoint" path:
//! ADR 0044 K2 removed the on-shutdown checkpoint entirely. Durability
//! for an uncontrolled node loss rides the always-on periodic
//! checkpoint; controlled drains migrate sessions off the node first.
//!
//! The driver itself is backend-agnostic in shape — currently only
//! the FC backend implements `reattach_sandbox`. VZ + process get a
//! one-line "no reattach for this backend" log and clean-slate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::SandboxBackend;

/// Per-sandbox outcome of the reattach pass. Aggregated into
/// [`ReattachReport`] for structured logging + the eventual
/// `/api/admin/reconcile-now` JSON output.
#[derive(Debug)]
pub enum ReattachOutcome {
    /// Path 1 succeeded: FC process verified + reattached. Sandbox
    /// rejoins `backend.list()`.
    Reattached { sandbox_id: String, pid: u32 },
    /// Path 1 failed for a reason that means "the original FC is
    /// truly gone." Caller can choose to try path 2 (Phase 8) or
    /// fall through to orphan-reap (which is what we do today).
    Orphaned {
        sandbox_id: String,
        manifest_path: PathBuf,
        reason: String,
    },
    /// The manifest itself was malformed or referenced a schema we
    /// don't support. Treated the same as orphaned — drop the
    /// manifest, let reconcile clean up the session row.
    Malformed {
        manifest_path: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Default)]
pub struct ReattachReport {
    pub reattached: Vec<ReattachOutcome>,
    pub orphaned: Vec<ReattachOutcome>,
    pub malformed: Vec<ReattachOutcome>,
}

impl ReattachReport {
    /// One-line summary for `tracing::info!` at startup.
    pub fn summary(&self) -> String {
        format!(
            "reattach pass: {} reattached, {} orphaned, {} malformed",
            self.reattached.len(),
            self.orphaned.len(),
            self.malformed.len()
        )
    }
}

/// Run the reattach pass against `work_dir`. The `backend` is the
/// concrete `FirecrackerBackend` — we downcast through `Any` since
/// the trait doesn't expose `reattach_sandbox`. (Phase 6 is FC-only;
/// adding the method to the trait would require all backends to
/// implement it, and we explicitly decided VZ doesn't.)
///
/// Returns even on no-manifests-found — that's the normal first-boot
/// path. Errors only on filesystem-level problems (work_dir
/// unreadable, etc.).
pub async fn reattach_pass(
    work_dir: &Path,
    backend: &Arc<engram_sandbox_firecracker::FirecrackerBackend>,
) -> std::io::Result<ReattachReport> {
    let mut report = ReattachReport::default();

    let entries = match std::fs::read_dir(work_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Fresh host: no sandboxes were ever created. Normal.
            tracing::debug!(
                work_dir = %work_dir.display(),
                "reattach pass: work_dir doesn't exist; nothing to reattach"
            );
            return Ok(report);
        }
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("sandbox.json");
        if !manifest_path.exists() {
            // Subdirectory that isn't a sandbox jail (e.g.
            // host_startup chunks, the chunk cache, etc). Skip.
            continue;
        }
        let manifest = match engram_sandbox_firecracker::sandbox_manifest::read_manifest(
            &manifest_path,
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "reattach pass: malformed manifest; deleting + falling through to orphan-reap"
                );
                engram_sandbox_firecracker::sandbox_manifest::delete_manifest(&manifest_path);
                report.malformed.push(ReattachOutcome::Malformed {
                    manifest_path: manifest_path.clone(),
                    reason: format!("{e}"),
                });
                continue;
            }
        };

        let sandbox_id_str = manifest.sandbox_id.to_string();
        match backend.reattach_sandbox(&manifest).await {
            Ok(()) => {
                tracing::info!(
                    sandbox_id = %sandbox_id_str,
                    pid = manifest.firecracker.process.pid,
                    "reattach pass: path 1 success (pidfd verified, sandbox rejoined backend.list())"
                );
                report.reattached.push(ReattachOutcome::Reattached {
                    sandbox_id: sandbox_id_str,
                    pid: manifest.firecracker.process.pid,
                });
            }
            Err(path1_err) => {
                // Reattach failed: the FC exited, its identity didn't match, a
                // dead API socket, or an unrecoverable network state. ADR 0044
                // K2 has no local restore fallback — drop the manifest and let
                // reconcile transition the owning session, which warm-recovers
                // elsewhere from its last periodic checkpoint.
                let reason = format!("{path1_err}");
                tracing::info!(
                    sandbox_id = %sandbox_id_str,
                    error = %reason,
                    "reattach pass: FC unreattachable; orphan-reaping (session reconciles)"
                );
                // ADR 0044 K2: don't leak a still-running microVM. The FC may
                // have died (then the pid is harmless), but it may also be
                // ALIVE-but-unreattachable — leaving it running orphans a
                // microVM that no host-agent owns, burning the node's RAM. Kill
                // the FC + its uffd-handler IFF they're still the recorded
                // processes (start-time identity match); a recycled pid is left
                // untouched.
                reap_orphan_if_alive(
                    manifest.firecracker.process.pid,
                    manifest.firecracker.process.start_time_jiffies,
                    "firecracker",
                );
                if let Some(uffd) = manifest.uffd_handler.as_ref() {
                    reap_orphan_if_alive(uffd.pid, uffd.start_time_jiffies, "uffd-handler");
                }
                engram_sandbox_firecracker::sandbox_manifest::delete_manifest(&manifest_path);
                report.orphaned.push(ReattachOutcome::Orphaned {
                    sandbox_id: sandbox_id_str,
                    manifest_path: manifest_path.clone(),
                    reason,
                });
            }
        }
    }

    Ok(report)
}

/// ADR 0044 K2: SIGKILL a manifest-recorded process IFF it's still the
/// original — verified by `/proc` start-time identity, so a recycled pid is
/// never signalled (the kernel reuses pids but not `(pid, start_time)` pairs).
/// The orphan-reap uses this to stop a leaked, unreattachable microVM (FC +
/// uffd-handler) from burning the node's RAM with no owner. On non-Linux
/// `read_proc_start_time_jiffies` returns `None`, so this is a no-op.
fn reap_orphan_if_alive(pid: u32, manifest_start_jiffies: u64, what: &str) {
    let live = engram_sandbox_firecracker::sandbox_manifest::read_proc_start_time_jiffies(pid);
    if live == Some(manifest_start_jiffies) {
        // SAFETY: SIGKILL on a pid whose `/proc` start-time we just verified
        // matches the manifest — provably the recorded process, not a recycled
        // pid. `libc::kill` with a recorded pid is the established pattern in
        // the FC backend's own teardown.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        tracing::info!(
            pid,
            what,
            rc,
            "ADR 0044 K2: SIGKILL'd leaked orphan microVM process (identity-matched)"
        );
    } else {
        tracing::debug!(
            pid,
            what,
            "orphan-reap: process gone or recycled; not signalling"
        );
    }
}

/// Trait-level entry point that takes any `SandboxBackend` and
/// no-ops for non-FC backends. Used by `lib::run` so it can call
/// `reattach_pass_any(&work_dir, &sandbox)` without inspecting the
/// backend type itself.
pub async fn reattach_pass_any(
    work_dir: &Path,
    backend: &Arc<dyn SandboxBackend>,
) -> std::io::Result<ReattachReport> {
    if let Some(fc) = downcast_to_fc(backend) {
        reattach_pass(work_dir, &fc).await
    } else {
        tracing::debug!(
            "reattach pass: backend is not Firecracker; skipping (VZ/process don't \
             support reattach across host-agent restart; clean-slate startup)"
        );
        Ok(ReattachReport::default())
    }
}

/// Attempts an Arc<dyn SandboxBackend> → Arc<FirecrackerBackend>
/// downcast. The trait isn't `Any` so we can't use `Arc::downcast`
/// directly; instead we go through a per-crate accessor pattern
/// that's set up in lib::run via the concrete-typed wiring. For
/// now, return None — Phase 6's host-agent integration plumbs the
/// concrete FC backend through a separate channel.
fn downcast_to_fc(
    _backend: &Arc<dyn SandboxBackend>,
) -> Option<Arc<engram_sandbox_firecracker::FirecrackerBackend>> {
    // We can't downcast through `dyn SandboxBackend` (the trait
    // doesn't extend `Any`). Phase 6's host-agent wiring instead
    // passes a typed `Option<Arc<FirecrackerBackend>>` directly via
    // a side channel (see `HostAgent` builder). This function
    // exists as a placeholder for a future trait method
    // `fn as_firecracker(&self) -> Option<&FirecrackerBackend>`
    // that we'd add only if we wanted backends to expose reattach
    // through the trait — but the current ADR scope keeps reattach
    // FC-specific.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_sandbox_firecracker::sandbox_manifest::{
        FirecrackerProcessRecord, ProcessRecord, SandboxManifest, BACKEND_FIRECRACKER,
        SCHEMA_VERSION,
    };
    use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

    fn fc_backend(work_dir: &Path) -> Arc<FirecrackerBackend> {
        let cfg = FirecrackerConfig {
            kernel_image_path: work_dir.join("nonexistent-vmlinux"),
            default_boot_args: "console=ttyS0".into(),
            guest_otel_endpoint: None,
            firecracker_bin: PathBuf::from("/nonexistent/firecracker"),
            uffd_handler_bin: PathBuf::from("/nonexistent/engram-uffd-handler"),
            uffd_base_dir: None,
            restore_mode: engram_sandbox_firecracker::RestoreMode::File,
            base_restore_mode: None,
            track_dirty_pages: false,
            net_pool: None,
            egress_proxy_port: None,
            egress_dns_port: None,
            host_id: None,
            uffd_cache_root: None,
            stub_harness_path: None,
            working_set_trace_output: None,
            uffd_blob_root: None,
            cpu_template: None,
            bundle_dir: work_dir.join("bundles"),
            vm_cgroup_parent: None,
        };
        Arc::new(FirecrackerBackend::new(work_dir, cfg))
    }

    #[tokio::test]
    async fn empty_work_dir_returns_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = fc_backend(tmp.path());
        let report = reattach_pass(tmp.path(), &backend).await.unwrap();
        assert!(report.reattached.is_empty());
        assert!(report.orphaned.is_empty());
        assert!(report.malformed.is_empty());
    }

    #[tokio::test]
    async fn malformed_manifest_is_deleted_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = fc_backend(tmp.path());
        let sandbox_dir = tmp.path().join("00000000-0000-0000-0000-000000000001");
        std::fs::create_dir_all(&sandbox_dir).unwrap();
        let manifest_path = sandbox_dir.join("sandbox.json");
        std::fs::write(&manifest_path, b"{ this is not valid json ").unwrap();

        let report = reattach_pass(tmp.path(), &backend).await.unwrap();
        assert!(report.reattached.is_empty());
        assert!(report.orphaned.is_empty());
        assert_eq!(report.malformed.len(), 1);
        assert!(
            !manifest_path.exists(),
            "malformed manifest must be deleted to prevent repeat-fail on next startup"
        );
    }

    #[tokio::test]
    async fn dead_pid_is_orphaned_not_reattached() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = fc_backend(tmp.path());
        let sandbox_id = engram_core::SandboxId::new();
        let sandbox_dir = tmp.path().join(sandbox_id.to_string());
        std::fs::create_dir_all(&sandbox_dir).unwrap();
        let manifest_path = sandbox_dir.join("sandbox.json");

        let manifest = SandboxManifest {
            schema_version: SCHEMA_VERSION,
            sandbox_id,
            backend: BACKEND_FIRECRACKER.to_string(),
            spec: engram_core::types::sandbox::SandboxSpec {
                image: "test".into(),
                rootfs_source: None,
                image_uri: None,
                rootfs_manifest: None,
                cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
                memory: engram_core::types::sandbox::MemoryLimit { max_mib: 64 },
                disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
                ttl: None,
                env: Default::default(),
                workdir: None,
                network: Default::default(),
                aux_ro_drives: Vec::new(),
            },
            firecracker: FirecrackerProcessRecord {
                process: ProcessRecord {
                    // Guaranteed-dead pid (above kernel default pid_max).
                    pid: 16_000_001,
                    start_time_jiffies: 42,
                    comm: "firecracker".into(),
                },
                api_socket: tmp.path().join("nonexistent.sock"),
                vsock_uds_base: tmp.path().join("nonexistent.vsock"),
                rootfs_canonical: tmp.path().join("rootfs/nonexistent.dev"),
                vsock_cid: 3,
            },
            network: None,
            netns: None,
            uffd_handler: None,
        };
        engram_sandbox_firecracker::sandbox_manifest::write_manifest(&manifest_path, &manifest)
            .unwrap();

        let report = reattach_pass(tmp.path(), &backend).await.unwrap();
        assert!(report.reattached.is_empty());
        assert_eq!(report.orphaned.len(), 1);
        assert!(report.malformed.is_empty());
        assert!(
            !manifest_path.exists(),
            "orphaned manifest must be deleted so reconcile + future startup don't retry"
        );
    }
}
