//! In-guest IDE (code-server) lifecycle (ADR 0081).
//!
//! Mirrors [`crate::browser`]'s hardened shape — probe-first fast path,
//! force-stop a wedged pidfile stack before re-ensure (issue #567), spawn
//! via the reaper, bounded readiness wait — but simpler: no CDP-style
//! secondary probe, and unlike the browser (untrusted page rendering →
//! env allowlist, ADR 0065 §7) the IDE is a trusted first-party surface
//! over the user's own workspace, equivalent to the shell: it inherits
//! the **full session env**, so its integrated terminal behaves
//! identically to the Shell tab.
//!
//! The launcher (`engram-ide`, shipped + PATH-symlinked by the `ide`
//! bundle) owns the process group: `--ensure` is idempotent, brings
//! code-server up detached via `setsid`, and records the pgid in a
//! pidfile. agentd holds no child handle; [`stop_ide`] reaps by the
//! pidfile (`killpg`), exactly like the browser (ADR 0065's shared-stack
//! reap model). Readiness is an HTTP probe against code-server's
//! `/healthz` on the loopback port — a real byte exchange, not a bare
//! connect, so a restore-wedged listener (#567) can't pass as ready.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

/// Default HTTP port the in-VM code-server binds — loopback (ADR 0066).
/// The orchestrator reaches it over the vsock port relay: agentd dials
/// `127.0.0.1:13337` in-guest and splices HTTP/WS bytes. 13337 is
/// code-server's documented example port, well away from common
/// dev-server ports so it doesn't collide with user workloads.
pub const DEFAULT_IDE_PORT: u16 = 13337;

/// Launcher symlinked onto PATH by the `ide` bundle (ADR 0081). Other
/// images that bake the launcher into a different prefix can override the
/// resolved path via `ENGRAM_IDE_BIN` in the agent environment.
const DEFAULT_IDE_LAUNCHER: &str = "engram-ide";

/// How long to wait for code-server to answer its first `/healthz` before
/// giving up. code-server (a Node server unpacking its extension host)
/// cold-starts slower than ttyd, so this deadline matches the browser's.
const READY_DEADLINE: Duration = Duration::from_secs(20);

/// Initial probe interval after spawn. Doubles up to [`READY_PROBE_MAX`] on
/// each miss.
const READY_PROBE_START: Duration = Duration::from_millis(100);
const READY_PROBE_MAX: Duration = Duration::from_millis(800);

/// Agentd-side serialization for `start_ide`: two concurrent `StartIde`
/// RPCs shouldn't both shell out to the launcher (the launcher's own flock +
/// idempotent `--ensure` make that safe regardless; this just avoids a
/// redundant subprocess). Lazy — dev/test agentd with no ide bundle never
/// allocates it. No handle is cached: the launcher records the process-group
/// id in a pidfile and [`stop_ide`] reaps by THAT.
static IDE: tokio::sync::OnceCell<Mutex<()>> = tokio::sync::OnceCell::const_new();

async fn start_lock() -> &'static Mutex<()> {
    IDE.get_or_init(|| async { Mutex::new(()) }).await
}

/// Pidfile the launcher records the process-group id into — must match the
/// launcher default (`deploy/bundles/ide/bin/engram-ide`). Override in
/// lockstep via `ENGRAM_IDE_PIDFILE`.
const DEFAULT_IDE_PIDFILE: &str = "/tmp/engram-ide.pgid";

fn ide_pidfile() -> String {
    std::env::var("ENGRAM_IDE_PIDFILE").unwrap_or_else(|_| DEFAULT_IDE_PIDFILE.to_string())
}

/// Outcome of a `start_ide` call. `spawned` distinguishes "the agent just
/// spawned the launcher" from "code-server was already up and we only
/// re-probed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdeOutcome {
    pub port: u16,
    pub spawned: bool,
}

