//! In-guest browser (Xvfb + chromium + x11vnc) lifecycle (ADR 0064).
//!
//! Mirrors [`crate::shell`]: lazy spawn on the first
//! [`StartBrowser`][crate::proto::WireRequest::StartBrowser], a real TCP
//! probe to `127.0.0.1:5900` before replying so the host's `proxy_vnc` dial
//! finds a listener, and respawn only if the prior launcher exited. The
//! launcher (`engram-browser`, symlinked onto PATH by the `browser` bundle's
//! activation — see ADR 0064 P0.1/P0.2) brings up the whole stack (Xvfb +
//! openbox + chromium + x11vnc) in its own process group via `setsid`, so
//! [`stop_browser`] reaps it with a single `killpg`.
//!
//! Like the shell, the state is process-wide (one browser stack per VM) behind
//! a tokio `Mutex` so concurrent `StartBrowser` calls serialize on the spawn
//! decision rather than racing each other into two launcher processes.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Default RFB port the in-VM x11vnc binds. The host's `proxy_vnc` uses the
/// matching `engram_host_agent::proxy_vnc::VNC_PORT` (Task P1.2).
pub const DEFAULT_VNC_PORT: u16 = 5900;

/// Launcher symlinked onto PATH by the `browser` bundle (ADR 0064). Other
/// images that bake the launcher into a different prefix can override the
/// resolved path via `ENGRAM_BROWSER_BIN` in the agent environment.
const DEFAULT_BROWSER_LAUNCHER: &str = "engram-browser";

/// How long to wait for x11vnc to accept its first TCP connection before
/// giving up. The stack (Xvfb → openbox → chromium → x11vnc) takes longer to
/// come up than ttyd, so this deadline is more generous than the shell's.
const READY_DEADLINE: Duration = Duration::from_secs(20);

/// Initial probe interval after spawn. Doubles up to [`READY_PROBE_MAX`] on
/// each miss.
const READY_PROBE_START: Duration = Duration::from_millis(100);
const READY_PROBE_MAX: Duration = Duration::from_millis(800);

/// Per-agent process state. Lazy because agentd in dev/test environments (no
/// browser bundle activated, no requirement for a VNC tab) shouldn't even
/// allocate a Mutex unless the host asks for one.
static BROWSER: tokio::sync::OnceCell<Mutex<Option<BrowserHandle>>> =
    tokio::sync::OnceCell::const_new();

struct BrowserHandle {
    child: Child,
    port: u16,
}

async fn state() -> &'static Mutex<Option<BrowserHandle>> {
    BROWSER.get_or_init(|| async { Mutex::new(None) }).await
}

/// Outcome of a `start_browser` call. `spawned` distinguishes "the agent just
/// spawned the launcher" from "the stack was already up and we only re-probed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserOutcome {
    pub port: u16,
    pub spawned: bool,
}

/// Ensure the browser stack is running and x11vnc is accepting on `port`.
///
/// On the first call this spawns the launcher at `ENGRAM_BROWSER_BIN` (or
/// `engram-browser` on PATH by default) in its own process group; on
/// subsequent calls it checks the prior handle is still alive and the port
/// still accepts — restarting only on failure. Either way the agent only
/// returns once a fresh TCP connect to the loopback `port` succeeds, so the
/// host can dial x11vnc with confidence right after this returns.
///
/// `session_env` is the durable session environment (image `[env]` + secrets +
/// session id) the host carried in on `SpawnHarness`. It's applied to the
/// launcher so chromium starts in the same environment the harness and the
/// shell see (proxy vars, toolchain PATH, …). Only the fresh-spawn path uses
/// it; a re-probe of an already-running stack leaves the existing process
/// untouched.
pub async fn start_browser(
    port: u16,
    session_env: HashMap<String, String>,
) -> io::Result<BrowserOutcome> {
    let mut guard = state().await.lock().await;

    // Path 0: the port is already accepting — either this agentd spawned the
    // stack on a prior call, or something restored it. Don't try to spawn (a
    // second x11vnc would fail with EADDRINUSE); report `spawned = false`.
    if probe_ready(port).await.is_ok() {
        return Ok(BrowserOutcome {
            port,
            spawned: false,
        });
    }

    // Path 1: we have a child handle but the port isn't accepting (Path 0
    // already ruled out a live listener). Whether the launcher exited, is
    // wedged, or was asked to move ports, drop the stale handle (its
    // `kill_on_drop` reaps the corpse) and fall through to a fresh spawn.
    if let Some(handle) = guard.as_mut() {
        if handle.port != port {
            tracing::info!(
                old = handle.port,
                new = port,
                "browser port change requested; restarting"
            );
        } else {
            match handle.child.try_wait() {
                Ok(None) => tracing::warn!(
                    port,
                    "browser launcher still alive but port not accepting; restarting"
                ),
                Ok(Some(status)) => {
                    tracing::warn!(port, exit = ?status, "browser launcher exited; restarting")
                }
                Err(e) => tracing::warn!(port, error = %e, "browser try_wait failed; restarting"),
            }
        }
        let _ = guard.take();
    }

    let bin = std::env::var("ENGRAM_BROWSER_BIN")
        .unwrap_or_else(|_| DEFAULT_BROWSER_LAUNCHER.to_string());
    tracing::info!(%bin, port, "spawning browser stack");

    let mut cmd = Command::new(&bin);
    cmd.env("ENGRAM_BROWSER_VNC_PORT", port.to_string())
        // The session env (proxy, secrets, toolchain PATH) on top of agentd's
        // inherited boot env; chromium and the rest of the stack inherit it.
        .envs(&session_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // agentd can exit cleanly; the launcher shouldn't survive it.
        // kill_on_drop sends SIGKILL to the leader when the handle drops.
        .kill_on_drop(true);
    // Own process group so stop_browser can killpg the whole stack (the
    // launcher re-parents the children it spawns under this leader).
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn browser launcher ({bin}): {e}")))?;

    *guard = Some(BrowserHandle { child, port });

    // Drop the guard while waiting so other StartBrowser callers see the new
    // handle immediately and don't serialize behind our probe loop.
    drop(guard);

    wait_until_ready(port).await?;

    Ok(BrowserOutcome {
        port,
        spawned: true,
    })
}

/// Tear down the browser stack: `killpg` the launcher's process group
/// (SIGTERM) so Xvfb/openbox/chromium/x11vnc all reap together, then a SIGKILL
/// backstop on the leader. Idempotent — a no-op when nothing is running.
pub async fn stop_browser() -> io::Result<()> {
    let mut guard = state().await.lock().await;
    if let Some(mut handle) = guard.take() {
        // `nix` is a Linux-only dependency of this crate (the guest is always
        // Linux); gate the killpg on linux specifically rather than `unix` so
        // the macOS cross-compile — where `cfg(unix)` is true but `nix` is
        // absent — still builds. The kill_on_drop / SIGKILL path below covers
        // the leader on every platform.
        #[cfg(target_os = "linux")]
        if let Some(pid) = handle.child.id() {
            // Negative pid == process group: the launcher is the group leader
            // (via process_group(0) at spawn). ESRCH (group already gone) is
            // fine — this is best-effort.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        let _ = handle.child.kill().await; // SIGKILL backstop on the leader
        let _ = handle.child.wait().await;
    }
    Ok(())
}

async fn wait_until_ready(port: u16) -> io::Result<()> {
    let deadline = Instant::now() + READY_DEADLINE;
    let mut backoff = READY_PROBE_START;
    let mut last_err: Option<io::Error> = None;
    while Instant::now() < deadline {
        match probe_ready(port).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(READY_PROBE_MAX);
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("x11vnc did not accept on 127.0.0.1:{port} within {READY_DEADLINE:?}"),
        )
    }))
}

