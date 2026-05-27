//! In-VM harness child supervisor.
//!
//! Holds the most recent harness child handle and provides
//! idempotent kill+respawn semantics so the host can re-deliver a
//! `SpawnHarness` frame after FC snapshot/restore invalidates the
//! previous adapter's connection. ADR 0014 M1.12 (option D) is
//! the warm-pool context: templates are harness-agnostic; the
//! per-session harness drive is hot-swapped via FC `PATCH /drives`
//! and this supervisor mounts it just before exec.
//!
//! Concurrency contract: at most one spawn-in-flight per agent.
//! Concurrent SpawnHarness calls serialise on the inner mutex;
//! second caller sees the new child after the first finishes its
//! kill+respawn.

use std::sync::Arc;

use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::proto::SpawnHarnessRequest;

/// Owns the harness child process handle. Cheap to clone via `Arc`;
/// shared across all `serve_connection` tasks so a fresh
/// `SpawnHarness` from any connection finds the current child.
pub struct HarnessSupervisor {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Most recent harness child. `None` until the first SpawnHarness;
    /// reset to None on graceful exit (we don't track that today, but
    /// it'd be the natural place to wire it).
    current_child: Option<Child>,
}

impl HarnessSupervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                current_child: None,
            }),
        })
    }

    /// Spawn (or respawn) the harness child described by `req`.
    /// Returns the new child's PID on success, or `None` when the
    /// call was a readiness probe (empty argv) — in which case we
    /// don't kill any in-flight child either, matching bootstrap's
    /// "ignore empty argv" branch.
    ///
    /// ADR 0019 0c: `agentd.spawn_harness` span — the in-guest tail of the
    /// host's `fc.spawn_harness`, covering the optional harness-drive mount
    /// (ext4, possibly NBD-backed → page-in) + the child fork/exec.
    #[tracing::instrument(name = "agentd.spawn_harness", skip_all)]
    pub async fn spawn(&self, req: SpawnHarnessRequest) -> std::io::Result<Option<u32>> {
        if req.argv.is_empty() {
            tracing::debug!("SpawnHarness with empty argv — readiness probe; no spawn");
            return Ok(None);
        }
        let argv0 = req.argv[0].clone();

        // Optional harness-drive mount. ext4 + read-only,
        // idempotent across re-entry.
        let mut harness_mounted_at: Option<String> = None;
        if let (Some(dev), Some(mount)) = (req.harness_dev.as_deref(), req.harness_mount.as_deref())
        {
            mount_harness(dev, mount)?;
            tracing::info!(dev = %dev, mount = %mount, "harness drive mounted");
            harness_mounted_at = Some(mount.to_string());
        }

        // Build the child Command before taking the lock — no need
        // to hold the lock during fs::read / fs::write inside
        // inject_egress_proxy_ca.
        let mut cmd = Command::new(&argv0);
        cmd.args(&req.argv[1..]);
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        if let Some(mount) = harness_mounted_at.as_deref() {
            if let Err(e) = inject_egress_proxy_ca(mount, &mut cmd) {
                tracing::warn!(
                    error = %e,
                    mount = %mount,
                    "egress-proxy CA setup failed; harness will not trust proxy-minted leaves",
                );
            }
        }

        let mut guard = self.inner.lock().await;
        if let Some(mut prev) = guard.current_child.take() {
            tracing::info!(
                pid = ?prev.id(),
                "respawn requested; killing previous harness child",
            );
            let _ = prev.kill().await;
            let _ = prev.wait().await;
        }

        let child = cmd
            .spawn()
            .map_err(|e| std::io::Error::new(e.kind(), format!("spawn {:?}: {e}", argv0)))?;
        let pid = child.id();
        tracing::info!(pid = ?pid, argv0 = %argv0, argc = req.argv.len(), "harness child running");
        guard.current_child = Some(child);
        Ok(pid)
    }
}