/// Ensure code-server is running and answering `/healthz` on `port`.
///
/// On the first call this spawns the launcher at `ENGRAM_IDE_BIN` (or
/// `engram-ide` on PATH by default) in its own process group; on
/// subsequent calls it re-probes and respawns only if the server went
/// away. Either way the agent only returns once a fresh HTTP probe to the
/// loopback `port` answers, so the host can relay to code-server with
/// confidence right after this returns.
///
/// `session_env` is the durable session environment (image `[env]` +
/// secrets + session id) the host carried in on `SpawnHarness`, handed to
/// the launcher **wholesale** — the IDE is a trusted first-party surface
/// like the shell (ADR 0081), not allowlist-scrubbed like the browser.
/// `workdir` is the session working directory (the harness cwd agentd
/// recorded from the `SpawnHarness` frame), exported as
/// `ENGRAM_IDE_WORKDIR` so the launcher opens code-server on the
/// workspace. Only the fresh-spawn path uses either; a re-probe of an
/// already-running server leaves the existing process untouched.
pub async fn start_ide(
    port: u16,
    session_env: HashMap<String, String>,
    workdir: Option<String>,
) -> io::Result<IdeOutcome> {
    // Held for the whole call (unlike the browser there's no secondary
    // probe to release it early for): probe → force-stop → --ensure →
    // readiness wait all serialize against concurrent StartIde RPCs.
    let _serialize = start_lock().await.lock().await;

    // Path 0: code-server is already serving on `port` — up from a prior
    // call on this agentd or a launcher someone else ran. Don't re-trigger;
    // report `spawned = false`.
    match probe_ready(port).await {
        Ok(()) => {
            return Ok(IdeOutcome {
                port,
                spawned: false,
            });
        }
        Err(probe_err) => {
            // The probe failed — but a snapshot/restore can resurrect a
            // WEDGED server: the listen socket survives the freeze and keeps
            // ACCEPTING connections, yet never serves bytes, so the HTTP
            // probe times out (issue #567's browser lesson). The launcher's
            // `--ensure` guards on a bare connect, which a wedged listener
            // still passes, so it would conclude "already up" and no-op.
            // Force-stop whatever is registered in the pidfile BEFORE
            // re-`--ensure`ing so a fresh server takes its place.
            //
            // `stop_ide_locked`, not `stop_ide`: `start_lock()` is already
            // held above and the mutex isn't reentrant. Best-effort — a cold
            // start (no pidfile yet) is a no-op `Ok(())`, and even a failed
            // reap just falls through to `--ensure` below.
            tracing::info!(
                %probe_err,
                port,
                "ide probe failed; force-stopping any recorded stack before re-ensuring (#567)"
            );
            if let Err(e) = stop_ide_locked().await {
                tracing::warn!(
                    error = %e,
                    "force-stop before re-ensure failed; proceeding to --ensure anyway"
                );
            }
        }
    }

    // Bring code-server up via the launcher's idempotent `--ensure`: it
    // (re)probes and, if down, starts code-server DETACHED in its own
    // process group, records that pgid in the pidfile, and waits until the
    // port accepts before exiting. The server is NOT our child (it reparents
    // to agentd, pid 1); we only wait on the short-lived `--ensure` process,
    // then confirm the `/healthz` answer the host's relay dial relies on.
    let bin = std::env::var("ENGRAM_IDE_BIN").unwrap_or_else(|_| DEFAULT_IDE_LAUNCHER.to_string());
    tracing::info!(%bin, port, "ensuring ide (code-server)");

    let mut cmd = Command::new(&bin);
    cmd.arg("--ensure")
        // Full session env: the IDE's integrated terminal must see exactly
        // what the Shell tab sees (sccache wrapper, secrets, toolchain
        // PATH). Set the port + workdir *after* so a stray session var
        // can't shadow the values agentd resolved.
        .envs(&session_env)
        .env("ENGRAM_IDE_PORT", port.to_string())
        .stdin(Stdio::null())
        // `.spawn()` doesn't imply piped stdout/stderr; set them explicitly
        // so `wait_with_output` below can build the error message on failure.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(wd) = &workdir {
        cmd.env("ENGRAM_IDE_WORKDIR", wd);
    }
    // Track the short-lived launcher subprocess with the reaper (issue
    // #569): a launcher that races to exit before our `.await` on it
    // resumes is as reapable-out-from-under-us as any other child.
    // Untrack once `wait_with_output` has consumed the exit status.
    let child = crate::reaper::spawn_tracked(&mut cmd).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("spawn ide launcher ({bin} --ensure): {e}"),
        )
    })?;
    let launcher_pid = child.id();
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("run ide launcher ({bin} --ensure): {e}")))?;
    if let Some(pid) = launcher_pid {
        crate::reaper::untrack(pid);
    }
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "engram-ide --ensure failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    wait_until_ready(port).await?;

    Ok(IdeOutcome {
        port,
        spawned: true,
    })
}

