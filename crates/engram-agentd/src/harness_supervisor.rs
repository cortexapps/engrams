//! In-VM harness child supervisor.
//!
//! Holds the most recent harness child handle and provides
//! idempotent kill+respawn semantics so the host can re-deliver a
//! `SpawnHarness` frame after FC snapshot/restore invalidates the
//! previous adapter's connection.
//!
//! ADR 0021 P1.4: the harness binary lives in the rootfs at the
//! manifest-declared `[harness] exec` path (built-in templates plant
//! it at `/opt/engram/harness/harness`; custom templates wherever the
//! author COPY'd it). There's no drive mount, no NBD page-in on the
//! spawn hot path; the supervisor just exec's argv. The egress-proxy
//! CA reaches the child via env vars pointing at the canonical
//! [`crate::cacerts`] install paths — the host's `InstallHostCa` RPC
//! ran during `start_agent`, before the SpawnHarness call landed.
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
    /// ADR 0019 0c: `agentd.spawn_harness` span — the in-guest tail
    /// of the host's `fc.spawn_harness`, covering just the child
    /// fork/exec since ADR 0021 P1.4 retired the harness-drive
    /// mount (the binary lives in the rootfs now).
    #[tracing::instrument(name = "agentd.spawn_harness", skip_all)]
    pub async fn spawn(&self, req: SpawnHarnessRequest) -> std::io::Result<Option<u32>> {
        if req.argv.is_empty() {
            tracing::debug!("SpawnHarness with empty argv — readiness probe; no spawn");
            return Ok(None);
        }
        let argv0 = req.argv[0].clone();

        // ADR 0021 P1.4: the harness binary is baked into the rootfs
        // at /opt/engram/harness/ (per the [harness] block in the
        // image manifest). No drive mount, no NBD page-in on the
        // spawn path; just exec the argv the host computed from the
        // manifest's resolved `exec` + `args`.
        let mut cmd = Command::new(&argv0);
        cmd.args(&req.argv[1..]);
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        // Point TLS libraries at the canonical CA paths the
        // CaCertInstaller writes (ADR 0021 P1.1). The host called
        // InstallHostCa before SpawnHarness, so these files exist
        // by the time the harness child starts. NODE_EXTRA_CA_CERTS
        // wants the single engram cert file (Node doesn't read the
        // system bundle by default); the bundle env vars cover
        // OpenSSL / libcurl / Python requests.
        let bundle = crate::cacerts::CaCertPaths::default_linux();
        let bundle_path = bundle.bundle.to_string_lossy().into_owned();
        let extra_cert_path = bundle.extra_cert.to_string_lossy().into_owned();
        cmd.env("SSL_CERT_FILE", &bundle_path);
        cmd.env("CURL_CA_BUNDLE", &bundle_path);
        cmd.env("REQUESTS_CA_BUNDLE", &bundle_path);
        cmd.env("NODE_EXTRA_CA_CERTS", &extra_cert_path);

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
            })
            .await
            .unwrap();
        assert_eq!(pid, None);
        // Subsequent call still works (no child to clean up).
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![],
                env: HashMap::new(),
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
            })
            .await
            .unwrap();
        let second = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
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