async fn probe_ready(port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    // A successful connect is sufficient — x11vnc accepts immediately and
    // waits for the RFB handshake. We don't speak RFB here; closing cleanly
    // is fine.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Kill the cached browser handle, if any. Test-only — production relies on
/// `kill_on_drop` (and `stop_browser`) but `cargo test` shares the `OnceCell`
/// across cases in one process, so a test needs an explicit reset.
#[cfg(test)]
pub async fn shutdown_for_tests() -> io::Result<()> {
    stop_browser().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a fake launcher (a tiny python TCP listener that binds the VNC
    /// port and accepts in a loop) and assert the full "spawn → bind → probe →
    /// ready" sequence works, then that a second call sees the stack already
    /// up and reports `spawned = false`.
    ///
    /// `python3` is present in the `just test-linux` `rust:bookworm` image
    /// (Python 3.11). If it's somehow absent the test skips rather than
    /// failing spuriously — but the canonical lane has it.
    #[tokio::test]
    async fn start_browser_spawns_launcher_and_probes_port() {
        use std::os::unix::fs::PermissionsExt;

        if std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: python3 not available; browser test relies on it for a fake launcher");
            return;
        }

        // Pick an unused localhost port by binding briefly then dropping, so
        // the fake launcher can rebind it.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // Fake launcher: bind the VNC port and accept connections forever so
        // both agentd's internal probe AND the test's re-probe succeed (a
        // backlog-1 listen that never accepts would refuse the second connect).
        let script = format!(
            "#!/bin/sh\nexec python3 -c \"import socket\n\
             s=socket.socket()\n\
             s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n\
             s.bind(('127.0.0.1',{port}))\n\
             s.listen(16)\n\
             while True:\n    c,_=s.accept(); c.close()\"\n"
        );
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("engram-browser");
        std::fs::write(&launcher, script).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Scope the env var so we don't pollute sibling tests.
        let prev = std::env::var("ENGRAM_BROWSER_BIN").ok();
        std::env::set_var("ENGRAM_BROWSER_BIN", &launcher);
        // Ensure no stale handle from a prior run leaks in.
        let _ = shutdown_for_tests().await;

        let out = start_browser(port, HashMap::new()).await;

        // Second call should observe the listener already up (spawned=false)
        // — but only attempt it if the first succeeded.
        let again = if out.is_ok() {
            Some(start_browser(port, HashMap::new()).await)
        } else {
            None
        };

        // Always tear down + restore env before asserting so a panic can't
        // leak the fake launcher into a sibling test.
        let _ = shutdown_for_tests().await;
        if let Some(p) = prev {
            std::env::set_var("ENGRAM_BROWSER_BIN", p);
        } else {
            std::env::remove_var("ENGRAM_BROWSER_BIN");
        }

        let out = out.expect("first start_browser should spawn + probe ready");
        assert_eq!(out.port, port);
        assert!(out.spawned, "first call should report spawned = true");

        let again = again.unwrap().expect("re-probe should succeed");
        assert_eq!(again.port, port);
        assert!(
            !again.spawned,
            "second call should see the stack already up (spawned = false)"
        );
    }
}
