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
use std::time::Duration;

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

/// Chromium's `--remote-debugging-port` (headful chromium in the browser
/// bundle). Overridable per session via `ENGRAM_BROWSER_CDP_PORT` in the
/// durable session env — the same knob a differently-configured launcher
/// would use, so the probe never has to guess.
const DEFAULT_CDP_PORT: u16 = 9222;

/// Poll interval for the CDP liveness check (issue #569), shared by both the
/// fast/re-probe path and the fresh-spawn background watch (see
/// [`probe_cdp`]).
const CDP_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Budget for the fresh-spawn CDP watch: a detached background task (see
/// `start_browser`) polls for up to this long and `tracing::warn!`s in
/// agentd if chrome's CDP debug port never binds. Not on `start_browser`'s
/// response path — a fresh spawn's cold start on a 2-vCPU FC microVM can lag
/// well behind x11vnc's bind, so gating the RPC reply on this would make a
/// merely-slow-but-healthy start look like a failure. Mirrors the launcher's
/// own `--wait-cdp` budget for parity.
const CDP_PROBE_BUDGET_BACKGROUND: Duration = Duration::from_secs(20);
/// Budget for the FAST-PATH CDP check (stack already up, this call is only
/// re-probing). A wedged chrome behind a healthy x11vnc is exactly the #569
/// state, so we still check every call — but a short budget, so a repeat
/// `StartBrowser` (polled routinely while a session is open) stays snappy
/// instead of paying the full background-watch budget.
const CDP_PROBE_BUDGET_FAST: Duration = Duration::from_secs(1);

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
///
/// `cdp_warning` is set when x11vnc (what [`start_browser`] actually gates
/// readiness on) came up but chromium's CDP debug port never answered within
/// budget — issue #569: a dead-forever/crash-looping chrome behind a healthy
/// x11vnc used to report success unconditionally. Never fails the RPC; purely
/// diagnostic. Only ever populated on the fast/re-probe path (the stack was
/// already up): a fresh spawn always returns `None` here, because its CDP
/// liveness is watched by a detached background task instead (see
/// `start_browser`) — a cold start's CDP lag is expected, so probing it
/// inline would make an ordinary slow-but-healthy launch look like a
/// failure. Persistent chrome death is still wire-visible regardless: every
/// subsequent `StartBrowser` call (each VNC WebSocket open triggers one)
/// takes the fast path and carries its own warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserOutcome {
    pub port: u16,
    pub spawned: bool,
    pub cdp_warning: Option<String>,
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
    let serialize = start_lock().await.lock().await;

    // Path 0: x11vnc is already serving on `port` — the stack is up (this
    // agentd on a prior call, a restore, or the agent's `playwright-cli` via
    // `engram-browser --ensure`). Don't re-trigger; report `spawned = false`.
    match probe_ready(port).await {
        Ok(()) => {
            // Issue #569: x11vnc being up doesn't mean chrome is — check CDP
            // too, even on this fast/re-probe path (a wedged chrome behind a
            // healthy x11vnc is exactly the failure mode), but with a short
            // budget so a routine repeat StartBrowser call stays snappy.
            // Drop the serialize guard first: the probe is a read-only TCP
            // dial, and `start_lock` only needs to serialize launcher
            // subprocess spawns — holding it across every routine re-probe
            // just adds needless latency.
            drop(serialize);
            let cdp_warning = probe_cdp(&session_env, CDP_PROBE_BUDGET_FAST).await;
            if let Some(warning) = &cdp_warning {
                tracing::warn!(port, %warning, "start_browser: chromium CDP check failed");
            }
            return Ok(BrowserOutcome {
                port,
                spawned: false,
                cdp_warning,
            });
        }
        Err(probe_err) => {
            // The probe failed — but a snapshot/restore can resurrect a
            // WEDGED stack: x11vnc's listen socket survives the freeze and
            // keeps ACCEPTING connections, yet never (re-)sends the RFB
            // banner, so `probe_ready`'s banner read times out (issue #567).
            // The launcher's `--ensure` can't see this on its own: its
            // `stack_up()` check is a bare TCP connect, which a wedged
            // listener still passes, so `--ensure` concludes "already up" and
            // no-ops — the wedged stack is never replaced and every future
            // `StartBrowser` call fails the same way until the VM is
            // recreated. Force-stop whatever is registered in the pidfile
            // BEFORE re-`--ensure`ing, so a wedged-but-accepting stack is
            // actually killed and a fresh one takes its place.
            //
            // `stop_browser_locked`, not `stop_browser`: `start_lock()` is
            // already held above and the mutex isn't reentrant. This is
            // best-effort — a cold start (no pidfile yet) is already a no-op
            // `Ok(())`, and even a failed reap just falls through to
            // `--ensure` below, where a genuine failure surfaces anyway.
            tracing::info!(
                %probe_err,
                port,
                "browser probe failed; force-stopping any recorded stack before re-ensuring (#567)"
            );
            if let Err(e) = stop_browser_locked().await {
                tracing::warn!(
                    error = %e,
                    "force-stop before re-ensure failed; proceeding to --ensure anyway"
                );
            }
        }
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

    let mut cmd = Command::new(&bin);
    cmd.arg("--ensure")
        // The browser renders untrusted pages, so hand it only the non-secret
        // allowlist (`browser_env`), never the full `session_env`. Set the VNC
        // port *after* so a stray ENGRAM_BROWSER_VNC_PORT can't shadow it.
        .envs(browser_env(&session_env))
        .env("ENGRAM_BROWSER_VNC_PORT", port.to_string())
        .stdin(Stdio::null())
        // `.output()` implied `Stdio::piped()` for stdout/stderr; `.spawn()`
        // does not, so set them explicitly — `wait_with_output` below still
        // needs to capture both to build the same error message on failure.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Issue #569: this short-lived launcher subprocess went completely
    // unwatched by the reaper's tracked registry (unlike every other spawn
    // site in this crate) — a launcher that raced to exit before this
    // function's `.await` on it resumed was exactly as reapable-out-from-under-us
    // as the /exec or ttyd children. `spawn_tracked` closes that gap;
    // untrack once `wait_with_output` has consumed the exit status below.
    let child = crate::reaper::spawn_tracked(&mut cmd).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("spawn browser launcher ({bin} --ensure): {e}"),
        )
    })?;
    let launcher_pid = child.id();
    let out = child.wait_with_output().await.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("run browser launcher ({bin} --ensure): {e}"),
        )
    })?;
    if let Some(pid) = launcher_pid {
        crate::reaper::untrack(pid);
    }
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "engram-browser --ensure failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    wait_until_ready(port).await?;

    // Issue #569: x11vnc coming up is NOT proof chrome is alive — chrome can
    // be dead/crash-looping forever behind a healthy x11vnc. But a fresh
    // spawn's CDP lag is *expected* (chrome's cold start on a 2-vCPU FC
    // microVM measured slow), so probing inline here and warning at a
    // short-ish budget would just be a false-positive machine on every
    // ordinary session start. Don't hold up this RPC's reply on it: hand off
    // to a detached background task with a generous budget
    // ([`CDP_PROBE_BUDGET_BACKGROUND`], `--wait-cdp` parity) that
    // `tracing::warn!`s in agentd if CDP genuinely never binds. Persistent
    // chrome death is still wire-visible regardless — every subsequent
    // `StartBrowser` (each VNC WebSocket open triggers one) takes the fast
    // path above, which probes CDP on every call.
    let watch_env = session_env.clone();
    tokio::spawn(async move {
        if let Some(warning) = probe_cdp(&watch_env, CDP_PROBE_BUDGET_BACKGROUND).await {
            tracing::warn!(
                port,
                %warning,
                "start_browser: chromium CDP check failed (background watch)",
            );
        }
    });

    Ok(BrowserOutcome {
        port,
        spawned: true,
        cdp_warning: None,
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
    stop_browser_locked().await
}

/// Body of [`stop_browser`], for callers that already hold `start_lock()`.
///
/// `start_browser` (issue #567) calls this directly instead of `stop_browser`:
/// it force-stops a wedged stack from *inside* its own critical section, and
/// `start_lock()`'s `tokio::sync::Mutex` is not reentrant — going through the
/// public `stop_browser` there would deadlock the task against itself.
async fn stop_browser_locked() -> io::Result<()> {
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
/// exits the group reparents to agentd (pid 1); we hold no `Child` to wait
/// on, so [`crate::reaper`] (issue #569) is what actually collects the
/// corpses once SIGKILL lands. ESRCH (group already gone) is fine.
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

/// Resolve chromium's CDP port for this session: `ENGRAM_BROWSER_CDP_PORT`
/// from the (unscrubbed) durable session env if present, else
/// [`DEFAULT_CDP_PORT`]. An unparsable override falls back to the default
/// rather than erroring — this only gates a diagnostic warning, never the
/// RPC itself.
fn cdp_port(session_env: &HashMap<String, String>) -> u16 {
    session_env
        .get("ENGRAM_BROWSER_CDP_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CDP_PORT)
}

/// One `GET /json/version` attempt against chromium's CDP debug port,
/// hand-rolled over a raw `TcpStream` — this crate is guest-side and
/// deliberately carries no HTTP client dependency. Only the status line
/// matters (the body is a JSON blob naming the browser/protocol version we
/// don't need); reading up to 32 bytes is comfortably enough to see
/// `"HTTP/1.1 200"` land in one read on loopback. Returns `false` on any
/// connect/write/read failure or a non-200 status — this is a liveness
/// probe, not a diagnostic surface in its own right.
async fn probe_cdp_once(port: u16, budget: Duration) -> bool {
    let attempt = async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream
            .write_all(
                b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .ok()?;
        let mut buf = [0u8; 32];
        let mut filled = 0usize;
        while filled < 12 && filled < buf.len() {
            let n = stream.read(&mut buf[filled..]).await.ok()?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let status = &buf[..filled];
        Some(status.starts_with(b"HTTP/1.1 200") || status.starts_with(b"HTTP/1.0 200"))
    };
    matches!(timeout(budget, attempt).await, Ok(Some(true)))
}

/// CDP liveness check (issue #569): poll every [`CDP_PROBE_INTERVAL`] for up
/// to `budget`. `None` = CDP answered in time; `Some` = it never did, worded
/// as a warning the caller surfaces without failing the RPC (x11vnc — what
/// actually gates `start_browser`'s success — is up either way, so the
/// human-facing VNC tab still works regardless of chrome's state).
///
/// Shared by both call sites in [`start_browser`], which differ only in
/// `budget` and in what they do with the result: the fast/re-probe path (a
/// short [`CDP_PROBE_BUDGET_FAST`], result goes straight into the wire
/// `cdp_warning`) and the fresh-spawn background watch (a generous
/// [`CDP_PROBE_BUDGET_BACKGROUND`], run off the RPC's response path — see
/// `start_browser` for why). The warning text deliberately doesn't hardcode
/// which of the two budgets was in play; it just reports the one it was
/// given.
async fn probe_cdp(session_env: &HashMap<String, String>, budget: Duration) -> Option<String> {
    let cdp = cdp_port(session_env);
    let deadline = crate::time_source::metrics_now() + budget;
    loop {
        if probe_cdp_once(cdp, CDP_PROBE_INTERVAL).await {
            return None;
        }
        if crate::time_source::metrics_now() >= deadline {
            break;
        }
        sleep(CDP_PROBE_INTERVAL).await;
    }
    Some(format!(
        "chromium CDP (:{cdp}) not responding {secs}s after x11vnc came up — chrome may be \
         dead or still starting; see /tmp/engram-browser.chrome.log in the guest",
        secs = budget.as_secs(),
    ))
}

/// Reset the browser stack between tests — `cargo test` shares the `OnceCell`
/// and the pidfile across cases in one process, so a test explicitly reaps.
#[cfg(test)]
pub async fn shutdown_for_tests() -> io::Result<()> {
    stop_browser().await
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a
    // decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]

    // Only the Linux-gated spawn/wedge tests read the wall clock directly.
    #[cfg(target_os = "linux")]
    use std::time::Instant;

    use super::*;

    /// Guards the process-global `ENGRAM_BROWSER_BIN` / `ENGRAM_BROWSER_PIDFILE`
    /// env vars that the `start_browser` tests mutate. `cargo nextest` runs
    /// each test in its own process, so this is a no-op there — but a bare
    /// `cargo test -p engram-agentd` (and `just test-linux`, which shells out
    /// to exactly that inside the container) runs every test as a thread in
    /// ONE process, and two browser tests racing on the same env vars would
    /// cross-contaminate each other's launcher/pidfile. A `tokio` mutex, not
    /// `std::sync` — the guard is held across the tests' awaits, which a std
    /// `MutexGuard` must never be (clippy `await_holding_lock`); and tokio
    /// mutexes don't poison, so one test's panic can't wedge the tests that
    /// follow (the guard just drops on unwind). Linux-gated like its only
    /// users — on macOS both tests vanish and the lock would be dead code.
    #[cfg(target_os = "linux")]
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Fake chromium CDP endpoint: a loop-accepting HTTP listener that
    /// answers anything with `HTTP/1.1 200` — enough for `probe_cdp_once`'s
    /// status-line check. Returns the bound port; the accept thread lives for
    /// the rest of the test process (cheap, and each test binds its own).
    fn spawn_fake_cdp() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf); // consume the request head
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
                );
            }
        });
        port
    }

    /// `session_env` carrying an `ENGRAM_BROWSER_CDP_PORT` override — how the
    /// tests point the #569 CDP probe at [`spawn_fake_cdp`] (or at a dead
    /// port), instead of the default :9222 nothing in a test binds.
    fn cdp_env(cdp_port: u16) -> HashMap<String, String> {
        HashMap::from([("ENGRAM_BROWSER_CDP_PORT".to_string(), cdp_port.to_string())])
    }

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

        let _env_guard = ENV_LOCK.lock().await;

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

        // A live fake CDP endpoint so the #569 chromium-liveness probe
        // passes: both calls should come back warning-free (and without
        // burning the probe's timeout budget in this test).
        let cdp = spawn_fake_cdp();

        let out = start_browser(port, cdp_env(cdp)).await;

        // Second call should observe the stack already up (spawned=false) — but
        // only attempt it if the first succeeded.
        let again = if out.is_ok() {
            Some(start_browser(port, cdp_env(cdp)).await)
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
        assert_eq!(
            out.cdp_warning, None,
            "CDP answered (fake endpoint) — the fresh-spawn path must not warn"
        );

        let again = again.unwrap().expect("re-probe should succeed");
        assert_eq!(again.port, port);
        assert!(
            !again.spawned,
            "second call should see the stack already up (spawned = false)"
        );
        assert_eq!(
            again.cdp_warning, None,
            "CDP answered (fake endpoint) — the fast/re-probe path must not warn"
        );

        assert!(
            reaped,
            "stop_browser should have killpg'd the pidfile's group; port still accepts"
        );
    }

    /// Reproduces issue #567 root cause #2: after a snapshot/restore, x11vnc
    /// can come back ACCEPTING TCP but never again serving the RFB banner (its
    /// listen socket survived; its RFB service loop did not). The real
    /// launcher's `--ensure` guards on a bare connect (`stack_up()` in
    /// `deploy/bundles/browser/bin/engram-browser`), which a wedged listener
    /// still passes, so `--ensure` concludes "already up" and no-ops — the
    /// wedged stack is never replaced and `start_browser` fails the same way
    /// forever. This test plants exactly that wedge, then asserts
    /// `start_browser` recovers by force-stopping the recorded pgid before
    /// re-`--ensure`ing (the Half-A fix in this file), rather than trusting
    /// the launcher to notice on its own.
    ///
    /// The fake launcher below deliberately mirrors TODAY'S dumb bare-connect
    /// semantics (same `up()` check as the real launcher and as the sibling
    /// test's fake launcher) — this test must only go green because agentd
    /// force-stopped the wedge, not because the launcher got smarter.
    ///
    /// Linux-only for the same reasons as
    /// [`start_browser_spawns_launcher_and_probes_port`]: `terminate_pgid` is
    /// a `nix` `killpg`, a no-op on the macOS cross-build.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn start_browser_replaces_wedged_stack() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = ENV_LOCK.lock().await;

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

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("browser.pgid");
        let pidfile_disp = pidfile.display();

        // --- Plant a WEDGED stack directly (no launcher involved yet): a
        // detached listener that binds `port`, accepts in a loop, and NEVER
        // writes anything — modeling x11vnc surviving a restore with its
        // listen socket intact but its RFB service loop dead (#567).
        //
        // DOUBLE-forked so the listener ends up a grandchild of this helper
        // process, never a child of the test binary itself: the helper forks
        // `pid1`, `pid1` forks `pid2` (which calls `setsid` — its own pid
        // becomes its own pgid, so a pgid-targeted kill hits exactly it) and
        // then `pid1` exits immediately, so `pid2` reparents to pid 1 right
        // away. That matters for the death check below: a direct child of the
        // test process would sit as OUR zombie until we `wait()` it, whereas
        // a reparented orphan is either collected by `crate::reaper` (in-guest,
        // where it's actually spawned) or parked as a pid-1 zombie (this test
        // binary, which never spawns the reaper task) — see the reap-model
        // note there.
        let plant = format!(
            r#"#!/usr/bin/env python3
import os, sys, socket
PORT = {port}
PIDFILE = "{pidfile_disp}"

pid1 = os.fork()
if pid1 == 0:
    pid2 = os.fork()
    if pid2 == 0:
        os.setsid()  # own session + pgid == own pid
        os.close(0); os.close(1); os.close(2)
        s = socket.socket()
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", PORT))
        s.listen(16)
        conns = []
        while True:
            try:
                c, _ = s.accept()
                conns.append(c)   # accept but NEVER write — the wedge (#567)
            except Exception:
                pass
    else:
        with open(PIDFILE, "w") as f:
            f.write(str(pid2))
        os._exit(0)   # exit now so pid2 reparents to init immediately
else:
    os.waitpid(pid1, 0)
    sys.exit(0)
"#
        );
        let plant_script = dir.path().join("plant_wedge.py");
        std::fs::write(&plant_script, plant).unwrap();
        std::fs::set_permissions(&plant_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plant_out = std::process::Command::new("python3")
            .arg(&plant_script)
            .output()
            .expect("plant-wedge helper failed to run");
        assert!(
            plant_out.status.success(),
            "plant-wedge helper exited non-zero: {}",
            String::from_utf8_lossy(&plant_out.stderr)
        );

        // Wait until the wedged port actually accepts before proceeding
        // (bind() happens inside the grandchild, just after this helper
        // process returns to us).
        let bind_deadline = Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            assert!(
                Instant::now() < bind_deadline,
                "wedged listener never came up"
            );
            sleep(Duration::from_millis(20)).await;
        }

        let old_pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // --- Fake launcher: mirrors TODAY'S dumb `--ensure` semantics (a
        // bare connect — identical to `stack_up()` in the real launcher and
        // to the sibling test's fake launcher). If the port already accepts
        // it's a no-op, exactly today's bug; only if agentd force-stopped the
        // wedge first will this `up()` check see the port down and spawn a
        // fresh, properly-banner-serving listener.
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
    os.setsid()
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
        sys.exit(0)                   # bare-connect no-op: TODAY's bug
    pid = os.fork()
    if pid == 0:
        os.close(0); os.close(1); os.close(2)
        serve(); os._exit(0)
    with open(PIDFILE, "w") as f:
        f.write(str(pid))
    for _ in range(200):
        if up():
            sys.exit(0)
        time.sleep(0.05)
    sys.exit(1)
