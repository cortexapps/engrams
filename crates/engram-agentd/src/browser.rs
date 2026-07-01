//! In-guest browser (Xvfb + chromium + x11vnc) lifecycle (ADR 0065).
//!
//! ONE shared stack serves two audiences — the human over VNC and the agent
//! over CDP — so it can be brought up by EITHER: the human `EnsureBrowser` path
//! (this module's [`start_browser`]) or the agent's `playwright-cli` wrapper
//! (`engram-browser --ensure`). Both call the SAME idempotent launcher, which
//! brings the whole stack up detached in its own process group (`setsid`) and
//! records that pgid in a pidfile. So agentd holds no child handle;
//! [`stop_browser`] reaps by the pidfile (`killpg`), and the reap is correct
//! whoever spawned the stack (ADR 0065). `start_browser` still reads x11vnc's
//! RFB banner on `127.0.0.1:5900` before replying so the ADR-0066 relay's
//! guest-loopback dial finds x11vnc actually *serving* (not a bare listener).

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
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

/// Agentd-side serialization for `start_browser`: two concurrent `StartBrowser`
/// RPCs shouldn't both shell out to the launcher (the launcher's own flock +
/// idempotent `--ensure` make that safe regardless; this just avoids a redundant
/// subprocess). Lazy — dev/test agentd with no browser bundle never allocates
/// it. No stack handle is cached: the launcher records the stack's process-group
/// id in a pidfile and [`stop_browser`] reaps by THAT (ADR 0065), so the reap
/// works whether the human path or the agent's `playwright-cli` brought it up.
static BROWSER: tokio::sync::OnceCell<Mutex<()>> = tokio::sync::OnceCell::const_new();

async fn start_lock() -> &'static Mutex<()> {
    BROWSER.get_or_init(|| async { Mutex::new(()) }).await
}

/// Pidfile the launcher records the stack's process-group id into — must match
/// the launcher default (`deploy/bundles/browser/bin/engram-browser`). Override
/// in lockstep via `ENGRAM_BROWSER_PIDFILE`.
const DEFAULT_BROWSER_PIDFILE: &str = "/tmp/engram-browser.pgid";

fn browser_pidfile() -> String {
    std::env::var("ENGRAM_BROWSER_PIDFILE").unwrap_or_else(|_| DEFAULT_BROWSER_PIDFILE.to_string())
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
    let _serialize = start_lock().await.lock().await;

    // Path 0: x11vnc is already serving on `port` — the stack is up (this
    // agentd on a prior call, a restore, or the agent's `playwright-cli` via
    // `engram-browser --ensure`). Don't re-trigger; report `spawned = false`.
    if probe_ready(port).await.is_ok() {
        return Ok(BrowserOutcome {
            port,
            spawned: false,
        });
    }

    // Bring the stack up via the launcher's idempotent `--ensure`: it (re)probes
    // and, if down, brings the whole stack up DETACHED in its own process group,
    // records that pgid in the pidfile, and waits until x11vnc accepts before
    // exiting. So this is the SAME entrypoint the agent's `playwright-cli`
    // wrapper calls — one shared stack, and `stop_browser` reaps it by the
    // pidfile regardless of which audience triggered it (ADR 0065). The stack is
    // NOT our child (it reparents to agentd, pid 1); we only wait on the
    // short-lived `--ensure` process, then confirm the RFB banner the host's VNC
    // dial relies on.
    let bin = std::env::var("ENGRAM_BROWSER_BIN")
        .unwrap_or_else(|_| DEFAULT_BROWSER_LAUNCHER.to_string());
    tracing::info!(%bin, port, "ensuring browser stack");

    let out = Command::new(&bin)
        .arg("--ensure")
        // The browser renders untrusted pages, so hand it only the non-secret
        // allowlist (`browser_env`), never the full `session_env`. Set the VNC
        // port *after* so a stray ENGRAM_BROWSER_VNC_PORT can't shadow it.
        .envs(browser_env(&session_env))
        .env("ENGRAM_BROWSER_VNC_PORT", port.to_string())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("run browser launcher ({bin} --ensure): {e}"),
            )
        })?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "engram-browser --ensure failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    wait_until_ready(port).await?;

    Ok(BrowserOutcome {
        port,
        spawned: true,
    })
}

