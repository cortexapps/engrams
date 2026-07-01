//! In-guest browser (Xvfb + chromium + x11vnc) lifecycle (ADR 0065).
//!
//! Mirrors [`crate::shell`]: lazy spawn on the first
//! [`StartBrowser`][crate::proto::WireRequest::StartBrowser], an RFB-banner
//! probe to `127.0.0.1:5900` before replying so the ADR-0066 relay's
//! guest-loopback dial finds x11vnc actually *serving* RFB (not a bare listener), and
//! respawn only if the prior launcher exited. The launcher (`engram-browser`,
//! symlinked onto PATH by the `browser` bundle's activation — see ADR 0065
//! P0.1/P0.2) brings up the whole stack (Xvfb + openbox + chromium + x11vnc) in
//! its own process group via `setsid`, so [`stop_browser`] reaps it with a
//! single `killpg`. The launcher's stdout/stderr are drained to tracing (never
//! left on an undrained pipe) so chromium's continuous logging can't fill the
//! OS pipe buffer and block — and thereby freeze — the whole stack.
//!
//! Like the shell, the state is process-wide (one browser stack per VM) behind
//! a tokio `Mutex` so concurrent `StartBrowser` calls serialize on the spawn
//! decision rather than racing each other into two launcher processes.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

/// Default RFB port the in-VM x11vnc binds — loopback (ADR 0066). The
/// orchestrator reaches it over the vsock port relay (`PROXY_PORT_VSOCK_PORT`):
/// agentd dials `127.0.0.1:5900` in-guest and splices RFB bytes.
pub const DEFAULT_VNC_PORT: u16 = 5900;

/// Launcher symlinked onto PATH by the `browser` bundle (ADR 0065). Other
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

/// The environment a freshly-spawned browser stack is allowed to inherit.
///
/// The browser renders pages the human navigates to — untrusted code — so it
/// must never carry the session's secrets in its process environment: a
/// renderer compromise reads `/proc/self/environ`, and chromium spawns helpers
/// that inherit it (ADR 0065 §7). agentd is the only layer that holds
/// `session_env`, so the scrub lives here.
///
/// This is an **allowlist**, not a denylist. `session_env` is an opaque flat map
/// from the coordinator (image `[env]` + arbitrary secrets), so a denylist would
/// leak any *new* secret key by default. The browser needs nothing secret —
/// egress is transparent (host iptables REDIRECT; no `*_PROXY` var to forward)
/// and the egress-proxy CA is trusted at the OS level — so we keep only locale,
/// `PATH`, and the launcher's own `ENGRAM_BROWSER_*` knobs.
fn browser_env(session_env: &HashMap<String, String>) -> HashMap<String, String> {
    session_env
        .iter()
        .filter(|(k, _)| is_browser_safe_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Whether `key` is safe to hand the (untrusted-content) browser stack — see
/// [`browser_env`]. Conservative: anything not matched here is dropped.
fn is_browser_safe_key(key: &str) -> bool {
    // Non-secret keys the X/chromium stack legitimately uses.
    matches!(key, "PATH" | "LANG" | "LANGUAGE" | "TZ")
        // Locale categories: LC_ALL, LC_CTYPE, LC_TIME, …
        || key.starts_with("LC_")
        // Launcher knobs: geometry, homepage, display, vnc port, uid/gid.
        || key.starts_with("ENGRAM_BROWSER_")
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
/// session id) the host carried in on `SpawnHarness`. Unlike the shell, the
/// browser is **not** handed this wholesale — it renders untrusted pages, so
/// [`browser_env`] scrubs it to a non-secret allowlist before the launcher sees
/// it (ADR 0065 §7). Only the fresh-spawn path uses it; a re-probe of an
/// already-running stack leaves the existing process untouched.
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
    // wedged, or was asked to move ports, tear the whole old group down (see
    // `terminate_group` — `kill_on_drop` alone would orphan the chromium
    // respawn loop) and fall through to a fresh spawn.
    if let Some(mut handle) = guard.take() {
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
        terminate_group(&mut handle).await;
    }

    let bin = std::env::var("ENGRAM_BROWSER_BIN")
        .unwrap_or_else(|_| DEFAULT_BROWSER_LAUNCHER.to_string());
    tracing::info!(%bin, port, "spawning browser stack");

    let mut cmd = Command::new(&bin);
    // The browser renders untrusted pages, so it must NOT inherit the session's
    // secrets — hand it only the non-secret allowlist (`browser_env`), not the
    // full `session_env`. agentd's chosen VNC port is set *after* so a stray
    // ENGRAM_BROWSER_VNC_PORT carried in the env can't shadow it.
    cmd.envs(browser_env(&session_env))
        .env("ENGRAM_BROWSER_VNC_PORT", port.to_string())
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

    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn browser launcher ({bin}): {e}")))?;

    // Drain the launcher's stdout/stderr so chromium's continuous output can't
    // fill the 64 KiB OS pipe buffer and block — and thereby freeze — the whole
    // stack (Xvfb/openbox/chromium/x11vnc all inherit the launcher's fds). The
    // drain tasks self-terminate on EOF when stop_browser / kill_on_drop reaps
    // the group, so they need no separate shutdown.
    if let Some(out) = child.stdout.take() {
        drain_launcher_output(out, "stdout");
    }
    if let Some(err) = child.stderr.take() {
        drain_launcher_output(err, "stderr");
    }

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
        terminate_group(&mut handle).await;
    }
    Ok(())
}

/// SIGTERM the launcher's whole process group, then SIGKILL the leader as a
/// backstop, and reap it. Shared by `stop_browser` and `start_browser`'s
/// restart path: `kill_on_drop` alone only SIGKILLs the group *leader*, which
/// on a respawn would orphan the backgrounded children — Xvfb, openbox, and
/// especially the chromium respawn loop, which would otherwise keep relaunching
/// chrome forever. killpg reaps the whole stack as a unit.
async fn terminate_group(handle: &mut BrowserHandle) {
    // `nix` is a Linux-only dependency of this crate (the guest is always
    // Linux); gate the killpg on linux specifically rather than `unix` so the
    // macOS cross-compile — where `cfg(unix)` is true but `nix` is absent —
    // still builds. The SIGKILL path below covers the leader on every platform.
    #[cfg(target_os = "linux")]
    if let Some(pid) = handle.child.id() {
        // Negative pid == process group: the launcher is the group leader (via
        // process_group(0) at spawn). ESRCH (group already gone) is fine —
        // this is best-effort.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    let _ = handle.child.kill().await; // SIGKILL backstop on the leader
    let _ = handle.child.wait().await;
}

/// Wait until x11vnc is serving RFB on `port` — that's "the browser stack is
/// up". The CDP debug port (`:9222`) chromium exposes for the agent's
/// `connectOverCDP` (ADR 0065 §1a) is deliberately NOT gated here: chromium's
/// DevTools socket lags x11vnc, and the agent/orchestrator poll `/json/version`
/// themselves, so gating `start_browser` on it just makes the spawn flaky
/// (chromium's cold-start on FC can exceed the ready deadline).
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
            format!("x11vnc did not serve RFB on 127.0.0.1:{port} within {READY_DEADLINE:?}"),
        )
    }))
}

