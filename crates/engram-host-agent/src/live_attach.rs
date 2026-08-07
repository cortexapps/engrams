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

use engram_core::SandboxId;

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
    /// don't support. When the recorded process identities are still
    /// recoverable from the raw JSON (the schema-bump case), the arm
    /// reaps them and removes the jail dir like the orphan arm; when
    /// they are not (garbage bytes), the dir and manifest stay in
    /// place as the maybe-running VM's only on-disk handle. Reconcile
    /// cleans up the session row either way.
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

/// ADR 0045 C2: scan the work dir's sandbox manifests for persisted
/// post-copy migration roles — sandboxes that were mid-move when the
/// previous host-agent generation died. The startup pass re-arms the
/// lifecycle fences from this: a reattached SOURCE stays paused under
/// the dumb-host ownership rule (NEVER resumed — state may have
/// shipped); a reattached DEST is reaped (its drain state died with
/// the old generation; the coordinator rewinds the session).
pub fn scan_migration_roles(
    work_dir: &Path,
) -> Vec<(engram_core::SandboxId, crate::migration::MigrationRole)> {
    use engram_sandbox_firecracker::sandbox_manifest;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(work_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("sandbox.json");
        if !path.exists() {
            continue;
        }
        let Ok(m) = sandbox_manifest::read_manifest(&path) else {
            continue;
        };
        if let Some(role) = m
            .migration_role
            .as_deref()
            .and_then(crate::migration::MigrationRole::parse)
        {
            out.push((m.sandbox_id, role));
        }
    }
    out
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
                // Review finding on #1001: a malformed manifest can
                // front a STILL-RUNNING VM — the reachable case is a
                // SCHEMA_VERSION bump, where every detached survivor's
                // manifest parses as JSON but rejects on the version
                // field. Reap the recorded processes first (the same
                // identity-checked kill the orphan arm does), extracted
                // leniently from the raw JSON, then remove the whole
                // dir. When even the pid is unrecoverable (garbage
                // bytes — manifests are written atomically, so this is
                // node-corruption territory), keep the dir AND the
                // manifest: an unkillable maybe-running VM must retain
                // its on-disk handle for ops, and the residue sweep
                // deliberately never touches a dir that still has a
                // sandbox.json.
                match extract_process_records(&manifest_path) {
                    Some(records) => {
                        tracing::warn!(
                            path = %manifest_path.display(),
                            error = %e,
                            "reattach pass: malformed manifest; reaping recorded processes + removing the jail dir"
                        );
                        for (pid, start_jiffies, what) in records {
                            reap_orphan_if_alive(pid, start_jiffies, what);
                        }
                        if let Some(jail_dir) = manifest_path.parent() {
                            if let Err(rm) = std::fs::remove_dir_all(jail_dir) {
                                tracing::warn!(
                                    path = %jail_dir.display(),
                                    error = %rm,
                                    "reattach pass: removing malformed jail dir failed",
                                );
                            }
                        }
                    }
                    None => {
                        tracing::warn!(
                            path = %manifest_path.display(),
                            error = %e,
                            "reattach pass: manifest unreadable and process records \
                             unrecoverable; leaving the jail dir in place for ops \
                             (a recorded VM may still be running)"
                        );
                    }
                }
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
                // The WHOLE dir (see the malformed arm): the reaped
                // sandbox can never be wanted again, and a manifest-less
                // dir would leak invisibly.
                if let Some(jail_dir) = manifest_path.parent() {
                    if let Err(rm) = std::fs::remove_dir_all(jail_dir) {
                        tracing::warn!(
                            path = %jail_dir.display(),
                            error = %rm,
                            "reattach pass: removing orphaned jail dir failed",
                        );
                    }
                }
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

/// Leniently extract the recorded `(pid, start_time_jiffies, role)`
/// pairs from a manifest that failed strict parsing. A schema-version
/// mismatch (the reachable malformed case: a SCHEMA_VERSION bump over
/// detached survivors) still deserializes as JSON, so the process
/// identities the orphan reap needs are recoverable even when the
/// typed read is not. `None` = not even the pid is trustworthy
/// (garbage bytes) — the caller must NOT delete the VM's on-disk
/// handles.
fn extract_process_records(manifest_path: &Path) -> Option<Vec<(u32, u64, &'static str)>> {
    let raw = std::fs::read(manifest_path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let proc_of = |v: &serde_json::Value| -> Option<(u32, u64)> {
        let pid = v.get("pid")?.as_u64()?;
        let start = v.get("start_time_jiffies")?.as_u64()?;
        Some((u32::try_from(pid).ok()?, start))
    };
    let fc = proc_of(value.get("firecracker")?.get("process")?)?;
    let mut out = vec![(fc.0, fc.1, "firecracker")];
    if let Some(uffd) = value.get("uffd_handler").and_then(proc_of) {
        out.push((uffd.0, uffd.1, "uffd-handler"));
    }
    Some(out)
}

/// What [`sweep_dead_sandbox_residue`] removed, for the startup log.
#[derive(Debug, Default)]
pub struct ResidueSweepReport {
    pub jail_dirs: usize,
    pub vsock_files: usize,
    pub canonical_entries: usize,
    /// ADR 0112: leaked `swap/<id>.img` backings reclaimed (the design
    /// is unlink-after-attach, so any present at startup is a leak).
    pub swap_backings: usize,
}

/// Remove the on-disk residue of sandboxes with no surviving VM: uuid
/// jail dirs, the `<sandbox_id>.vsock*` sockets that live OUTSIDE the
/// jail by design, and the ADR 0014 canonical entries. `destroy()`
/// removes all of these on the happy path, but the orphan/malformed
/// reap used to drop only `sandbox.json` (leaving a manifest-less dir
/// no later pass could see), a failed `remove_dir_all` was "left for
/// ops cleanup" with nothing behind it, and the vsock sockets leaked
/// unconditionally (prod 2026-08-04: ~259 sessions' socket files on a
/// days-old node with 3 live VMs).
///
/// Safety mirrors the ADR 0110 dirty-root sweep exactly: run AFTER the
/// reattach pass fixes the live set and BEFORE coordinator
/// registration can create or resume anything, and sandbox ids never
/// recur — so an entry whose id is not live can never be wanted again.
/// Non-uuid entries (rootfs/, snapshots/, chunk-cache/, bindings/, …)
/// never parse as a `SandboxId` and pass through untouched.
pub fn sweep_dead_sandbox_residue(
    work_dir: &Path,
    live: &std::collections::HashSet<SandboxId>,
) -> std::io::Result<ResidueSweepReport> {
    let mut report = ResidueSweepReport::default();
    let mut dead_ids: std::collections::HashSet<SandboxId> = std::collections::HashSet::new();
    for entry in std::fs::read_dir(work_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let Ok(id) = name.parse::<SandboxId>() else {
                continue;
            };
            if live.contains(&id) {
                continue;
            }
            // A dir that still carries a sandbox.json is the reattach
            // pass's jurisdiction, never the sweep's: the pass either
            // adopted it (live), or reaped-and-removed it, or
            // DELIBERATELY kept it because the manifest was garbage and
            // a recorded VM may still be running with this dir as its
            // only on-disk handle (review finding on #1001). The sweep
            // removes only manifest-less residue.
            if entry.path().join("sandbox.json").exists() {
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    report.jail_dirs += 1;
                    dead_ids.insert(id);
                }
                Err(e) => tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "residue sweep: removing dead jail dir failed",
                ),
            }
        } else {
            // `<sandbox_id>.vsock` and its `<...>.vsock_<port>` siblings.
            let Some((stem, _)) = name.split_once(".vsock") else {
                continue;
            };
            let Ok(id) = stem.parse::<SandboxId>() else {
                continue;
            };
            if live.contains(&id) {
                continue;
            }
            // Same jurisdiction rule as the dir arm: a kept jail dir
            // (garbage manifest, maybe-running VM) keeps its sockets.
            if work_dir.join(id.to_string()).join("sandbox.json").exists() {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    report.vsock_files += 1;
                    dead_ids.insert(id);
                }
                Err(e) => tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "residue sweep: removing dead vsock socket failed",
                ),
            }
        }
    }
    for id in dead_ids {
        for path in engram_sandbox_firecracker::paths::canonical_entries_for(work_dir, id) {
            match std::fs::remove_file(&path) {
                Ok(()) => report.canonical_entries += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "residue sweep: removing dead canonical entry failed",
                ),
            }
        }
    }
    // ADR 0112: leaked swap BACKING files (`<work_dir>/swap/<id>.img`).
    // The design is unlink-after-attach, so any `.img` present at
    // startup is a leak: an aborted create/restore whose process died
    // before the drop-guard ran, or a failed unlink whose sandbox never
    // reached destroy. Removal is safe even for a LIVE sandbox — a
    // running FC holds the fd (that is the whole unlink-after-attach
    // contract), so this just completes the unlink it was owed; the
    // sweep runs at startup before any create can be mid-flight. Only
    // `<uuid>.img` names are touched; the `.swap` canonical symlinks
    // stay owned by the dead_ids loop above / destroy.
    let swap_dir = work_dir.join("swap");
    if let Ok(entries) = std::fs::read_dir(&swap_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".img") else {
                continue;
            };
            if stem.parse::<SandboxId>().is_err() {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    report.swap_backings += 1;
                    tracing::info!(
                        path = %entry.path().display(),
                        "residue sweep: reclaimed leaked swap backing",
                    );
                }
                Err(e) => tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "residue sweep: removing leaked swap backing failed",
                ),
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
            uffd_substrate_sock: None,
            restore_mode: engram_sandbox_firecracker::RestoreMode::File,
            fresh_restore_override: None,
            track_dirty_pages: false,
            balloon: false,
            net_pool: None,
            egress_proxy_port: None,
            egress_dns_port: None,
            egress_metadata_port: None,
            host_id: None,
            uffd_cache_root: None,
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
    async fn garbage_manifest_keeps_the_jail_dir_for_ops() {
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
            sandbox_dir.exists() && manifest_path.exists(),
            "garbage bytes = process records unrecoverable: a maybe-running \
             VM must keep its only on-disk handle (review finding on #1001)"
        );
    }

    /// The reachable malformed case (review finding on #1001): a
    /// SCHEMA_VERSION bump makes every detached survivor's manifest
    /// parse as JSON but fail the typed read. The recorded processes
    /// are reaped (identity-checked — a dead/recycled pid is a no-op)
    /// and only then is the jail dir removed.
    #[tokio::test]
    async fn schema_mismatch_manifest_reaps_then_removes_the_jail_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = fc_backend(tmp.path());
        let sandbox_id = engram_core::SandboxId::new();
        let sandbox_dir = tmp.path().join(sandbox_id.to_string());
        std::fs::create_dir_all(&sandbox_dir).unwrap();
        let manifest_path = sandbox_dir.join("sandbox.json");
        // Valid JSON with the process records present, wrong schema.
        std::fs::write(
            &manifest_path,
            serde_json::json!({
                "schema_version": SCHEMA_VERSION + 999,
                "sandbox_id": sandbox_id,
                "backend": BACKEND_FIRECRACKER,
                "firecracker": {
                    "process": {
                        "pid": 16_000_001u32,
                        "start_time_jiffies": 42u64,
                        "comm": "firecracker",
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let report = reattach_pass(tmp.path(), &backend).await.unwrap();
        assert_eq!(report.malformed.len(), 1);
        assert!(
            !sandbox_dir.exists(),
            "with recoverable process records the arm reaps (dead pid = \
             no-op) and removes the whole dir"
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
                swap_mib: None,
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
                swap_canonical: None,
                vsock_cid: 3,
            },
            network: None,
            netns: None,
            uffd_handler: None,
            migration_role: None,
        };
        engram_sandbox_firecracker::sandbox_manifest::write_manifest(&manifest_path, &manifest)
            .unwrap();

        let report = reattach_pass(tmp.path(), &backend).await.unwrap();
        assert!(report.reattached.is_empty());
        assert_eq!(report.orphaned.len(), 1);
        assert!(report.malformed.is_empty());
        assert!(
            !sandbox_dir.exists(),
            "the WHOLE orphaned jail dir must be removed — a manifest-less \
             dir is invisible to every later pass and leaks until node \
             replacement"
        );
    }

    /// The startup residue sweep: dead uuid jail dirs, dead `.vsock*`
    /// sockets, and dead canonical entries are removed; live sandboxes'
    /// entries and non-uuid directories are untouched. (Prod
    /// 2026-08-04: ~259 sessions' vsock sockets on a days-old node
    /// with 3 live VMs.)
    #[test]
    fn residue_sweep_removes_dead_keeps_live_and_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let live_id = engram_core::SandboxId::new();
        let dead_dir_id = engram_core::SandboxId::new();
        let dead_sock_id = engram_core::SandboxId::new();

        // Live jail dir + its vsock socket.
        std::fs::create_dir_all(tmp.path().join(live_id.to_string())).unwrap();
        std::fs::write(tmp.path().join(format!("{live_id}.vsock")), b"").unwrap();
        // Dead jail dir with contents (the orphan-leak shape).
        let dead_dir = tmp.path().join(dead_dir_id.to_string());
        std::fs::create_dir_all(&dead_dir).unwrap();
        std::fs::write(dead_dir.join("firecracker.log"), b"log").unwrap();
        // Dead vsock sockets (base + forwarded-port siblings).
        std::fs::write(tmp.path().join(format!("{dead_sock_id}.vsock")), b"").unwrap();
        std::fs::write(tmp.path().join(format!("{dead_sock_id}.vsock_1026")), b"").unwrap();
        // Dead canonical entry.
        let canonical =
            engram_sandbox_firecracker::paths::canonical_entries_for(tmp.path(), dead_sock_id);
        std::fs::create_dir_all(canonical[0].parent().unwrap()).unwrap();
        std::fs::write(&canonical[0], b"dev").unwrap();
        // A dead-id dir that still carries a sandbox.json is the
        // reattach pass's jurisdiction (garbage-manifest kept-dir case)
        // — the sweep must not touch it, nor its sockets.
        let kept_id = engram_core::SandboxId::new();
        let kept_dir = tmp.path().join(kept_id.to_string());
        std::fs::create_dir_all(&kept_dir).unwrap();
        std::fs::write(kept_dir.join("sandbox.json"), b"{ garbage ").unwrap();
        std::fs::write(tmp.path().join(format!("{kept_id}.vsock")), b"").unwrap();
        // Foreign (non-uuid) residents must never be touched.
        std::fs::create_dir_all(tmp.path().join("chunk-cache")).unwrap();
        std::fs::create_dir_all(tmp.path().join("bindings")).unwrap();
        std::fs::write(tmp.path().join("bindings/keep.json"), b"{}").unwrap();
        // ADR 0112: leaked swap backings. Design is unlink-after-attach,
        // so ANY .img at startup is a leak — a dead sandbox's (aborted
        // create) and a LIVE one's (failed unlink; FC holds the fd, so
        // removal just completes the unlink it was owed). A non-uuid
        // .img is foreign and stays.
        let swap_dir = tmp.path().join("swap");
        std::fs::create_dir_all(&swap_dir).unwrap();
        std::fs::write(swap_dir.join(format!("{dead_sock_id}.img")), b"x").unwrap();
        std::fs::write(swap_dir.join(format!("{live_id}.img")), b"x").unwrap();
        std::fs::write(swap_dir.join("not-a-uuid.img"), b"x").unwrap();
        // The canonical `.swap` symlink entries are the dead_ids loop's
        // jurisdiction, not the .img pass's.
        std::fs::write(swap_dir.join(format!("{live_id}.swap")), b"link").unwrap();

        let live = std::collections::HashSet::from([live_id]);
        let report = sweep_dead_sandbox_residue(tmp.path(), &live).unwrap();

        assert_eq!(report.jail_dirs, 1);
        assert_eq!(report.vsock_files, 2);
        assert_eq!(report.canonical_entries, 1);
        assert_eq!(
            report.swap_backings, 2,
            "dead AND live leaked .img reclaimed"
        );
        assert!(!dead_dir.exists(), "dead jail dir removed");
        assert!(
            !tmp.path().join(format!("{dead_sock_id}.vsock")).exists()
                && !tmp
                    .path()
                    .join(format!("{dead_sock_id}.vsock_1026"))
                    .exists(),
            "dead vsock sockets removed"
        );
        assert!(!canonical[0].exists(), "dead canonical entry removed");
        assert!(
            tmp.path().join(live_id.to_string()).exists()
                && tmp.path().join(format!("{live_id}.vsock")).exists(),
            "live sandbox entries untouched"
        );
        assert!(
            tmp.path().join("chunk-cache").exists()
                && tmp.path().join("bindings/keep.json").exists(),
            "non-uuid residents untouched"
        );
        assert!(
            kept_dir.exists() && tmp.path().join(format!("{kept_id}.vsock")).exists(),
            "a manifest-bearing dir and its sockets are the reattach \
             pass's jurisdiction, never the sweep's"
        );
        assert!(
            !swap_dir.join(format!("{dead_sock_id}.img")).exists()
                && !swap_dir.join(format!("{live_id}.img")).exists(),
            "leaked swap backings reclaimed regardless of liveness \
             (a live FC holds the fd — unlink-after-attach's contract)"
        );
        assert!(
            swap_dir.join("not-a-uuid.img").exists(),
            "foreign .img untouched"
        );
        assert!(
            swap_dir.join(format!("{live_id}.swap")).exists(),
            "live canonical .swap entry untouched by the .img pass"
        );
    }
}
