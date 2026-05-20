//! In-guest ttyd lifecycle.
//!
//! Pre-M1.16 the in-VM init script started ttyd in the background at
//! boot, so it was running when the snapshot was captured. On warm
//! restore, the kernel was supposed to bring the TCP listen socket
//! back up automatically. In practice this raced — VMs that had been
//! warm-restored for >10 minutes were still failing host→guest dials
//! with `Connection refused`, the in-browser shell timed out, and
//! the user got an "abnormal close" with no useful signal (prod
//! session 73fe33a3 on 2026-05-20).
//!
//! The fix here is to make shell start *lazy*: ttyd is spawned by
//! agentd the first time the host calls
//! [`WireRequest::StartShell`][crate::proto::WireRequest::StartShell],
//! verified to be accepting on its port (via a real TCP connect from
//! inside the guest), and only THEN replied with
//! [`WireResponse::ShellReady`][crate::proto::WireResponse::ShellReady].
//! Subsequent StartShell calls re-probe the port and respawn only if
//! the prior process exited. This means the host's `proxy_shell` path
//! never has to retry-with-backoff against an uncertain TCP state —
//! ttyd is provably bound when the gRPC tunnel opens.
//!
//! The state is process-wide (one ttyd per VM); we keep it behind a
//! tokio Mutex so concurrent StartShell calls serialize on the spawn
//! decision rather than racing each other into two ttyd processes.

use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Default port the in-VM ttyd listens on. The host's `proxy_shell`
/// uses [`engram_sandbox_firecracker::proxy_shell::TTYD_PORT`] as the
/// matching constant.
pub const DEFAULT_TTYD_PORT: u16 = 7681;

/// Where the ttyd binary lives inside the canonical demo image.
/// Other images that bake ttyd into a different prefix can override
/// via `ENGRAM_TTYD_BIN` in the agent environment.
const DEFAULT_TTYD_PATH: &str = "/usr/local/bin/ttyd";

/// How long to wait for ttyd to accept its first TCP connection
/// before giving up. ttyd binds in <100ms under normal load; this is
/// generous to absorb a cold-cache page-in or a kernel scheduler hit
/// on a small VM.
const READY_DEADLINE: Duration = Duration::from_secs(10);

/// Initial probe interval after spawn. Doubles up to READY_PROBE_MAX
/// on each miss.
const READY_PROBE_START: Duration = Duration::from_millis(50);
const READY_PROBE_MAX: Duration = Duration::from_millis(400);

/// Per-agent process state. Lazy because agentd in dev/test environments
/// (no ttyd binary on PATH, no requirement for a shell tab) shouldn't
/// even allocate a Mutex unless the host asks for one.
static SHELL: tokio::sync::OnceCell<Mutex<Option<ShellHandle>>> =
    tokio::sync::OnceCell::const_new();

struct ShellHandle {
    child: Child,
    port: u16,
}

async fn state() -> &'static Mutex<Option<ShellHandle>> {
    SHELL
        .get_or_init(|| async { Mutex::new(None) })
        .await
}

/// Outcome of a `start_shell` call. `spawned` distinguishes "the
/// agent just spawned ttyd" from "ttyd was already running and we
/// only re-probed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellOutcome {
    pub port: u16,
    pub spawned: bool,
}

/// Ensure ttyd is running on `port` and accepting connections. On
/// first call this spawns the binary at `ENGRAM_TTYD_BIN` (or
/// `/usr/local/bin/ttyd` by default); on subsequent calls it checks
/// the prior handle is still alive and the port still accepts —
/// restarting only on failure.
pub async fn start_shell(port: u16) -> io::Result<ShellOutcome> {
    let mut guard = state().await.lock().await;

    // Path 1: we already have a child. Verify it's still running AND
    // the port still accepts. Either failure → drop the handle and
    // fall through to the spawn path.
    if let Some(handle) = guard.as_mut() {
        if handle.port == port {
            match handle.child.try_wait() {
                Ok(None) => {
                    if probe_ready(port).await.is_ok() {
                        return Ok(ShellOutcome {
                            port,
                            spawned: false,
                        });
                    }
                    tracing::warn!(
                        port,
                        "ttyd child still alive but port not accepting; restarting"
                    );
                }
                Ok(Some(status)) => {
                    tracing::warn!(
                        port,
                        exit = ?status,
                        "ttyd child exited; restarting"
                    );
                }
                Err(e) => {
                    tracing::warn!(port, error = %e, "ttyd try_wait failed; restarting");
                }
            }
            // Fall through to spawn — drop the stale handle first so
            // the Drop's kill_on_drop reaps the corpse.
            let _ = guard.take();
        } else {
            // Port mismatch — kill the old one and respawn on the
            // new port. (This shouldn't happen in practice; the
            // host has no reason to flip the port mid-session.)
            tracing::info!(
                old = handle.port,
                new = port,
                "ttyd port change requested; restarting"
            );
            let _ = guard.take();
        }
    }

    let bin = std::env::var("ENGRAM_TTYD_BIN").unwrap_or_else(|_| DEFAULT_TTYD_PATH.to_string());
    let shell_bin = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    tracing::info!(%bin, %shell_bin, port, "spawning ttyd");

    let child = Command::new(&bin)
        // -W = read-write terminal (default is read-only).
        // -p <port> = bind port.
        // Final positional arg = command to exec on connect.
        .args(["-W", "-p", &port.to_string(), &shell_bin])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The agent itself can exit cleanly; ttyd shouldn't survive
        // it. kill_on_drop ensures the Child handle's drop sends
        // SIGKILL.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("spawn ttyd ({bin}): {e}"),
            )
        })?;

    *guard = Some(ShellHandle { child, port });

    // Probe loop. Drop the guard while waiting so other StartShell
    // callers see the new handle as soon as it lands and we don't
    // serialize them behind our probe.
    drop(guard);

    wait_until_ready(port).await?;

    Ok(ShellOutcome {
        port,
        spawned: true,
    })
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
            format!("ttyd did not accept on 127.0.0.1:{port} within {READY_DEADLINE:?}"),
        )
    }))
}

async fn probe_ready(port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    // A successful connect is sufficient — ttyd accepts immediately
    // and waits for the WebSocket upgrade. We don't speak HTTP here;
    // closing the stream cleanly is fine.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Kill the cached ttyd handle, if any. Test-only — production code
/// relies on `kill_on_drop` when agentd itself exits, but unit tests
/// share a OnceCell across `#[tokio::test]` cases and need an
/// explicit reset.
#[cfg(test)]
pub async fn shutdown_for_tests() -> io::Result<()> {
    let mut guard = state().await.lock().await;
    if let Some(mut handle) = guard.take() {
        let _ = handle.child.kill().await;
        let _ = handle.child.wait().await;
    }
    Ok(())
}