/// Length of the RFB ProtocolVersion banner ("RFB 003.008\n") x11vnc sends the
/// instant a viewer connects, before reading anything (RFC 6143 §7.1.1).
const RFB_BANNER_LEN: usize = 12;
/// How long to wait for that banner before treating the listener as not-ready.
/// x11vnc emits it on accept, so this only has to absorb scheduler jitter.
const RFB_BANNER_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe that x11vnc is genuinely serving RFB on `port` — not merely that
/// *something* accepted the TCP connection.
///
/// A bare connect is too weak: the original ADR 0065 break (the host dialing
/// the guest's routable IP while x11vnc bound loopback) and any future
/// "port accepts but no bytes flow" wedge both pass a connect yet serve
/// nothing — the user sees the VNC tab close with "no messages over the
/// endpoint". So we read x11vnc's opening RFB banner: a real byte exchange that
/// proves the server is alive and speaking the protocol. We don't complete the
/// handshake; closing after the banner is the same as any viewer that hangs up
/// mid-negotiation, which `-forever` x11vnc tolerates.
async fn probe_ready(port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut banner = [0u8; RFB_BANNER_LEN];
    match timeout(RFB_BANNER_TIMEOUT, stream.read_exact(&mut banner)).await {
        Ok(Ok(_)) => {}
        // Short read / connection reset before the full banner → not ready.
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no RFB banner from 127.0.0.1:{port} within {RFB_BANNER_TIMEOUT:?}"),
            ));
        }
    }
    let _ = stream.shutdown().await;
    if !banner.starts_with(b"RFB ") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("listener on 127.0.0.1:{port} did not speak RFB (banner {banner:?})"),
        ));
    }
    Ok(())
}