/// Tear down the IDE by the pidfile the launcher recorded: `killpg` its
/// process group (SIGTERM, then a SIGKILL backstop) so code-server and its
/// helpers all reap together, then unlink the pidfile. Idempotent: no
/// pidfile → nothing registered → no-op.
pub async fn stop_ide() -> io::Result<()> {
    let _serialize = start_lock().await.lock().await;
    stop_ide_locked().await
}

/// Body of [`stop_ide`], for callers that already hold `start_lock()`.
///
/// `start_ide` (issue #567 pattern) calls this directly instead of
/// `stop_ide`: it force-stops a wedged server from *inside* its own
/// critical section, and `start_lock()`'s `tokio::sync::Mutex` is not
/// reentrant — going through the public `stop_ide` there would deadlock
/// the task against itself.
async fn stop_ide_locked() -> io::Result<()> {
    let pidfile = ide_pidfile();
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

/// SIGTERM then (after a grace) SIGKILL the whole process group the
/// launcher put code-server in via `setsid`. Once the launcher exits the
/// group reparents to agentd (pid 1); we hold no `Child` to wait on, so
/// [`crate::reaper`] is what actually collects the corpses once SIGKILL
/// lands. ESRCH (group already gone) is fine.
#[cfg(target_os = "linux")]
async fn terminate_pgid(pgid: i32) {
    let pid = nix::unistd::Pid::from_raw(pgid);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM);
    sleep(Duration::from_millis(300)).await;
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

/// `nix` is a Linux-only dep of this crate (the guest is always Linux); on
/// the macOS cross-compile — where `cfg(unix)` holds but `nix` is absent —
/// reaping is a no-op (there is no in-guest IDE there).
#[cfg(not(target_os = "linux"))]
async fn terminate_pgid(_pgid: i32) {}

/// Wait until code-server answers `/healthz` on `port` — that's "the IDE
/// is up".
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
            format!(
                "code-server did not answer /healthz on 127.0.0.1:{port} within {READY_DEADLINE:?}"
            ),
        )
    }))
}

/// How long to wait for the `/healthz` status line before treating the
/// listener as not-ready. code-server answers on accept, so this only has
/// to absorb scheduler jitter.
const HEALTHZ_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe that code-server is genuinely serving HTTP on `port` — not merely
/// that *something* accepted the TCP connection.
///
/// A bare connect is too weak: a restore-wedged listener (issue #567's
/// browser lesson) accepts yet serves nothing. So we issue a real
/// `GET /healthz` and require an HTTP status line back — hand-rolled over a
/// raw `TcpStream`, since this crate is guest-side and deliberately carries
/// no HTTP client dependency. Any HTTP/1.x response proves the server is
/// alive and speaking the protocol (code-server's `/healthz` answers 200;
/// we don't parse the body).
async fn probe_ready(port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await?;
    let mut buf = [0u8; 32];
    let mut filled = 0usize;
    let read_status = async {
        while filled < 12 && filled < buf.len() {
            let n = stream.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok::<_, io::Error>(())
    };
    match timeout(HEALTHZ_TIMEOUT, read_status).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no /healthz response from 127.0.0.1:{port} within {HEALTHZ_TIMEOUT:?}"),
            ));
        }
    }
    let _ = stream.shutdown().await;
    let status = &buf[..filled];
    if !(status.starts_with(b"HTTP/1.1 ") || status.starts_with(b"HTTP/1.0 ")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("listener on 127.0.0.1:{port} did not speak HTTP (got {status:?})"),
        ));
    }
    Ok(())
}

