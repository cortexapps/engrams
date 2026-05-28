//! ADR 0009 Phase 6: live-VM reattach pass.
//!
//! Run once at host-agent startup (before dialing the coord) when
//! the backend is Firecracker AND `ENGRAM_LIVE_ATTACH=1`. Scans the
//! work_dir for `<sandbox_id>/sandbox.json` manifests and, for each:
//!
//!   - Path 1 (this phase): pidfd-attach the still-live FC process.
//!     Three-axis identity verification + FC API liveness + TAP
//!     check. On success the sandbox rejoins `backend.list()` and
//!     reconcile will see it on the next heartbeat — no flip.
//!   - Path 2 (Phase 8): NVMe local-snapshot restore. Not implemented
//!     yet; this driver reports the path-1 failure cause so the
//!     Phase 8 layering knows what to look at.
//!   - Path 3 (this phase + Phase 10): orphan-reap. On any verification
//!     failure that isn't "FC alive but elsewhere" we delete the
//!     manifest and let the jail_dir get reaped by the existing
//!     `engram-host-agent/src/orphan_reap.rs` machinery. The reconcile
//!     pass then transitions the owning session per ADR 0009 §3.
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
                let reason = format!("{path1_err}");
                tracing::info!(
                    sandbox_id = %sandbox_id_str,
                    error = %reason,
                    "reattach pass: path 1 failed; trying path 2 (NVMe local-snapshot restore)"
                );
                // ADR 0009 Phase 8 path 2: when path 1 fails AND
                // the manifest carries a SIGTERM checkpoint
                // reference, re-spawn FC from the local snapshot
                // preserving the original sandbox_id. The coord's
                // routing then continues working without flipping
                // the session.
                match try_path2_restore(backend, &manifest).await {
                    Some(Ok(())) => {
                        tracing::info!(
                            sandbox_id = %sandbox_id_str,
                            "reattach pass: path 2 success (NVMe local-snapshot restore)"
                        );
                        report.reattached.push(ReattachOutcome::Reattached {
                            sandbox_id: sandbox_id_str,
                            pid: 0,
                        });
                        continue;
                    }
                    Some(Err(path2_err)) => {
                        tracing::warn!(
                            sandbox_id = %sandbox_id_str,
                            error = %path2_err,
                            "reattach pass: path 2 failed; falling through to orphan-reap"
                        );
                    }
                    None => {
                        tracing::debug!(
                            sandbox_id = %sandbox_id_str,
                            "reattach pass: no last_local_snapshot in manifest; path 2 not applicable"
                        );
                    }
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

/// ADR 0009 Phase 8: path 2 NVMe restore. Returns `Some(Ok)` on
/// successful restore, `Some(Err)` on failure (FC restore errored
/// out — caller falls through to orphan), `None` when the manifest
/// has no `last_local_snapshot` (no SIGTERM checkpoint was taken;
/// path 2 isn't applicable — caller orphans).
async fn try_path2_restore(
    backend: &Arc<engram_sandbox_firecracker::FirecrackerBackend>,
    manifest: &engram_sandbox_firecracker::sandbox_manifest::SandboxManifest,
) -> Option<Result<(), engram_core::SandboxError>> {
    let local = manifest.last_local_snapshot.as_ref()?;
    let snapshot_id = local.snapshot_id?;
    Some(
        backend
            .restore_as_sandbox_id(manifest.sandbox_id, snapshot_id)
            .await,
    )
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
            restore_mode: engram_sandbox_firecracker::RestoreMode::File,
            net_pool: None,
            egress_proxy_port: None,
            egress_dns_port: None,
            host_id: None,
            uffd_cache_root: None,
            stub_harness_path: None,
            working_set_trace_output: None,
            uffd_blob_root: None,
            cpu_template: None,
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
                cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
                memory: engram_core::types::sandbox::MemoryLimit { max_mib: 64 },
                disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
                ttl: None,
                env: Default::default(),
                workdir: None,
                network: Default::default(),
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
            uffd_handler: None,
            last_local_snapshot: None,
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