sys.exit(0)
"#
        );
        let launcher = dir.path().join("engram-browser");
        std::fs::write(&launcher, script).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Scope both env vars so we don't pollute sibling tests.
        let prev_bin = std::env::var("ENGRAM_BROWSER_BIN").ok();
        let prev_pid = std::env::var("ENGRAM_BROWSER_PIDFILE").ok();
        std::env::set_var("ENGRAM_BROWSER_BIN", &launcher);
        std::env::set_var("ENGRAM_BROWSER_PIDFILE", &pidfile);

        // Live fake CDP so the #569 chromium-liveness probe answers instantly
        // — this test times the wedge-recovery path and must not absorb the
        // probe's full timeout budget into `elapsed`.
        let cdp = spawn_fake_cdp();

        let started = Instant::now();
        let out = start_browser(port, cdp_env(cdp)).await;
        let elapsed = started.elapsed();

        // The force-stop happens INSIDE `start_browser` itself, well before
        // it returns, so the original wedged pid should already be dead —
        // poll briefly to absorb kill+reap latency (`terminate_pgid`'s own
        // SIGTERM-then-SIGKILL grace sleep). "Dead" is reap-model aware: the
        // detached listener reparented to pid 1, and what collects its
        // zombie depends on where this test runs. In-guest, agentd is pid 1
        // AND runs the `crate::reaper` task (issue #569), so the corpse
        // vanishes from /proc entirely — but the canonical `just test-linux`
        // lane runs `bash -c "cargo test …"` in a container, and bash
        // exec-optimizes a lone simple command: pid 1 is *cargo*, which
        // spawns no reaper of its own, so the killed listener parks as a
        // zombie (`/proc/<pid>` persists in state `Z`) forever.
        // Gone-or-zombie both prove the killpg landed; a still-wedged
        // listener would show S/R.
        fn wedge_pid_dead(pid: i32) -> bool {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => true, // gone entirely (a real reaper collected it)
                // The state field follows the parenthesized comm — parse
                // after the LAST ')' (comm may itself contain parens).
                Ok(stat) => stat
                    .rsplit(')')
                    .next()
                    .map(|rest| rest.trim_start().starts_with('Z'))
                    .unwrap_or(false),
            }
        }
        let mut old_pid_dead = false;
        let death_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < death_deadline {
            if wedge_pid_dead(old_pid) {
                old_pid_dead = true;
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }

        // A fresh connect should now see the REAL RFB banner from the
        // replacement stack.
        let banner_ok = probe_ready(port).await.is_ok();

        // Tear down via the pidfile-reap path.
        let _ = shutdown_for_tests().await;

        // Restore env before asserting so a panic can't leak into a sibling.
        match prev_bin {
            Some(p) => std::env::set_var("ENGRAM_BROWSER_BIN", p),
            None => std::env::remove_var("ENGRAM_BROWSER_BIN"),
        }
        match prev_pid {
            Some(p) => std::env::set_var("ENGRAM_BROWSER_PIDFILE", p),
            None => std::env::remove_var("ENGRAM_BROWSER_PIDFILE"),
        }

        let out =
            out.expect("start_browser should force-stop the wedge and replace it, not time out");
        assert_eq!(out.port, port);
        assert!(
            out.spawned,
            "should report spawned = true — the wedge forced a re-ensure"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "took {elapsed:?} — the buggy no-force-stop path takes ~22s+ \
             (2s initial probe + a full 20s wait_until_ready deadline)"
        );
        assert!(
            old_pid_dead,
            "original wedged pid {old_pid} should have been force-stopped before re-ensuring"
        );
        assert!(
            banner_ok,
            "fresh connect after start_browser should see the RFB banner"
        );
    }

    /// Issue #569: a healthy x11vnc with a dead chromium behind it — the
    /// exact prod state (chrome crash-looping while the VNC tab "works") that
    /// used to report unqualified success. `start_browser` takes the fast
    /// path (RFB banner answers on the first probe → no launcher involved),
    /// the CDP probe points at a dead port, and the call must still be `Ok`
    /// with `cdp_warning: Some(..)`.
    ///
    /// Not linux-gated: the fast path touches no launcher / pidfile /
    /// `killpg`, and doesn't mutate the `ENGRAM_BROWSER_*` process env
    /// (the CDP override rides `session_env`), so it also needs no
    /// `ENV_LOCK` and runs on the macOS lane.
    #[tokio::test]
    async fn start_browser_warns_when_cdp_never_answers() {
        // A fake x11vnc: loop-accept and serve the RFB banner so
        // `probe_ready` passes — the stack "is up".
        let vnc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = vnc.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::Write;
            for mut s in vnc.incoming().flatten() {
                let _ = s.write_all(b"RFB 003.008\n");
            }
        });

        // A CDP port with NOTHING behind it: bind-then-drop guarantees the
        // probe's connect is refused (dead chrome), not answered.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_cdp = dead.local_addr().unwrap().port();
        drop(dead);

        let out = start_browser(port, cdp_env(dead_cdp))
            .await
            .expect("a dead chrome must NOT fail the RPC — x11vnc is up");
        assert_eq!(out.port, port);
        assert!(
            !out.spawned,
            "fast path: the pre-existing listener sufficed"
        );
        let warning = out
            .cdp_warning
            .expect("dead CDP behind healthy x11vnc must produce a warning (#569)");
        assert!(
            warning.contains(&format!(":{dead_cdp}")),
            "warning should name the probed CDP port: {warning}"
        );
        assert!(
            warning.contains("engram-browser.chrome.log"),
            "warning should point at the in-guest chrome log: {warning}"
        );

        // Counter-case on the same stack: a LIVE CDP endpoint clears the
        // warning (the fast path probes on every call, so this exercises the
        // exact same code path with chrome "recovered").
        let live_cdp = spawn_fake_cdp();
        let out = start_browser(port, cdp_env(live_cdp))
            .await
            .expect("re-probe against the same live x11vnc");
        assert!(!out.spawned);
        assert_eq!(
            out.cdp_warning, None,
            "live CDP endpoint must clear the warning"
        );
    }

    /// Direct coverage of the merged [`probe_cdp`] (fix for the FIX 3+5
    /// mutex-hold/RPC-latency finding): both `start_browser` call sites
    /// (fast/re-probe, fresh-spawn background watch) now share this one
    /// function and differ only in the `budget` argument. Rather than
    /// waiting out the real [`CDP_PROBE_BUDGET_BACKGROUND`] (20s — exactly
    /// what the fresh-spawn path no longer blocks on), inject a tiny budget
    /// directly to prove the shared poll-and-warn behavior without a slow
    /// test.
    #[tokio::test]
    async fn probe_cdp_warns_after_its_injected_budget_elapses() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);

        let warning = probe_cdp(&cdp_env(port), Duration::from_millis(300))
            .await
            .expect("a dead port must never answer CDP");
        assert!(
            warning.contains(&format!(":{port}")),
            "warning should name the probed CDP port: {warning}"
        );
        assert!(
            warning.contains("engram-browser.chrome.log"),
            "warning should point at the in-guest chrome log: {warning}"
        );

        // Counter-case: a live endpoint answers before the budget elapses.
        let live_cdp = spawn_fake_cdp();
        assert_eq!(
            probe_cdp(&cdp_env(live_cdp), Duration::from_millis(300)).await,
            None,
            "a live CDP endpoint must not produce a warning regardless of budget"
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
