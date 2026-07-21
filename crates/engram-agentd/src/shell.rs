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

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Default port the in-VM ttyd listens on. The host's `proxy_shell`
/// uses [`engram_sandbox_firecracker::proxy_shell::TTYD_PORT`] as the
/// matching constant.
pub const DEFAULT_TTYD_PORT: u16 = 7681;

/// Transitional fallback path for images that still bake ttyd (ADR 0080
/// §D retired that from the image contract — ttyd now rides the
/// `guest-tools` bundle slot and is resolved by [`resolve_ttyd_bin`]).
/// Images that bake ttyd into a different prefix can override via
/// `ENGRAM_TTYD_BIN` in the agent environment.
const DEFAULT_TTYD_PATH: &str = "/usr/local/bin/ttyd";

/// Where the init shim mounts the reserved dynamic slots (mirrors
/// `AuxRoDrive::slot_guest_mount`; same probing rationale as
/// `refresh::find_bundle_mount` — VZ compacts resolved drives onto
/// sequential mounts, so position can't be trusted).
const DYN_MOUNT_ROOT: &str = "/opt/engram/dyn";

/// Locate the ttyd binary to spawn (ADR 0080 §D). Order:
///
/// 1. `ENGRAM_TTYD_BIN` — explicit operator/image override.
/// 2. the `guest-tools` bundle mount — probe the dyn slots for a `ttyd`
///    file, exactly like `refresh::find_bundle_mount` probes for agentd.
///    This is the steady-state path: ttyd ships fleet-side, images don't
///    carry it.
/// 3. the legacy image-baked path, with a WARN — transitional images may
///    still bake ttyd; a fleet that stages no guest-tools bundle degrades
///    to it rather than losing the SHELL tab outright.
fn resolve_ttyd_bin() -> String {
    if let Ok(bin) = std::env::var("ENGRAM_TTYD_BIN") {
        return bin;
    }
    if let Some(found) = find_guest_tools_ttyd() {
        return found.to_string_lossy().into_owned();
    }
    tracing::warn!(
        fallback = DEFAULT_TTYD_PATH,
        "no guest-tools bundle mount carries ttyd under {DYN_MOUNT_ROOT}; \
         falling back to the image-baked path (stage bundle-guest-tools — \
         ADR 0080 removed ttyd from the image contract)"
    );
    DEFAULT_TTYD_PATH.to_string()
}

/// Probe the dyn slots for the guest-tools `ttyd`. Position-independent:
/// FC keeps `dyn/<i> == slot i`, VZ compacts resolved drives, and either
/// way at most one mount carries a top-level `ttyd`.
fn find_guest_tools_ttyd() -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(DYN_MOUNT_ROOT).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("ttyd");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

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
    /// Issue #569: registers `child`'s pid with the reaper's tracked-pid
    /// set for as long as this handle lives — untracks on drop (handle
    /// replaced on respawn, or torn down in tests), so agentd's init-style
    /// zombie reaper never races this module's own `try_wait()`/`kill()`.
    _tracked: crate::reaper::TrackedChild,
}

async fn state() -> &'static Mutex<Option<ShellHandle>> {
    SHELL.get_or_init(|| async { Mutex::new(None) }).await
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
/// first call this spawns the binary [`resolve_ttyd_bin`] finds
/// (`ENGRAM_TTYD_BIN` → the guest-tools bundle mount → the legacy
/// image-baked path); on subsequent calls it checks the prior handle
/// is still alive and the port still accepts — restarting only on
/// failure.
///
/// Important: the legacy bake's init script also starts ttyd in
/// the background at VM boot, so on a freshly-restored warm VM the
/// port is likely ALREADY accepting before agentd ever sees a
/// StartShell. In that case we must NOT try to spawn — `ttyd -p
/// 7681 ...` would fail with `EADDRINUSE`. So before falling into
/// the spawn path we probe the port; an existing listener (whether
/// from init or from a previous StartShell on this same agentd) is
/// reported as `spawned = false`.
/// `session_env` is the durable session environment (image `[env]` +
/// secrets + session id) the host carried in on `SpawnHarness`. It's
/// applied to ttyd so the interactive shell — and the bash it execs on
/// connect — start in the same environment the harness and `/exec` see
/// (sccache wrapper, GCS creds, toolchain PATH, …). Only the fresh-spawn
/// path uses it; a pre-existing/re-entry ttyd already carries it from its
/// own spawn.
pub async fn start_shell(
    port: u16,
    session_env: HashMap<String, String>,
) -> io::Result<ShellOutcome> {
    let mut guard = state().await.lock().await;

    // Path 0: somebody already bound the port — typically the bake's
    // init script's `ttyd -W -p 7681 bash &`. Acknowledge it and
    // return without touching anything. We don't take ownership of
    // a child we didn't spawn; the bake's ttyd has its lifetime tied
    // to PID 1.
    if probe_ready(port).await.is_ok() {
        return Ok(ShellOutcome {
            port,
            spawned: false,
        });
    }

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

    let bin = resolve_ttyd_bin();
    let shell_bin = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    tracing::info!(%bin, %shell_bin, port, "spawning ttyd");

    let mut cmd = Command::new(&bin);
    cmd
        // -W = read-write terminal (default is read-only).
        // -p <port> = bind port.
        // Final positional arg = command to exec on connect.
        .args(["-W", "-p", &port.to_string(), &shell_bin])
        // The session env (sccache, secrets, toolchain PATH) on top of
        // agentd's inherited boot env; the bash ttyd execs on connect
        // inherits it, then layers /root/.bashrc.
        .envs(&session_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The agent itself can exit cleanly; ttyd shouldn't survive
        // it. kill_on_drop ensures the Child handle's drop sends
        // SIGKILL.
        .kill_on_drop(true);
    // Issue #569: `spawn_tracked` registers the pid with the reaper's
    // tracked-pid set atomically with the spawn — from this point on the
    // reaper must never touch this pid; `kill_on_drop` + this module's own
    // `try_wait()`/`kill()`+`wait()` own its lifecycle exclusively.
    let child = crate::reaper::spawn_tracked(&mut cmd)
        .map_err(|e| io::Error::new(e.kind(), format!("spawn ttyd ({bin}): {e}")))?;

    // A just-spawned `Child` always has a live pid (tokio only clears it
    // once something has waited the child to completion). `TrackedChild::new`
    // re-asserts the (already-set) registration and gives us untrack-on-drop.
    let tracked =
        crate::reaper::TrackedChild::new(child.id().expect("freshly spawned child has a pid"));
    *guard = Some(ShellHandle {
        child,
        port,
        _tracked: tracked,
    });

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
    let deadline = crate::time_source::metrics_now() + READY_DEADLINE;
    let mut backoff = READY_PROBE_START;
    let mut last_err: Option<io::Error> = None;
    while crate::time_source::metrics_now() < deadline {
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
