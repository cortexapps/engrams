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
//! [`crate::cacerts`] install paths — 2026-07 core-ops fold: the
//! handler installs the CA (carried on this same `SpawnHarness`
//! frame) before calling into [`HarnessSupervisor::spawn`], so those
//! paths are always populated by the time the child execs.
//!
//! Concurrency contract: at most one spawn-in-flight per agent.
//! Concurrent SpawnHarness calls serialise on the inner mutex;
//! second caller sees the new child after the first finishes its
//! kill+respawn.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, RwLock};

use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::proto::SpawnHarnessRequest;

/// Where the harness child's stdout/stderr land in the guest.
/// Overridable via `ENGRAM_HARNESS_LOG` (tests point it at a tempdir).
///
/// The harness must NEVER inherit agentd's stdio: agentd runs as pid 1
/// with fds 0/1/2 on `/dev/console`, and after an FC snapshot restore
/// nothing drains the emulated serial port — once the TTY output buffer
/// fills, the next `write(2)` to the console blocks *forever*. A harness
/// whose `tracing` output went to the console deadlocked mid-log-line in
/// prod (session 5665bdd3, 2026-06-03): the prompt was never processed
/// and the session sat "thinking" indefinitely. File writes can't block
/// that way, and the log stays readable in-guest via `/exec`.
const HARNESS_LOG_PATH: &str = "/var/log/engram/harness.log";