/// Tear down the browser stack by the pidfile the launcher recorded: `killpg`
/// its process group (SIGTERM, then a SIGKILL backstop) so Xvfb/openbox/
/// chromium/x11vnc all reap together, then unlink the pidfile. Reaps regardless
/// of who spawned the stack — the human `EnsureBrowser` path or the agent's
/// `playwright-cli` `--ensure` — since agentd holds no handle either way (ADR
/// 0065). Idempotent: no pidfile → nothing registered → no-op.
pub async fn stop_browser() -> io::Result<()> {
    let _serialize = start_lock().await.lock().await;
    let pidfile = browser_pidfile();
    let contents = match tokio::fs::read_to_string(&pidfile).await {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    if let Ok(pgid) = contents.trim().parse::<i32>() {
        terminate_pgid(pgid).await;
    }
    // Clear the registration so a later probe-miss doesn't reap a recycled pgid.
    let _ = tokio::fs::remove_file(&pidfile).await;
    Ok(())
}

/// SIGTERM then (after a grace) SIGKILL the stack's whole process group. The
/// launcher put every process — Xvfb/openbox/chromium/x11vnc — in this one
/// group via `setsid`, so this reaps the stack as a unit. Once their launcher
/// exits the group reparents to agentd (pid 1), whose init reaping collects the
/// corpses; we hold no `Child` to wait on. ESRCH (group already gone) is fine.
#[cfg(target_os = "linux")]
async fn terminate_pgid(pgid: i32) {
    let pid = nix::unistd::Pid::from_raw(pgid);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM);
    sleep(Duration::from_millis(300)).await;
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

/// `nix` is a Linux-only dep of this crate (the guest is always Linux); on the
/// macOS cross-compile — where `cfg(unix)` holds but `nix` is absent — reaping
/// is a no-op (there is no in-guest stack there).
#[cfg(not(target_os = "linux"))]
async fn terminate_pgid(_pgid: i32) {}

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

/// Reset the browser stack between tests — `cargo test` shares the `OnceCell`
/// and the pidfile across cases in one process, so a test explicitly reaps.
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
    ///
    /// Linux-only: the reap goes through `terminate_pgid`, which is a `nix`
    /// `killpg` (a no-op on the macOS cross-build, where `nix` is absent) — and
    /// the browser stack is a Linux-guest feature that never runs on macOS
    /// agentd anyway. The workspace macOS lane excludes it; the Linux lane runs it.
    #[cfg(target_os = "linux")]
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

        // Fake launcher speaking the real `--ensure` contract (ADR 0065): if the
        // port already accepts it's a no-op; otherwise it forks a DETACHED
        // (`setsid`) listener that replies with x11vnc's 12-byte RFB banner, and
        // records that detached process's pgid (== its pid after setsid) in the
        // pidfile — exactly what agentd's `stop_browser` reads to `killpg`.
        // Accepting in a loop lets both agentd's probe and the test's re-probe
        // connect. A python-shebang script keeps the source plain (no `-c`
        // escaping).
        let script = format!(
            r#"#!/usr/bin/env python3
import os, sys, socket, time
PORT = {port}
PIDFILE = os.environ["ENGRAM_BROWSER_PIDFILE"]

def up():
    try:
        socket.create_connection(("127.0.0.1", PORT), timeout=0.2).close()
        return True
    except OSError:
        return False

def serve():
    os.setsid()                       # detached: own session/group, pgid == pid
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", PORT))
    s.listen(16)
    while True:
        try:
            c, _ = s.accept(); c.sendall(b"RFB 003.008\n"); c.close()
        except Exception:
            pass

if sys.argv[1:2] == ["--ensure"]:
    if up():
        sys.exit(0)                   # already up: idempotent no-op
    pid = os.fork()
    if pid == 0:
        os.close(0); os.close(1); os.close(2)
        serve(); os._exit(0)
    with open(PIDFILE, "w") as f:
        f.write(str(pid))             # register the detached stack's pgid
    for _ in range(200):
        if up():
            sys.exit(0)
        time.sleep(0.05)
    sys.exit(1)
sys.exit(0)
"#
        );
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("engram-browser");
        std::fs::write(&launcher, script).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pidfile = dir.path().join("browser.pgid");

        // Scope both env vars so we don't pollute sibling tests.
        let prev_bin = std::env::var("ENGRAM_BROWSER_BIN").ok();
        let prev_pid = std::env::var("ENGRAM_BROWSER_PIDFILE").ok();
        std::env::set_var("ENGRAM_BROWSER_BIN", &launcher);
        std::env::set_var("ENGRAM_BROWSER_PIDFILE", &pidfile);
        // Ensure no stale stack from a prior run leaks in.
        let _ = shutdown_for_tests().await;

        let out = start_browser(port, HashMap::new()).await;

        // Second call should observe the stack already up (spawned=false) — but
        // only attempt it if the first succeeded.
        let again = if out.is_ok() {
            Some(start_browser(port, HashMap::new()).await)
        } else {
            None
        };

        // Tear down via the pidfile-reap path, then confirm the detached stack
        // is actually gone (the port stops accepting).
        let _ = shutdown_for_tests().await;
        sleep(Duration::from_millis(400)).await;
        let reaped = TcpStream::connect(("127.0.0.1", port)).await.is_err();

        // Restore env before asserting so a panic can't leak into a sibling.
        match prev_bin {
            Some(p) => std::env::set_var("ENGRAM_BROWSER_BIN", p),
            None => std::env::remove_var("ENGRAM_BROWSER_BIN"),
        }
        match prev_pid {
            Some(p) => std::env::set_var("ENGRAM_BROWSER_PIDFILE", p),
            None => std::env::remove_var("ENGRAM_BROWSER_PIDFILE"),
        }

        let out = out.expect("first start_browser should ensure + probe ready");
        assert_eq!(out.port, port);
        assert!(out.spawned, "first call should report spawned = true");

        let again = again.unwrap().expect("re-probe should succeed");
        assert_eq!(again.port, port);
        assert!(
            !again.spawned,
            "second call should see the stack already up (spawned = false)"
        );

        assert!(
            reaped,
            "stop_browser should have killpg'd the pidfile's group; port still accepts"
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