/// Reset the IDE between tests — `cargo test` shares the `OnceCell` and
/// the pidfile across cases in one process, so a test explicitly reaps.
#[cfg(test)]
pub async fn shutdown_for_tests() -> io::Result<()> {
    stop_ide().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the process-global `ENGRAM_IDE_BIN` / `ENGRAM_IDE_PIDFILE`
    /// env vars the launcher test mutates — same rationale as browser.rs's
    /// `ENV_LOCK` (nextest isolates per-process; a bare `cargo test` /
    /// `just test-linux` runs every test as a thread in ONE process).
    /// Linux-gated like its only user.
    #[cfg(target_os = "linux")]
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A fake code-server: loop-accept and answer any request with
    /// `HTTP/1.1 200` — enough for `probe_ready`'s status-line check.
    /// Returns the bound port; the accept thread lives for the rest of the
    /// test process (cheap, and each test binds its own).
    fn spawn_fake_healthz() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf); // consume the request head
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
                );
            }
        });
        port
    }

    /// Path 0: code-server already answers `/healthz` — `start_ide` must
    /// take the fast path (`spawned = false`) without touching the
    /// launcher/pidfile. Runs on every lane (no killpg, no env mutation).
    #[tokio::test]
    async fn start_ide_recognises_a_preexisting_server_without_spawning() {
        let port = spawn_fake_healthz();
        let out = start_ide(port, HashMap::new(), None)
            .await
            .expect("fast path against a live fake healthz must succeed");
        assert_eq!(out.port, port);
        assert!(
            !out.spawned,
            "must NOT spawn when a healthy server already owns the port"
        );
    }

    /// An accepting-but-mute listener must NOT pass the probe (the #567
    /// wedge shape): `probe_ready` requires a real HTTP status line, not a
    /// bare connect.
    #[tokio::test]
    async fn probe_rejects_a_listener_that_serves_no_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut conns = Vec::new();
            for s in listener.incoming().flatten() {
                conns.push(s); // accept but never write — the wedge
            }
        });
        let err = probe_ready(port)
            .await
            .expect_err("a mute listener must fail the /healthz probe");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Spawn a fake launcher (a python script speaking the real `--ensure`
    /// contract: no-op if the port accepts, else fork a DETACHED healthz
    /// server and record its pgid in the pidfile) and assert the full
    /// "spawn → bind → probe → ready" sequence, that the launcher received
    /// the full session env + `ENGRAM_IDE_PORT` + `ENGRAM_IDE_WORKDIR`,
    /// that a second call reports `spawned = false`, and that `stop_ide`
    /// reaps by the pidfile. Mirrors browser.rs's
    /// `start_browser_spawns_launcher_and_probes_port`.
    ///
    /// Linux-only: the reap goes through `terminate_pgid` (`nix` `killpg`,
    /// a no-op on the macOS cross-build) and the IDE is a Linux-guest
    /// feature anyway. Skips when python3 is absent (the canonical
    /// `just test-linux` rust:bookworm image has it).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn start_ide_spawns_launcher_and_probes_port() {
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
            eprintln!("SKIP: python3 not available; ide test relies on it for a fake launcher");
            return;
        }

        // Pick an unused localhost port by binding briefly then dropping.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let dir = tempfile::tempdir().unwrap();
        let env_dump = dir.path().join("launcher-env.json");
        let env_dump_disp = env_dump.display();

        // Fake launcher speaking the real `--ensure` contract: if the port
        // already accepts it's a no-op; otherwise it forks a DETACHED
        // (`setsid`) HTTP listener answering 200 to anything, and records
        // the detached process's pgid in the pidfile — exactly what
        // `stop_ide` reads to `killpg`. It also dumps the env keys agentd
        // is contracted to pass (full session env + port + workdir).
        let script = format!(
            r#"#!/usr/bin/env python3
import os, sys, socket, time
PORT = int(os.environ["ENGRAM_IDE_PORT"])
PIDFILE = os.environ["ENGRAM_IDE_PIDFILE"]

with open("{env_dump_disp}", "w") as f:
    for k in ["ENGRAM_IDE_PORT", "ENGRAM_IDE_WORKDIR", "FAKE_SESSION_SECRET"]:
        f.write(k + "=" + os.environ.get(k, "") + "\n")

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
            c, _ = s.accept()
            c.recv(512)
            c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            c.close()
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
        f.write(str(pid))             # register the detached server's pgid
    for _ in range(200):
        if up():
            sys.exit(0)
        time.sleep(0.05)
    sys.exit(1)
sys.exit(0)
"#
        );
        let launcher = dir.path().join("engram-ide");
        std::fs::write(&launcher, script).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pidfile = dir.path().join("ide.pgid");

        // Scope both env vars so we don't pollute sibling tests.
        let prev_bin = std::env::var("ENGRAM_IDE_BIN").ok();
        let prev_pid = std::env::var("ENGRAM_IDE_PIDFILE").ok();
        std::env::set_var("ENGRAM_IDE_BIN", &launcher);
        std::env::set_var("ENGRAM_IDE_PIDFILE", &pidfile);
        // Ensure no stale server from a prior run leaks in.
        let _ = shutdown_for_tests().await;

        // Full-env contract: a session var (a secret shape the browser
        // would have scrubbed) must reach the launcher intact.
        let session_env =
            HashMap::from([("FAKE_SESSION_SECRET".to_string(), "hunter2".to_string())]);
        let out = start_ide(port, session_env.clone(), Some("/workspace".into())).await;

        // Second call should observe the server already up (spawned=false) —
        // but only attempt it if the first succeeded.
        let again = if out.is_ok() {
            Some(start_ide(port, session_env, Some("/workspace".into())).await)
        } else {
            None
        };

        // Tear down via the pidfile-reap path, then confirm the detached
        // server is actually gone (the port stops accepting).
        let _ = shutdown_for_tests().await;
        sleep(Duration::from_millis(400)).await;
        let reaped = TcpStream::connect(("127.0.0.1", port)).await.is_err();

        // Restore env before asserting so a panic can't leak into a sibling.
        match prev_bin {
            Some(p) => std::env::set_var("ENGRAM_IDE_BIN", p),
            None => std::env::remove_var("ENGRAM_IDE_BIN"),
        }
        match prev_pid {
            Some(p) => std::env::set_var("ENGRAM_IDE_PIDFILE", p),
            None => std::env::remove_var("ENGRAM_IDE_PIDFILE"),
        }

        let out = out.expect("first start_ide should ensure + probe ready");
        assert_eq!(out.port, port);
        assert!(out.spawned, "first call should report spawned = true");

        let seen: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_dump)
            .unwrap()
            .lines()
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        assert_eq!(
            seen.get("ENGRAM_IDE_PORT").map(String::as_str),
            Some(port.to_string().as_str()),
            "launcher must receive the resolved port via ENGRAM_IDE_PORT"
        );
        assert_eq!(
            seen.get("ENGRAM_IDE_WORKDIR").map(String::as_str),
            Some("/workspace"),
            "launcher must receive the session workdir via ENGRAM_IDE_WORKDIR"
        );
        assert_eq!(
            seen.get("FAKE_SESSION_SECRET").map(String::as_str),
            Some("hunter2"),
            "full session env passthrough (ADR 0081): no allowlist scrub"
        );

        let again = again.unwrap().expect("re-probe should succeed");
        assert_eq!(again.port, port);
        assert!(
            !again.spawned,
            "second call should see the server already up (spawned = false)"
        );

        assert!(
            reaped,
            "stop_ide should have killpg'd the pidfile's group; port still accepts"
        );
    }
}