/// stdout/stderr `Stdio` pair for the harness child: append handles on
/// [`HARNESS_LOG_PATH`]. Falls back to `Stdio::null()` if the log file
/// can't be opened — losing logs is acceptable; inheriting the blocking
/// console is not.
fn harness_log_stdio() -> (Stdio, Stdio) {
    let path = std::env::var("ENGRAM_HARNESS_LOG").unwrap_or_else(|_| HARNESS_LOG_PATH.to_string());
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|f| f.try_clone().map(|c| (f, c)));
    match opened {
        Ok((out, err)) => (Stdio::from(out), Stdio::from(err)),
        Err(e) => {
            tracing::warn!(error = %e, path = %path, "harness log open failed; using null stdio");
            (Stdio::null(), Stdio::null())
        }
    }
}

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
    /// The session working directory, recorded from the reserved
    /// `HARNESS_CWD_ENV` key on each `SpawnHarness` frame (same lifecycle
    /// as `session_env`, including the empty-argv readiness probe). The
    /// IDE (`StartIde`, ADR 0085) reads it back via
    /// [`Self::session_workdir`] so code-server opens on the workspace,
    /// not agentd's cwd (`/`). `None` when the frame carried no cwd
    /// (dev_vm / workdir-less images).
    session_workdir: RwLock<Option<String>>,
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
            session_workdir: RwLock::new(None),
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

    /// The session working directory most recently delivered by the host
    /// on a `SpawnHarness` frame (the reserved `HARNESS_CWD_ENV` key).
    /// `None` until the first bind, or when the session has no workdir.
    pub fn session_workdir(&self) -> Option<String> {
        self.session_workdir
            .read()
            .expect("session_workdir lock poisoned")
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
        // ADR 0085: record the session workdir alongside — the IDE
        // (`StartIde`) opens code-server on it, and like the env it must
        // land even on the readiness probe (dev_vm spawns no harness).
        *self
            .session_workdir
            .write()
            .expect("session_workdir lock poisoned") =
            req.env.get(engram_harness_proto::HARNESS_CWD_ENV).cloned();

        // ADR 0035 §3: re-mount the bundle mounts BEFORE activation. On a
        // fresh create the host may have patch_drive'd an aux bundle to a
        // newer generation while load-paused; the guest's captured squashfs
        // superblock predates the swap, so reads (including activate()'s
        // probes below) would EIO until a umount/mount re-parses the device.
        // EBUSY = resume path (drive unswapped, mount still correct) — kept.
        crate::remount::remount_and_log();

        // ADR 0027: wire whatever RO bundles the init shim mounted (the
        // skills / playwright squashfs) into the harness's skill
        // discovery paths, gated by the session env. Done before the
        // readiness-probe early return too, so a dev_vm session's exec/shell
        // also see `share-file` et al. `root = /` — we're in the guest.
        // Best-effort: this never returns an error, and we don't let a
        // failure here block the spawn.
        //
        // The forge broker token rides `req.env` (per-spawn extras), NOT
        // `session_env` (coord keeps it out of the cached env), so gate on
        // the union — otherwise the forge-gated askpass/gitconfig (and any
        // `requires_env` skill) would never wire.
        let mut gate_env = req.session_env.clone();
        gate_env.extend(req.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let report = engram_session_bundles::activate(std::path::Path::new("/"), &gate_env);
        if !report.activated.is_empty() {
            tracing::info!(activated = ?report.activated, "ADR 0027: activated session bundles");
        }
        for w in &report.warnings {
            tracing::warn!(warning = %w, "ADR 0027: session bundle activation");
        }

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
        // CaCertInstaller writes (ADR 0021 P1.1). The handler installs
        // the CA (2026-07 fold: carried on this SpawnHarness frame)
        // before calling into `spawn`, so these files exist by the
        // time the harness child starts. NODE_EXTRA_CA_CERTS
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
            // Capture the pid BEFORE `try_wait`/`wait`: once either of those
            // observes the child has exited, tokio clears `Child::id()` to
            // `None` (the handle moves to its `Done` state), so `prev.id()`
            // called after the fact would silently return `None` here.
            let prev_pid = prev.id();
            // ADR 0045 C1: REATTACH, don't respawn, when the previous
            // harness is still ALIVE. A live-teleported (or mid-run
            // resumed) guest arrives with its harness running inside
            // the moved memory image — the post-move SpawnHarness from
            // `finish_resume_to_active` used to KILL it here, silently
            // destroying the in-flight run the teleport had just
            // preserved losslessly. The harness's event pipe into
            // agentd is intact (both ends moved together), and the
            // host re-dials agentd's stream regardless, so the only
            // correct action for a live child is: return its pid and
            // leave it alone. An EXITED child (the idle-resume shape:
            // the run completed before capture) is reaped and a fresh
            // harness spawns — the pre-existing resume semantics.
            match prev.try_wait() {
                Ok(None) => {
                    let pid = prev_pid;
                    // Issue #569: re-assert tracking on every reattach —
                    // idempotent (a `HashSet` insert), and self-healing if
                    // this pid's registration were ever lost (e.g. a future
                    // bug elsewhere untracks too eagerly). A live harness
                    // child must NEVER be visible to the zombie reaper.
                    if let Some(pid) = pid {
                        crate::reaper::track(pid);
                    }
                    tracing::info!(
                        pid = ?pid,
                        "harness still running (live move / mid-run resume); reattaching, not respawning",
                    );
                    // ADR 0045 C1: nudge the live harness to re-dial the
                    // host NOW. A snapshot restore rebuilds the vsock
                    // device, but the harness's established connection
                    // doesn't EOF — its read blocks forever, the
                    // reconnect loop never wakes, and the destination
                    // host never sees a harness attach (prompts 500
                    // "sandbox not found" while exec works — prod
                    // canaries 1f64052e / 51d51740). This SpawnHarness
                    // is the one signal that fires exactly at restore
                    // time, so deliver SIGUSR1 = "drop the connection
                    // and re-dial" (handled in engram-harness-claude's
                    // connection loop; harnesses without a handler are
                    // respawned by the next SpawnHarness anyway).
                    #[cfg(target_os = "linux")]
                    if let Some(pid) = pid {
                        if let Err(e) = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGUSR1,
                        ) {
                            tracing::warn!(
                                pid,
                                error = %e,
                                "reconnect nudge (SIGUSR1) failed; harness must \
                                 detect the dead connection on its own",
                            );
                        }
                    }
                    guard.current_child = Some(prev);
                    return Ok(pid);
                }
                Ok(Some(status)) => {
                    tracing::info!(
                        pid = ?prev_pid,
                        ?status,
                        "previous harness exited; reaping and spawning fresh",
                    );
                    // `try_wait` already reaped it (the kernel drops the
                    // zombie the instant its exit status is retrieved, even
                    // via WNOHANG) — untrack so the registry doesn't grow
                    // unbounded across a long-lived agent's many resumes.
                    if let Some(pid) = prev_pid {
                        crate::reaper::untrack(pid);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "harness liveness probe failed; respawning");
                    let _ = prev.kill().await;
                    let _ = prev.wait().await;
                    if let Some(pid) = prev_pid {
                        crate::reaper::untrack(pid);
                    }
                }
            }
        }

        // Detach the child from agentd's stdio (= /dev/console, which
        // blocks forever once full in a restored VM — see
        // `HARNESS_LOG_PATH`). stdout/stderr append to the in-guest
        // harness log; stdin is closed (the harness takes input over
        // vsock, never the console).
        let (h_out, h_err) = harness_log_stdio();
        cmd.stdin(Stdio::null()).stdout(h_out).stderr(h_err);
        // Issue #569: `spawn_tracked` registers the pid atomically with the
        // spawn — this child is held in `guard.current_child` without being
        // polled between `SpawnHarness` calls, so tokio's own background
        // reaper does NOT collect it if it exits early; only the
        // supervisor's own `try_wait()` above may observe (and reap) it, and
        // the reaper must never race that.
        let child = crate::reaper::spawn_tracked(&mut cmd)
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
                host_ca_pem: None,
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
                host_ca_pem: None,
            })
            .await
            .unwrap();
        assert_eq!(pid, None);
    }

    /// ADR 0045 C1: SpawnHarness against a LIVE child reattaches
    /// (same pid, child untouched); against an EXITED child it reaps
    /// and respawns. The live-teleport handshake depends on the first
    /// arm — the old kill-and-respawn destroyed mid-run harnesses the
    /// move had just preserved.
    #[tokio::test]
    async fn spawn_reattaches_live_child_and_respawns_exited() {
        let sup = HarnessSupervisor::new();
        let long = SpawnHarnessRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 300".into()],
            env: HashMap::new(),
            session_env: HashMap::new(),
            host_ca_pem: None,
        };
        let pid1 = sup.spawn(long.clone()).await.unwrap().expect("pid");
        // The teleport-handshake shape: SpawnHarness while running.
        let pid2 = sup.spawn(long.clone()).await.unwrap().expect("pid");
        assert_eq!(pid1, pid2, "live child must be reattached, not respawned");
        // The child is genuinely still alive.
        assert!(
            std::process::Command::new("kill")
                .args(["-0", &pid1.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            "reattach must not have killed the child"
        );

        // Exited child: reap + fresh spawn (the idle-resume shape).
        let short = SpawnHarnessRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            env: HashMap::new(),
            session_env: HashMap::new(),
            host_ca_pem: None,
        };
        let sup2 = HarnessSupervisor::new();
        let pid3 = sup2.spawn(short.clone()).await.unwrap().expect("pid");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pid4 = sup2.spawn(short).await.unwrap().expect("pid");
        assert_ne!(pid3, pid4, "exited child must be reaped and respawned");
    }

    #[tokio::test]
    async fn spawn_real_child_returns_pid() {
        let sup = HarnessSupervisor::new();
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
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
                host_ca_pem: None,
            })
            .await
            .unwrap();
        let second = sup
            .spawn(SpawnHarnessRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
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
    async fn child_stdio_goes_to_harness_log_not_inherited() {
        // The harness must never inherit agentd's stdio (= /dev/console in
        // the guest, which blocks forever once the restored VM's TTY buffer
        // fills). Assert the child's stdout+stderr land in the harness log
        // file instead.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("harness.log");
        std::env::set_var("ENGRAM_HARNESS_LOG", &log_path);
        let sup = HarnessSupervisor::new();
        let pid = sup
            .spawn(SpawnHarnessRequest {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo out-line; echo err-line >&2".into(),
                ],
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: None,
            })
            .await
            .unwrap();
        assert!(pid.is_some());
        // Reap the child so the writes are flushed before we read.
        let mut guard = sup.inner.lock().await;
        if let Some(mut c) = guard.current_child.take() {
            let _ = c.wait().await;
        }
        drop(guard);
        std::env::remove_var("ENGRAM_HARNESS_LOG");
        let logged = std::fs::read_to_string(&log_path).unwrap();
        assert!(logged.contains("out-line"), "stdout missing: {logged:?}");
        assert!(logged.contains("err-line"), "stderr missing: {logged:?}");
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
                host_ca_pem: None,
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
