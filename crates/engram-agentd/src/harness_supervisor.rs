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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::proto::SpawnHarnessRequest;

/// Owns the harness child process handle. Cheap to clone via `Arc`;
/// shared across all `serve_connection` tasks so a fresh
/// `SpawnHarness` from any connection finds the current child.
pub struct HarnessSupervisor {
    inner: Mutex<Inner>,
    /// The durable session environment, set from each `SpawnHarness`
    /// frame (including the dev_vm readiness probe) and applied as the
    /// base env for every process agentd spawns — the harness here,
    /// plus `/exec` commands and the interactive shell, which read it
    /// back via [`Self::session_env`]. One agentd serves exactly one
    /// session, so this single shared map *is* the session's env.
    session_env: RwLock<HashMap<String, String>>,
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
            session_env: RwLock::new(HashMap::new()),
        })
    }

    /// The durable session env most recently delivered by the host on a
    /// `SpawnHarness` frame. Cloned per reader (the map is small — image
    /// env plus a handful of secrets). Empty until the first bind.
    pub fn session_env(&self) -> HashMap<String, String> {
        self.session_env
            .read()
            .expect("session_env lock poisoned")
            .clone()
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
        // Record the session env before the readiness-probe early return:
        // dev_vm sessions deliver it on an empty-argv probe and never spawn
        // a harness, but their exec/shell processes still inherit it.
        *self.session_env.write().expect("session_env lock poisoned") = req.session_env.clone();
        if req.argv.is_empty() {
            tracing::debug!("SpawnHarness with empty argv — readiness probe; no spawn");
            return Ok(None);
        }
        // Sync the guest clock to the host before spawning the harness, so
        // the agent (and the git/cargo children it drives) start on a
        // correct clock right after a resume. The periodic tick keeps a
        // long-running harness corrected across later resumes.
        crate::clock::sync_now();
        let argv0 = req.argv[0].clone();

        // ADR 0021 P1.4: the harness binary is baked into the rootfs
        // at /opt/engram/harness/ (per the [harness] block in the
        // image manifest). No drive mount, no NBD page-in on the
        // spawn path; just exec the argv the host computed from the
        // manifest's resolved `exec` + `args`.
        let mut cmd = Command::new(&argv0);
        cmd.args(&req.argv[1..]);
        // Base: the durable session env (image `[env]`, secrets, PATH,
        // session id) — the very same env `/exec` and the interactive
        // shell inherit, so the harness runs in an identical environment.
        for (k, v) in &req.session_env {
            cmd.env(k, v);
        }
        // Harness-only extras layered on top (forge broker token, initial
        // prompt, dial address). The working directory rides in on a
        // reserved env key (see `engram_harness_proto::HARNESS_CWD_ENV`
        // for why it's not a wire-struct field). Pull it out so it sets
        // `current_dir` rather than leaking into the child's environment.
        let harness_cwd = req.env.get(engram_harness_proto::HARNESS_CWD_ENV);
        for (k, v) in &req.env {
            if k == engram_harness_proto::HARNESS_CWD_ENV {
                continue;
            }
            cmd.env(k, v);
        }
        if let Some(cwd) = harness_cwd {
            cmd.current_dir(cwd);
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
    async fn empty_argv_probe_records_session_env_without_spawning() {
        let sup = HarnessSupervisor::new();
        let mut session_env = HashMap::new();
        session_env.insert("RUSTC_WRAPPER".to_string(), "sccache".to_string());
        // A readiness probe spawns no child...
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![],
                env: HashMap::new(),
                session_env: session_env.clone(),
            })
            .await
            .unwrap();
        assert_eq!(pid, None);
        // ...but it DOES record the session env, so a dev_vm session's
        // exec/shell inherit it even though no harness ever spawns.
        assert_eq!(sup.session_env(), session_env);
        // A subsequent probe is still a no-op (no child to clean up).
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![],
                env: HashMap::new(),
                session_env: HashMap::new(),
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
                session_env: HashMap::new(),
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
                session_env: HashMap::new(),
            })
            .await
            .unwrap();
        let second = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                session_env: HashMap::new(),
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

    #[tokio::test]
    async fn harness_cwd_env_sets_working_dir_and_is_stripped_from_child() {
        let dir = tempfile::tempdir().unwrap();
        let sup = HarnessSupervisor::new();
        // cwd rides in on the reserved key; a normal var rides alongside
        // it. The child writes `pwd`/`env` into files — relative to its
        // cwd, so they land in `dir` iff current_dir was honored.
        let mut env = HashMap::new();
        env.insert(
            engram_harness_proto::HARNESS_CWD_ENV.to_string(),
            dir.path().to_string_lossy().into_owned(),
        );
        env.insert("HARNESS_CWD_MARKER".to_string(), "yes".to_string());
        // The durable session env is applied as the base; the harness
        // child should see it alongside its own extras.
        let session_env =
            HashMap::from_iter([("SESSION_ENV_MARKER".to_string(), "base".to_string())]);
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "pwd > pwd.out; printenv > env.out".into(),
                ],
                env,
                session_env,
            })
            .await
            .unwrap();
        assert!(pid.is_some());
        // Let the (fast) child run to completion, then inspect.
        {
            let mut guard = sup.inner.lock().await;
            let mut child = guard.current_child.take().expect("child handle");
            drop(guard);
            child.wait().await.unwrap();
        }
        let pwd = std::fs::read_to_string(dir.path().join("pwd.out")).unwrap();
        // Symlinked temp roots (e.g. macOS /var → /private/var) mean we
        // compare canonical forms, not raw strings.
        assert_eq!(
            std::fs::canonicalize(pwd.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap(),
            "harness should have started in the cwd from HARNESS_CWD_ENV",
        );
        let child_env = std::fs::read_to_string(dir.path().join("env.out")).unwrap();
        assert!(
            child_env.lines().any(|l| l == "HARNESS_CWD_MARKER=yes"),
            "ordinary env vars still reach the child",
        );
        assert!(
            child_env.lines().any(|l| l == "SESSION_ENV_MARKER=base"),
            "the durable session env reaches the harness child as the base",
        );
        assert!(
            !child_env
                .lines()
                .any(|l| l.starts_with(&format!("{}=", engram_harness_proto::HARNESS_CWD_ENV))),
            "the reserved cwd key must be consumed, not leaked into the child env",
        );
    }
}