/// `mount(2)` wrapper for the harness device. ext4 + read-only,
/// idempotent (EBUSY → Ok).
#[cfg(target_os = "linux")]
fn mount_harness(dev: &str, mount_point: &str) -> std::io::Result<()> {
    use nix::mount::{mount, MsFlags};

    if let Err(e) = std::fs::create_dir_all(mount_point) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }

    match mount(
        Some(dev),
        mount_point,
        Some("ext4"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        Ok(()) => Ok(()),
        // Already mounted — idempotent on re-entry.
        Err(nix::errno::Errno::EBUSY) => Ok(()),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

/// Stub for non-Linux builds. Agentd only runs on Linux in practice
/// (vsock + virtio-blk are kernel features), but the crate compiles
/// on macOS via the `from_env` transport's stub layer so the workspace
/// cargo check stays clean.
#[cfg(not(target_os = "linux"))]
fn mount_harness(_dev: &str, _mount_point: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mount_harness is Linux-only",
    ))
}

/// Append the egress-proxy CA at `<harness_mount>/.engram-host/ca.pem`
/// to the system bundle and set the env-var family the harness
/// child inherits. No-op when the CA file is absent.
#[cfg(target_os = "linux")]
fn inject_egress_proxy_ca(harness_mount: &str, cmd: &mut Command) -> std::io::Result<()> {
    let ca_path = format!("{harness_mount}/.engram-host/ca.pem");
    if !std::path::Path::new(&ca_path).exists() {
        return Ok(());
    }
    let bundle = "/etc/ssl/certs/ca-certificates.crt";
    let ca_bytes = std::fs::read(&ca_path)?;
    let _ = std::fs::create_dir_all("/etc/ssl/certs");
    let mut bundle_bytes = std::fs::read(bundle).unwrap_or_default();
    if !bundle_bytes.ends_with(b"\n") && !bundle_bytes.is_empty() {
        bundle_bytes.push(b'\n');
    }
    bundle_bytes.extend_from_slice(&ca_bytes);
    std::fs::write(bundle, &bundle_bytes)?;

    cmd.env("SSL_CERT_FILE", bundle);
    cmd.env("CURL_CA_BUNDLE", bundle);
    cmd.env("REQUESTS_CA_BUNDLE", bundle);
    cmd.env("NODE_EXTRA_CA_CERTS", &ca_path);
    tracing::info!(ca = %ca_path, bundle, "egress-proxy CA installed into trust store");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn inject_egress_proxy_ca(_harness_mount: &str, _cmd: &mut Command) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn empty_argv_is_readiness_probe() {
        let sup = HarnessSupervisor::new();
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![],
                env: HashMap::new(),
                harness_dev: None,
                harness_mount: None,
            })
            .await
            .unwrap();
        assert_eq!(pid, None);
        // Subsequent call still works (no child to clean up).
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![],
                env: HashMap::new(),
                harness_dev: None,
                harness_mount: None,
            })
            .await
            .unwrap();
        assert_eq!(pid, None);
    }

    #[tokio::test]
    async fn spawn_real_child_returns_pid() {
        let sup = HarnessSupervisor::new();
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                harness_dev: None,
                harness_mount: None,
            })
            .await
            .unwrap();
        assert!(pid.is_some(), "expected a real PID for non-empty argv");
        // Clean up so the test process doesn't leak the sleep.
        let mut guard = sup.inner.lock().await;
        if let Some(mut c) = guard.current_child.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
    }

    #[tokio::test]
    async fn second_spawn_kills_first() {
        let sup = HarnessSupervisor::new();
        let _first = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                harness_dev: None,
                harness_mount: None,
            })
            .await
            .unwrap();
        let second = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                harness_dev: None,
                harness_mount: None,
            })
            .await
            .unwrap();
        assert!(second.is_some());
        // Clean up.
        let mut guard = sup.inner.lock().await;
        if let Some(mut c) = guard.current_child.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
    }
}