/// Forward a launcher pipe (stdout/stderr) to tracing, line by line, until EOF.
///
/// The reason this exists is the *read*, not the log: chromium writes to stderr
/// continuously, and if agentd left these pipes undrained the OS pipe buffer
/// would fill and block every writer that inherited the launcher's fds —
/// freezing the browser stack. Reading here empties the pipe; the `debug!` is a
/// bonus (even with that level disabled the read still drains). The task ends
/// on EOF — when the launcher group is reaped its write ends close — so it
/// needs no separate shutdown signal.
fn drain_launcher_output<R>(reader: R, stream: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    tracing::debug!(target: "engram_agentd::browser", stream, "{line}")
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(stream, error = %e, "browser log drain ended on read error");
                    break;
                }
            }
        }
    });
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
    /// port, accepts in a loop, and replies with x11vnc's RFB ProtocolVersion
    /// banner so `probe_ready`'s banner read succeeds) and assert the full
    /// "spawn → bind → probe → ready" sequence works, then that a second call
    /// sees the stack already up and reports `spawned = false`.
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

        // Fake launcher: bind the VNC port and accept connections forever,
        // replying with x11vnc's 12-byte RFB ProtocolVersion banner so
        // `probe_ready`'s banner read succeeds. Accepting in a loop lets both
        // agentd's internal probe AND the test's re-probe connect (a backlog-1
        // listen that never accepts would refuse the second connect). A
        // python-shebang script keeps the source as plain, correctly-indented
        // python — no inline `-c` escaping.
        let script = format!(
            r#"#!/usr/bin/env python3
import socket
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", {port}))
s.listen(16)
while True:
    try:
        c, _ = s.accept()
        c.sendall(b"RFB 003.008\n")
        c.close()
    except Exception:
        pass
"#
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

    #[test]
    fn browser_env_drops_secrets_keeps_allowlisted() {
        let mut env = HashMap::new();
        // Secrets / arbitrary session keys — every one must be dropped.
        env.insert("ENGRAM_FORGE_TOKEN".into(), "secret".into());
        env.insert("ENGRAM_UPLOAD_TOKEN".into(), "secret".into());
        env.insert("ANTHROPIC_API_KEY".into(), "sk-secret".into());
        env.insert("AWS_SECRET_ACCESS_KEY".into(), "secret".into());
        env.insert("RUSTC_WRAPPER".into(), "sccache".into());
        // Allowlisted, non-secret — must survive with values intact.
        env.insert("PATH".into(), "/usr/bin".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        env.insert("LC_CTYPE".into(), "C.UTF-8".into());
        env.insert("TZ".into(), "UTC".into());
        env.insert("ENGRAM_BROWSER_GEOMETRY".into(), "1440x1080x24".into());
        env.insert("ENGRAM_BROWSER_UID".into(), "9000".into());

        let scrubbed = browser_env(&env);

        for secret in [
            "ENGRAM_FORGE_TOKEN",
            "ENGRAM_UPLOAD_TOKEN",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "RUSTC_WRAPPER",
        ] {
            assert!(
                !scrubbed.contains_key(secret),
                "secret {secret} leaked into the browser env"
            );
        }
        assert_eq!(scrubbed.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(scrubbed.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(
            scrubbed.get("LC_CTYPE").map(String::as_str),
            Some("C.UTF-8")
        );
        assert_eq!(scrubbed.get("TZ").map(String::as_str), Some("UTC"));
        assert_eq!(
            scrubbed.get("ENGRAM_BROWSER_GEOMETRY").map(String::as_str),
            Some("1440x1080x24")
        );
        assert_eq!(
            scrubbed.get("ENGRAM_BROWSER_UID").map(String::as_str),
            Some("9000")
        );
        // Exactly the six allowlisted keys, nothing else.
        assert_eq!(scrubbed.len(), 6);
    }

    #[test]
    fn is_browser_safe_key_rejects_lookalikes() {
        // Prefix/exact discipline: a key that merely contains an allowed
        // substring must not slip through.
        assert!(!is_browser_safe_key("MYPATH"));
        assert!(!is_browser_safe_key("PATHX"));
        assert!(!is_browser_safe_key("ENGRAM_TOKEN")); // not the ENGRAM_BROWSER_ prefix
        assert!(!is_browser_safe_key("XLC_ALL")); // LC_ not at the start
        assert!(is_browser_safe_key("PATH"));
        assert!(is_browser_safe_key("LANGUAGE"));
        assert!(is_browser_safe_key("LC_ALL"));
        assert!(is_browser_safe_key("ENGRAM_BROWSER_HOMEPAGE"));
    }
}
